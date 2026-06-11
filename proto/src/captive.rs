//! Captive-portal protocol pieces for the onboarding hotspot.
//!
//! Sans-IO like `kosync`: a minimal DHCP server codec (enough to lease
//! addresses to a phone), a DNS catch-all responder (every name resolves
//! to the portal), and HTTP request parsing for the credential form. The
//! firmware's wifi task owns the sockets; host tests drive everything
//! here with byte slices.

// ------------------------------------------------------------------
// DHCP server
// ------------------------------------------------------------------

const DHCP_MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
const DHCP_DISCOVER: u8 = 1;
const DHCP_OFFER: u8 = 2;
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;
const DHCP_NAK: u8 = 6;
/// Fixed-field DHCP frame length before the options area.
const DHCP_OPTIONS_AT: usize = 240;
pub const DHCP_REPLY_LEN: usize = 300;
const LEASE_SECS: u32 = 3600;
const MAX_LEASES: usize = 4;

/// Hands out addresses from `portal_ip+1` upward on a /24. The pool is
/// tiny because onboarding serves exactly one phone, but a couple of
/// slots tolerate a second device wandering in.
pub struct DhcpServer {
    portal_ip: [u8; 4],
    leases: [Option<[u8; 6]>; MAX_LEASES],
}

impl DhcpServer {
    pub const fn new(portal_ip: [u8; 4]) -> Self {
        Self {
            portal_ip,
            leases: [None; MAX_LEASES],
        }
    }

    fn lease_ip(&self, slot: usize) -> [u8; 4] {
        let mut ip = self.portal_ip;
        ip[3] = ip[3].wrapping_add(1 + slot as u8);
        ip
    }

    fn slot_for(&mut self, mac: [u8; 6]) -> usize {
        if let Some(slot) = self.leases.iter().position(|lease| *lease == Some(mac)) {
            return slot;
        }
        if let Some(slot) = self.leases.iter().position(|lease| lease.is_none()) {
            self.leases[slot] = Some(mac);
            return slot;
        }
        // Pool exhausted: recycle slot 0. Onboarding traffic is one phone;
        // a stale lease losing its address is acceptable.
        self.leases[0] = Some(mac);
        0
    }

    /// Handles one received UDP payload from port 67. Returns the reply
    /// length when the packet warrants a broadcast reply from `out`.
    pub fn handle(&mut self, request: &[u8], out: &mut [u8]) -> Option<usize> {
        if request.len() < DHCP_OPTIONS_AT
            || request[0] != BOOTREQUEST
            || request[236..240] != DHCP_MAGIC
            || out.len() < DHCP_REPLY_LEN
        {
            return None;
        }
        let message_type = option_value(&request[DHCP_OPTIONS_AT..], 53)?
            .first()
            .copied()?;
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&request[28..34]);
        let slot = self.slot_for(mac);
        let your_ip = self.lease_ip(slot);

        let reply_type = match message_type {
            DHCP_DISCOVER => DHCP_OFFER,
            DHCP_REQUEST => {
                // A client requesting an address we did not offer gets a
                // NAK so it restarts discovery against this server.
                let requested = option_value(&request[DHCP_OPTIONS_AT..], 50)
                    .and_then(|value| value.get(..4))
                    .map(|value| value == your_ip)
                    // No option 50: renewing via ciaddr.
                    .unwrap_or(request[12..16] == your_ip);
                if requested {
                    DHCP_ACK
                } else {
                    DHCP_NAK
                }
            }
            _ => return None,
        };

        out[..DHCP_REPLY_LEN].fill(0);
        out[0] = BOOTREPLY;
        out[1] = request[1]; // htype
        out[2] = request[2]; // hlen
        out[4..8].copy_from_slice(&request[4..8]); // xid
        out[10..12].copy_from_slice(&request[10..12]); // flags
        if reply_type != DHCP_NAK {
            out[16..20].copy_from_slice(&your_ip); // yiaddr
            out[20..24].copy_from_slice(&self.portal_ip); // siaddr
        }
        out[28..44].copy_from_slice(&request[28..44]); // chaddr
        out[236..240].copy_from_slice(&DHCP_MAGIC);

        let mut at = DHCP_OPTIONS_AT;
        let mut push = |bytes: &[u8]| {
            out[at..at + bytes.len()].copy_from_slice(bytes);
            at += bytes.len();
        };
        push(&[53, 1, reply_type]);
        push(&[54, 4]);
        push(&self.portal_ip); // server identifier
        if reply_type != DHCP_NAK {
            push(&[51, 4]);
            push(&LEASE_SECS.to_be_bytes());
            push(&[1, 4, 255, 255, 255, 0]); // subnet mask
            push(&[3, 4]);
            push(&self.portal_ip); // router
            push(&[6, 4]);
            push(&self.portal_ip); // dns -> the captive responder
        }
        push(&[255]);
        Some(DHCP_REPLY_LEN)
    }
}

fn option_value(options: &[u8], wanted: u8) -> Option<&[u8]> {
    let mut at = 0;
    while at < options.len() {
        let code = options[at];
        match code {
            0 => at += 1,
            255 => return None,
            _ => {
                let len = *options.get(at + 1)? as usize;
                let value = options.get(at + 2..at + 2 + len)?;
                if code == wanted {
                    return Some(value);
                }
                at += 2 + len;
            }
        }
    }
    None
}

// ------------------------------------------------------------------
// DNS catch-all
// ------------------------------------------------------------------

/// Answers any single-question A/ANY query with the portal address, the
/// whole point of a captive portal's DNS. Returns the response length.
pub fn dns_answer(query: &[u8], portal_ip: [u8; 4], out: &mut [u8]) -> Option<usize> {
    if query.len() < 12 || query[2] & 0x80 != 0 {
        return None; // not a query
    }
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qdcount != 1 {
        return None;
    }
    // Walk the QNAME labels.
    let mut at = 12;
    loop {
        let len = *query.get(at)? as usize;
        if len == 0 {
            at += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None; // compression in a question is nonsense
        }
        at += 1 + len;
    }
    let qtype = u16::from_be_bytes([*query.get(at)?, *query.get(at + 1)?]);
    let question_end = at + 4;
    if qtype != 1 && qtype != 255 {
        return None; // only A/ANY get the captive answer
    }

    let answer_len = question_end + 16;
    if out.len() < answer_len {
        return None;
    }
    out[..question_end].copy_from_slice(&query[..question_end]);
    out[2] = 0x84; // response, authoritative
    out[3] = 0x00;
    out[6] = 0;
    out[7] = 1; // ancount = 1
    out[8..12].fill(0);
    let answer = &mut out[question_end..answer_len];
    answer[0] = 0xC0; // pointer to the question name
    answer[1] = 0x0C;
    answer[2..4].copy_from_slice(&1u16.to_be_bytes()); // type A
    answer[4..6].copy_from_slice(&1u16.to_be_bytes()); // class IN
    answer[6..10].copy_from_slice(&60u32.to_be_bytes()); // ttl
    answer[10..12].copy_from_slice(&4u16.to_be_bytes());
    answer[12..16].copy_from_slice(&portal_ip);
    Some(answer_len)
}

// ------------------------------------------------------------------
// HTTP request side
// ------------------------------------------------------------------

pub struct HttpRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a [u8],
}

/// Splits a buffered request once the header/body boundary and declared
/// content length are present. `None` means keep reading.
pub fn parse_request(raw: &[u8]) -> Option<HttpRequest<'_>> {
    let headers_end = raw.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let head = core::str::from_utf8(&raw[..headers_end]).ok()?;
    let mut request_line = head.lines().next()?.split(' ');
    let method = request_line.next()?;
    let path = request_line.next()?;
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let body = raw.get(headers_end..headers_end + content_length)?;
    Some(HttpRequest { method, path, body })
}

/// Pulls one key out of a urlencoded form body, decoding `%XX` and `+`
/// into `out`. Returns the decoded value.
pub fn form_value<'a>(body: &[u8], key: &str, out: &'a mut [u8]) -> Option<&'a str> {
    for pair in body.split(|byte| *byte == b'&') {
        let mut halves = pair.splitn(2, |byte| *byte == b'=');
        let name = halves.next()?;
        let value = halves.next().unwrap_or(&[]);
        if name != key.as_bytes() {
            continue;
        }
        let mut written = 0;
        let mut at = 0;
        while at < value.len() {
            if written >= out.len() {
                return None;
            }
            out[written] = match value[at] {
                b'+' => b' ',
                b'%' => {
                    let high = hex_nibble(*value.get(at + 1)?)?;
                    let low = hex_nibble(*value.get(at + 2)?)?;
                    at += 2;
                    (high << 4) | low
                }
                byte => byte,
            };
            written += 1;
            at += 1;
        }
        return core::str::from_utf8(&out[..written]).ok();
    }
    None
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    const PORTAL: [u8; 4] = [192, 168, 4, 1];

    fn dhcp_request(message_type: u8, mac: [u8; 6], extra: &[u8]) -> std::vec::Vec<u8> {
        let mut packet = std::vec![0u8; DHCP_OPTIONS_AT];
        packet[0] = BOOTREQUEST;
        packet[1] = 1;
        packet[2] = 6;
        packet[4..8].copy_from_slice(&0xdeadbeefu32.to_be_bytes());
        packet[28..34].copy_from_slice(&mac);
        packet[236..240].copy_from_slice(&DHCP_MAGIC);
        packet.extend_from_slice(&[53, 1, message_type]);
        packet.extend_from_slice(extra);
        packet.push(255);
        packet
    }

    #[test]
    fn dhcp_discover_gets_an_offer_with_portal_options() {
        let mut server = DhcpServer::new(PORTAL);
        let mac = [2, 0, 0, 0, 0, 7];
        let mut out = [0u8; DHCP_REPLY_LEN];
        let len = server
            .handle(&dhcp_request(DHCP_DISCOVER, mac, &[]), &mut out)
            .unwrap();
        let reply = &out[..len];
        assert_eq!(reply[0], BOOTREPLY);
        assert_eq!(&reply[4..8], &0xdeadbeefu32.to_be_bytes());
        assert_eq!(&reply[16..20], &[192, 168, 4, 2]); // first lease
        assert_eq!(&reply[28..34], &mac);
        assert_eq!(
            option_value(&reply[DHCP_OPTIONS_AT..], 53),
            Some(&[DHCP_OFFER][..])
        );
        assert_eq!(
            option_value(&reply[DHCP_OPTIONS_AT..], 54),
            Some(&PORTAL[..])
        );
        assert_eq!(
            option_value(&reply[DHCP_OPTIONS_AT..], 3),
            Some(&PORTAL[..])
        );
        assert_eq!(
            option_value(&reply[DHCP_OPTIONS_AT..], 6),
            Some(&PORTAL[..])
        );
    }

    #[test]
    fn dhcp_request_for_offered_ip_is_acked_and_stable() {
        let mut server = DhcpServer::new(PORTAL);
        let mac = [2, 0, 0, 0, 0, 7];
        let mut out = [0u8; DHCP_REPLY_LEN];
        server
            .handle(&dhcp_request(DHCP_DISCOVER, mac, &[]), &mut out)
            .unwrap();
        let len = server
            .handle(
                &dhcp_request(DHCP_REQUEST, mac, &[50, 4, 192, 168, 4, 2]),
                &mut out,
            )
            .unwrap();
        assert_eq!(
            option_value(&out[DHCP_OPTIONS_AT..len], 53),
            Some(&[DHCP_ACK][..])
        );
        // Same client keeps the same address.
        let len = server
            .handle(&dhcp_request(DHCP_DISCOVER, mac, &[]), &mut out)
            .unwrap();
        assert_eq!(&out[..len][16..20], &[192, 168, 4, 2]);
    }

    #[test]
    fn dhcp_request_for_foreign_ip_is_nakked() {
        let mut server = DhcpServer::new(PORTAL);
        let mut out = [0u8; DHCP_REPLY_LEN];
        let len = server
            .handle(
                &dhcp_request(DHCP_REQUEST, [2, 0, 0, 0, 0, 9], &[50, 4, 10, 0, 0, 5]),
                &mut out,
            )
            .unwrap();
        assert_eq!(
            option_value(&out[DHCP_OPTIONS_AT..len], 53),
            Some(&[DHCP_NAK][..])
        );
    }

    #[test]
    fn dns_query_resolves_to_portal() {
        // Standard query for captive.apple.com A.
        let mut query = std::vec::Vec::new();
        query.extend_from_slice(&[0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        for label in ["captive", "apple", "com"] {
            query.push(label.len() as u8);
            query.extend_from_slice(label.as_bytes());
        }
        query.push(0);
        query.extend_from_slice(&[0, 1, 0, 1]);

        let mut out = [0u8; 128];
        let len = dns_answer(&query, PORTAL, &mut out).unwrap();
        let reply = &out[..len];
        assert_eq!(&reply[..2], &[0x12, 0x34]);
        assert_eq!(reply[2] & 0x80, 0x80); // response bit
        assert_eq!(&reply[6..8], &[0, 1]); // one answer
        assert_eq!(&reply[len - 4..], &PORTAL);
        // AAAA queries get no captive answer.
        let mut aaaa = query.clone();
        let qtype_at = len - 16 - 4; // question qtype offset
        aaaa[qtype_at + 1] = 28;
        assert!(dns_answer(&aaaa, PORTAL, &mut out).is_none());
    }

    #[test]
    fn parses_form_post() {
        let raw = b"POST /save HTTP/1.1\r\nHost: 192.168.4.1\r\nContent-Length: 33\r\nContent-Type: application/x-www-form-urlencoded\r\n\r\nssid=latent.space&pass=a%26b+c%2F9";
        // Note: declared length 33 truncates the last byte on purpose below.
        let request = parse_request(&raw[..raw.len() - 1]).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/save");
        let mut ssid_buf = [0u8; 32];
        let mut pass_buf = [0u8; 64];
        assert_eq!(
            form_value(request.body, "ssid", &mut ssid_buf),
            Some("latent.space")
        );
        assert_eq!(
            form_value(request.body, "pass", &mut pass_buf),
            Some("a&b c/")
        );
    }

    #[test]
    fn incomplete_request_keeps_reading() {
        assert!(parse_request(b"POST /save HTTP/1.1\r\nContent-Length: 5\r\n\r\nab").is_none());
        assert!(parse_request(b"GET / HTTP/1.1\r\n").is_none());
    }
}
