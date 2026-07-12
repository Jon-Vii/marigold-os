//! The Wi-Fi session task behind the Wireless screen.
//!
//! Parked until the app sends `SyncCommand::Start`. The session is one
//! way by design: it asks the display task to loan the reader's scratch
//! memory as radio heap, joins the saved network in STA mode, and
//! serves the browser book shelf until the session ends; the
//! only path back to reading is the software reset on
//! `SyncCommand::Exit`. With no saved network the session runs the
//! AP-mode onboarding portal instead.

use crate::sync_mem::{self, SyncLoan};
use crate::upload::{sanitized_name, UploadBegin, UploadChunk};
use crate::{
    StorageCommand, SyncCommand, SyncEvent, STORAGE_COMMANDS, SYNC_COMMANDS, SYNC_EVENTS,
    SYNC_LOANS, UPLOAD_BEGINS, UPLOAD_CHUNKS, UPLOAD_RESULTS, UPLOAD_RETURNS,
};
use app_core::{SyncError, WifiCredentials};
use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_futures::select::select;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{
    Config as NetConfig, IpAddress, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources,
    StaticConfigV4,
};
use embassy_time::{with_timeout, Duration, Timer};
use esp_hal::peripherals::WIFI;
use esp_hal::rng::Rng;
use esp_radio::wifi::{
    ap::AccessPointConfig, sta::StationConfig, AuthenticationMethod, Config as WifiConfig,
    ControllerConfig, Interface, WifiController,
};
use proto::captive;

// Measured first-association joins ran ~21 s; give them headroom.
const JOIN_TIMEOUT: Duration = Duration::from_secs(35);
const DHCP_TIMEOUT: Duration = Duration::from_secs(15);
/// The hotspot beacons the SSID the join QR names; `ui::join_qr` is the
/// single source, so the QR a phone scans cannot drift from the AP.
const PORTAL_SSID: &str = ui::join_qr::PORTAL_SSID;
const PORTAL_IP: [u8; 4] = [192, 168, 4, 1];

/// Alphabet for the per-session portal PSK; lives in app-core next to
/// `PortalPsk` so the emulators' fixed demo value is host-tested against
/// it.
const PSK_ALPHABET: &[u8] = app_core::PSK_ALPHABET;

// mint_portal_psk's 6-bit draws can only reach indexes 0..=63; a longer
// alphabet would silently leave its tail characters unmintable.
const _: () = assert!(PSK_ALPHABET.len() <= 64);

/// Mints the onboarding hotspot's WPA2 PSK for this portal session from
/// the hardware RNG. Home credentials POST to /save over the hotspot
/// link, so it must not be open; and a PSK fixed at build time would be
/// public — committed to the repo or extractable from the released
/// firmware.bin — so it is drawn fresh here and travels only on the
/// screen's QR. Six-bit rejection sampling keeps the draw uniform over
/// the 55-character alphabet.
fn mint_portal_psk(rng: Rng) -> app_core::PortalPsk {
    let mut bytes = [0u8; app_core::PortalPsk::LEN];
    let mut filled = 0;
    while filled < bytes.len() {
        for byte in rng.random().to_le_bytes() {
            let draw = (byte & 0x3F) as usize;
            if draw < PSK_ALPHABET.len() && filled < bytes.len() {
                bytes[filled] = PSK_ALPHABET[draw];
                filled += 1;
            }
        }
    }
    // Every byte was drawn from PSK_ALPHABET, so validation cannot fail.
    app_core::PortalPsk::new(bytes).expect("minted PSK must be valid")
}

/// Compile-time station credentials for the dev phase:
/// `XTEINK_WIFI_SSID=... XTEINK_WIFI_PASS=... cargo build ...`
pub fn credentials() -> Option<(&'static str, &'static str)> {
    Some((
        option_env!("XTEINK_WIFI_SSID")?,
        option_env!("XTEINK_WIFI_PASS")?,
    ))
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn ap_net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
pub async fn run(spawner: Spawner, wifi: WIFI<'static>) {
    // Idle until the first Start; Exit before any radio work is a no-op
    // because nothing has been loaned yet.
    loop {
        match SYNC_COMMANDS.receive().await {
            SyncCommand::Start => break,
            SyncCommand::Exit => {}
        }
    }

    // The loan request runs through the storage queue so it serializes
    // behind any in-flight SD work, then the memory comes back to us.
    STORAGE_COMMANDS.send(StorageCommand::LoanSyncMemory).await;
    let loan = SYNC_LOANS.receive().await;
    sync_mem::donate_heap(loan.heap_a, loan.heap_b);
    let SyncLoan {
        tcp_rx,
        tcp_tx,
        http_a,
        http_b,
        wifi: stored_credentials,
        catalog_len,
        ..
    } = loan;

    // Stored credentials from the portal beat the compile-time dev pair;
    // neither present means this session runs the onboarding portal.
    let resolved = stored_credentials.or_else(|| {
        credentials().and_then(|(ssid, password)| WifiCredentials::from_strs(ssid, password))
    });

    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    // Deliberately trimmed, and re-validated on hardware 2026-07-11: an
    // upload A/B (3.2 MB EPUB, X3) measured no win from 8/24 RX buffers
    // + AMPDU-RX — throughput sits near 160 KB/s with either config, so
    // radio RX is not the bottleneck — while the bigger buffers cost
    // ~6.6 KB of the loaned heap at join. Before spending heap here
    // again, first find what actually caps upload throughput (the
    // per-upload heap log makes the slack observable).
    let radio_config = ControllerConfig::default()
        .with_static_rx_buf_num(4)
        .with_dynamic_rx_buf_num(8)
        .with_dynamic_tx_buf_num(8)
        .with_ampdu_rx_enable(false)
        .with_ampdu_tx_enable(false);
    let mut controller = match WifiController::new(wifi, radio_config) {
        Ok(controller) => controller,
        Err(err) => {
            esp_println::println!("wifi: init failed: {:?}", err);
            send_event(SyncEvent::Failed(SyncError::RadioInit));
            park_until_exit().await;
        }
    };
    let Some(creds) = resolved else {
        run_portal(
            spawner,
            &mut controller,
            seed,
            tcp_rx,
            tcp_tx,
            http_a,
            http_b,
        )
        .await;
    };

    let device = Interface::station();

    let resources: &'static mut StackResources<4> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(StackResources::new()));
    let (stack, runner) = embassy_net::new(
        device,
        NetConfig::dhcpv4(Default::default()),
        resources,
        seed,
    );
    spawner.spawn(net_task(runner).unwrap());

    let mut session = Session {
        controller,
        stack,
        started: false,
    };

    // First Start already consumed; later Starts are Confirm retries
    // from the error screen. A successful join falls through to the
    // book server, which runs until the session's reset.
    let ip = loop {
        match session.attempt(creds.ssid(), creds.password()).await {
            Ok(ip) => break ip,
            Err(error) => send_event(SyncEvent::Failed(error)),
        }
        // Start retries the session, Exit resets the device.
        match SYNC_COMMANDS.receive().await {
            SyncCommand::Start => {}
            SyncCommand::Exit => reset_now(),
        }
    };

    let stack = session.stack;
    esp_println::println!("upload: serving at {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
    send_event(SyncEvent::Serving(ip));
    select(
        exit_after_uploads(),
        upload_server(stack, tcp_rx, tcp_tx, http_a, http_b, catalog_len),
    )
    .await;
    unreachable!()
}

/// Exit during the serving phase defers the reset until any in-flight
/// book finishes writing (bounded), so a done press cannot truncate it.
async fn exit_after_uploads() -> ! {
    loop {
        if let SyncCommand::Exit = SYNC_COMMANDS.receive().await {
            let mut waited_ms = 0u32;
            while crate::upload::UPLOAD_IN_FLIGHT.load(portable_atomic::Ordering::SeqCst)
                && waited_ms < 120_000
            {
                Timer::after_millis(100).await;
                waited_ms += 100;
            }
            reset_now();
        }
    }
}

// ------------------------------------------------------------------
// Book upload server
// ------------------------------------------------------------------

const UPLOAD_PAGE: &str = concat!(
    r##"<!doctype html><html><head>"##,
    r##"<meta charset=utf-8>"##,
    r##"<meta name=viewport content="width=device-width,initial-scale=1">"##,
    r##"<title>Books · CalendulaOS</title><style>"##,
    r##"body{font-family:Georgia,'Times New Roman',serif;margin:3em auto;"##,
    r##"max-width:26em;padding:0 1.2em;color:#1a1a1a;background:#fbfbf8}"##,
    r##"h2{font-size:.8em;font-weight:600;letter-spacing:.25em;"##,
    r##"text-transform:uppercase;margin:2.2em 0 .8em}"##,
    r##"h2:first-of-type{margin-top:.5em}"##,
    r##"h2:before{content:'— '}"##,
    r##"ul{list-style:none;margin:0;padding:0}"##,
    r##"li{display:flex;align-items:baseline;justify-content:space-between;"##,
    r##"gap:1em;padding:.45em 0;border-bottom:1px dotted #bbb}"##,
    r##"li i{color:#777}"##,
    r##"a.del{font-size:.7em;letter-spacing:.2em;text-transform:uppercase;"##,
    r##"color:#888;text-decoration:none;white-space:nowrap;cursor:pointer}"##,
    r##"a.del:hover{color:#1a1a1a}"##,
    r##"#drop{border:1px dashed #999;border-radius:3px;padding:2.2em 1em;"##,
    r##"text-align:center;color:#666;font-style:italic;cursor:pointer}"##,
    r##"#drop.over{border-color:#1a1a1a;color:#1a1a1a}"##,
    r##"progress{width:7em;height:.45em;accent-color:#1a1a1a}"##,
    r##"footer{margin-top:3em;text-align:center;font-style:italic;"##,
    r##"color:#777;font-size:.85em}"##,
    r##"</style></head><body>"##,
    r##"<h2>Books</h2><ul id=shelf><li><i>reading the card …</i></li></ul>"##,
    r##"<h2>Add books</h2>"##,
    r##"<div id=drop>drop EPUB files here — or click to choose</div>"##,
    r##"<input id=files type=file accept=.epub multiple hidden>"##,
    r##"<ul id=queue></ul>"##,
    r##"<footer>changes appear on the reader after it restarts ·"##,
    r##" press <b>done</b> there to finish</footer>"##,
    r##"<script>"##,
    r##"const shelf=document.getElementById('shelf'),"##,
    r##"queue=document.getElementById('queue'),"##,
    r##"drop=document.getElementById('drop'),"##,
    r##"input=document.getElementById('files');"##,
    r##"function row(label){const li=document.createElement('li');"##,
    r##"const span=document.createElement('span');span.textContent=label;"##,
    r##"li.appendChild(span);return li}"##,
    r##"async function load(){let text=null;"##,
    r##"for(let i=0;i<10&&text===null;i++){try{"##,
    r##"const r=await fetch('/list');if(r.ok)text=await r.text();}"##,
    r##"catch(e){}if(text===null)await new Promise(d=>setTimeout(d,800))}"##,
    r##"if(text===null){shelf.textContent='';"##,
    r##"shelf.appendChild(row('— the card did not answer —'));return}"##,
    r##"shelf.textContent='';"##,
    r##"const lines=text.split(String.fromCharCode(10)).filter(Boolean);"##,
    r##"if(!lines.length){shelf.appendChild(row('— nothing yet —'))}"##,
    r##"for(const line of lines){const[flag,open,label]=line.split('|');"##,
    r##"const li=row(label||open);"##,
    r##"const a=document.createElement('a');a.className='del';"##,
    r##"a.textContent='remove';a.onclick=async()=>{"##,
    r##"if(!confirm('Remove '+(label||open)+' from the card?'))return;"##,
    r##"const r=await fetch('/delete?name='+encodeURIComponent(open)+"##,
    r##"(flag==='R'?'&root=1':''),"##,
    r##"{method:'POST'});if(r.ok)li.remove()};li.appendChild(a);"##,
    r##"shelf.appendChild(li)}}"##,
    r##"function send(files){[...files].reduce((chain,f)=>chain.then(()=>new Promise(done=>{"##,
    r##"const li=row(f.name);const bar=document.createElement('progress');"##,
    r##"bar.max=1;bar.value=0;li.appendChild(bar);queue.appendChild(li);"##,
    r##"const xhr=new XMLHttpRequest();"##,
    r##"xhr.open('POST','/upload?name='+encodeURIComponent(f.name));"##,
    r##"xhr.upload.onprogress=e=>{if(e.lengthComputable)bar.value=e.loaded/e.total};"##,
    r##"xhr.onloadend=()=>{bar.remove();"##,
    r##"li.appendChild(document.createTextNode(xhr.status===200?' ✓':' — failed'));"##,
    r##"done()};xhr.send(f)})),Promise.resolve())}"##,
    r##"drop.onclick=()=>input.click();"##,
    r##"input.onchange=()=>send(input.files);"##,
    r##"drop.ondragover=e=>{e.preventDefault();drop.classList.add('over')};"##,
    r##"drop.ondragleave=()=>drop.classList.remove('over');"##,
    r##"drop.ondrop=e=>{e.preventDefault();drop.classList.remove('over');"##,
    r##"send(e.dataTransfer.files)};"##,
    r##"load();"##,
    r##"</script></body></html>"##,
);

/// Serves the shelf page, streams POSTed books to the display task,
/// lists the catalog snapshot, and deletes /BOOKS entries on request.
async fn upload_server(
    stack: Stack<'static>,
    tcp_rx: &'static mut [u8],
    tcp_tx: &'static mut [u8],
    request_buf: &'static mut [u8],
    catalog: &'static mut [u8],
    catalog_len: usize,
) -> ! {
    // Staging ping-pong buffers live in the loaned heap.
    let mut pool: heapless::Vec<&'static mut [u8], 2> = heapless::Vec::new();
    let _ = pool.push(alloc::vec![0u8; 4096].leak());
    let _ = pool.push(alloc::vec![0u8; 4096].leak());
    let mut session_started = false;

    loop {
        let mut socket = TcpSocket::new(stack, &mut *tcp_rx, &mut *tcp_tx);
        socket.set_timeout(Some(Duration::from_secs(30)));
        if socket.accept(80).await.is_err() {
            continue;
        }

        let mut filled = 0;
        let head = loop {
            if filled == request_buf.len() {
                break None;
            }
            let Ok(read) = socket.read(&mut request_buf[filled..]).await else {
                break None;
            };
            if read == 0 {
                break None;
            }
            filled += read;
            if let Some(head) = captive::parse_request_head(&request_buf[..filled]) {
                break Some((
                    head.method.len(),
                    head.path.len(),
                    head.content_length,
                    head.body_start,
                ));
            }
        };
        let Some((method_len, path_len, content_length, body_start)) = head else {
            socket.close();
            continue;
        };
        // Reborrow the pieces by index so the buffer stays usable for the
        // body bytes that arrived with the headers.
        let path_at = method_len + 1;
        let is_upload_post = request_buf
            .get(..method_len)
            .map(|m| m == b"POST")
            .unwrap_or(false)
            && request_buf
                .get(path_at..path_at + path_len)
                .map(|p| p.starts_with(b"/upload"))
                .unwrap_or(false);

        let path = request_buf.get(path_at..path_at + path_len).unwrap_or(b"/");
        let is_list = path.starts_with(b"/list");
        let is_delete = request_buf
            .get(..method_len)
            .map(|m| m == b"POST")
            .unwrap_or(false)
            && path.starts_with(b"/delete");

        if is_list {
            let listing =
                core::str::from_utf8(&catalog[..catalog_len.min(catalog.len())]).unwrap_or("");
            let _ = write_http_response(&mut socket, "200 OK", listing).await;
        } else if is_delete {
            let mut path_bytes = request_buf.get_mut(path_at..path_at + path_len);
            let in_books = path_bytes
                .as_ref()
                .map(|p| !proto::upload::has_query_param(p, b"root=1"))
                .unwrap_or(true);
            let name = path_bytes
                .as_mut()
                .and_then(|p| proto::upload::raw_query_name(p))
                .and_then(|decoded| valid_short_name(decoded));
            let ok = match name {
                Some(name) => {
                    if !session_started {
                        STORAGE_COMMANDS.send(StorageCommand::ReceiveUpload).await;
                        session_started = true;
                    }
                    UPLOAD_BEGINS
                        .send(UploadBegin {
                            name,
                            delete: true,
                            in_books,
                            label: crate::upload::UploadLabel::new(),
                            identity_hash: 0,
                        })
                        .await;
                    UPLOAD_RESULTS.receive().await
                }
                None => false,
            };
            let _ = write_http_response(
                &mut socket,
                if ok { "200 OK" } else { "404 Not Found" },
                if ok { "deleted" } else { "failed" },
            )
            .await;
        } else if is_upload_post {
            let client_name = request_buf
                .get_mut(path_at..path_at + path_len)
                .and_then(proto::upload::raw_query_name)
                .map(|s| &*s)
                .unwrap_or(b"book");
            let name = sanitized_name(client_name);
            let label = crate::upload::readable_filename(client_name);
            let identity_hash = crate::upload::hash_identity(client_name);

            let begin = UploadBegin {
                name,
                delete: false,
                in_books: true,
                label,
                identity_hash,
            };

            if !session_started {
                STORAGE_COMMANDS.send(StorageCommand::ReceiveUpload).await;
                session_started = true;
            }
            let leftover_range = body_start..filled;
            let ok = stream_book(
                &mut socket,
                request_buf,
                leftover_range,
                content_length,
                begin,
                &mut pool,
            )
            .await;
            let _ = write_http_response(
                &mut socket,
                if ok {
                    "200 OK"
                } else {
                    "507 Insufficient Storage"
                },
                if ok { "stored" } else { "failed" },
            )
            .await;
        } else {
            let _ = write_http_response(&mut socket, "200 OK", UPLOAD_PAGE).await;
        }
        socket.close();
        let _ = with_timeout(Duration::from_secs(2), socket.flush()).await;
    }
}

/// Streams one book body to the display task; true when the card write
/// succeeded end to end.
async fn stream_book(
    socket: &mut TcpSocket<'_>,
    request_buf: &[u8],
    leftover: core::ops::Range<usize>,
    content_length: usize,
    begin: UploadBegin,
    pool: &mut heapless::Vec<&'static mut [u8], 2>,
) -> bool {
    esp_println::println!("upload: '{}' {} bytes", begin.name, content_length);
    crate::upload::UPLOAD_IN_FLIGHT.store(true, portable_atomic::Ordering::SeqCst);
    UPLOAD_BEGINS.send(begin).await;

    let mut leftover = &request_buf[leftover];
    if leftover.len() > content_length {
        leftover = &leftover[..content_length];
    }
    let mut remaining = content_length;
    let mut failed = false;
    while remaining > 0 && !failed {
        let buffer = match pool.pop() {
            Some(buffer) => buffer,
            None => UPLOAD_RETURNS.receive().await,
        };
        let mut len = 0;
        if !leftover.is_empty() {
            let take = leftover.len().min(buffer.len());
            buffer[..take].copy_from_slice(&leftover[..take]);
            leftover = &leftover[take..];
            len = take;
        }
        while len < buffer.len() && len < remaining {
            let window = buffer.len().min(remaining);
            match socket.read(&mut buffer[len..window]).await {
                Ok(0) | Err(_) => {
                    failed = true;
                    break;
                }
                Ok(read) => len += read,
            }
        }
        remaining -= len.min(remaining);
        UPLOAD_CHUNKS
            .send(UploadChunk {
                buffer: Some(buffer),
                len,
                last: remaining == 0 && !failed,
                abort: failed,
            })
            .await;
    }
    if content_length == 0 {
        // Nothing will flow; tell the writer to finish an empty file.
        UPLOAD_CHUNKS
            .send(UploadChunk {
                buffer: None,
                len: 0,
                last: true,
                abort: true,
            })
            .await;
    }
    // Refill the pool for the next file.
    let result = UPLOAD_RESULTS.receive().await;
    crate::upload::UPLOAD_IN_FLIGHT.store(false, portable_atomic::Ordering::SeqCst);
    // Heap slack per upload: the join-time log plus this one bound the
    // radio buffering budget (AMPDU reorder buffers allocate under load).
    esp_println::println!(
        "upload: heap used={} free={}",
        esp_alloc::HEAP.used(),
        esp_alloc::HEAP.free()
    );
    while pool.len() < 2 {
        match UPLOAD_RETURNS.try_receive() {
            Ok(buffer) => {
                let _ = pool.push(buffer);
            }
            Err(_) => break,
        }
    }
    result && !failed
}

// ------------------------------------------------------------------
// Onboarding portal
// ------------------------------------------------------------------

const PORTAL_PAGE: &str = concat!(
    "<!doctype html><html><head>",
    "<meta name=viewport content=\"width=device-width,initial-scale=1\">",
    "<title>CalendulaOS</title>",
    "<style>body{font-family:Georgia,serif;margin:2.5em auto;max-width:22em;",
    "padding:0 1em;color:#222}h1{font-size:1.25em;letter-spacing:.08em}",
    "label{display:block;margin:1em 0 .2em}",
    "input{width:100%;font-size:1.05em;padding:.5em;border:1px solid #999;",
    "border-radius:4px;box-sizing:border-box}",
    "button{margin-top:1.2em;font-size:1.05em;padding:.6em 1.6em;",
    "border:1px solid #222;background:#222;color:#fff;border-radius:4px}",
    "</style></head><body><h1>CalendulaOS</h1>",
    "<p>Connect this reader to your Wi-Fi network.</p>",
    "<form method=post action=/save>",
    "<label>Network name</label><input name=ssid maxlength=32 required>",
    "<label>Password</label><input name=pass type=password maxlength=64>",
    "<button>Save</button></form></body></html>",
);

const SAVED_PAGE: &str = concat!(
    "<!doctype html><html><head>",
    "<meta name=viewport content=\"width=device-width,initial-scale=1\">",
    "<title>CalendulaOS</title>",
    "<style>body{font-family:Georgia,serif;margin:2.5em auto;max-width:22em;",
    "padding:0 1em;color:#222}h1{font-size:1.25em;letter-spacing:.08em}",
    "</style></head><body><h1>Saved</h1>",
    "<p>Back on the reader: press <i>done</i>, then run sync again to ",
    "connect to your network.</p></body></html>",
);

/// The onboarding hotspot: WPA2 AP under a PSK minted for this session
/// (joined via the QR the Wireless screen renders from it), captive
/// DHCP + DNS, and the credential form on port 80. Never returns; the
/// session ends with the reset that `SyncCommand::Exit` triggers.
#[allow(clippy::too_many_arguments)]
async fn run_portal(
    spawner: Spawner,
    controller: &mut WifiController<'static>,
    seed: u64,
    tcp_rx: &'static mut [u8],
    tcp_tx: &'static mut [u8],
    http_a: &'static mut [u8],
    http_b: &'static mut [u8],
) -> ! {
    let psk = mint_portal_psk(Rng::new());
    let device = Interface::access_point();
    let config = WifiConfig::AccessPoint(
        AccessPointConfig::default()
            .with_ssid(PORTAL_SSID)
            .with_auth_method(AuthenticationMethod::Wpa2Personal)
            .with_password(psk.as_str().into()),
    );
    if controller.set_config(&config).is_err() {
        esp_println::println!("portal: ap start failed");
        SYNC_EVENTS
            .send(SyncEvent::Failed(SyncError::RadioInit))
            .await;
        park_until_exit().await;
    }

    let portal = Ipv4Address::new(PORTAL_IP[0], PORTAL_IP[1], PORTAL_IP[2], PORTAL_IP[3]);
    let mut dns_servers: heapless::Vec<Ipv4Address, 3> = heapless::Vec::new();
    let _ = dns_servers.push(portal);
    let resources: &'static mut StackResources<6> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(StackResources::new()));
    let (stack, runner) = embassy_net::new(
        device,
        NetConfig::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(portal, 24),
            gateway: Some(portal),
            dns_servers,
        }),
        resources,
        seed,
    );
    spawner.spawn(ap_net_task(runner).unwrap());

    // The PSK itself stays off the serial log; the screen is its only
    // channel.
    esp_println::println!("portal: up at 192.168.4.1 as {}", PORTAL_SSID);
    SYNC_EVENTS.send(SyncEvent::PortalUp(psk)).await;

    // Three servers share the task; Exit interrupts them with the reset.
    select(
        park_until_exit(),
        join3(
            dhcp_server(stack),
            dns_server(stack),
            credential_portal(stack, tcp_rx, tcp_tx, http_a, http_b),
        ),
    )
    .await;
    // park_until_exit resets and join3 never completes.
    unreachable!()
}

async fn dhcp_server(stack: Stack<'static>) -> ! {
    let rx_buf = alloc::vec![0u8; 1536].leak();
    let tx_buf = alloc::vec![0u8; 1536].leak();
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, rx_buf, &mut tx_meta, tx_buf);
    if socket.bind(67).is_err() {
        esp_println::println!("portal: dhcp bind failed");
        park_until_exit().await;
    }
    let mut server = captive::DhcpServer::new(PORTAL_IP);
    let mut packet = [0u8; 600];
    let mut reply = [0u8; captive::DHCP_REPLY_LEN];
    loop {
        let Ok((len, _meta)) = socket.recv_from(&mut packet).await else {
            continue;
        };
        if let Some(reply_len) = server.handle(&packet[..len], &mut reply) {
            let _ = socket
                .send_to(&reply[..reply_len], (IpAddress::v4(255, 255, 255, 255), 68))
                .await;
        }
    }
}

async fn dns_server(stack: Stack<'static>) -> ! {
    let rx_buf = alloc::vec![0u8; 1024].leak();
    let tx_buf = alloc::vec![0u8; 1024].leak();
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, rx_buf, &mut tx_meta, tx_buf);
    if socket.bind(53).is_err() {
        esp_println::println!("portal: dns bind failed");
        park_until_exit().await;
    }
    let mut query = [0u8; 300];
    let mut answer = [0u8; 360];
    loop {
        let Ok((len, meta)) = socket.recv_from(&mut query).await else {
            continue;
        };
        if let Some(answer_len) = captive::dns_answer(&query[..len], PORTAL_IP, &mut answer) {
            let _ = socket.send_to(&answer[..answer_len], meta).await;
        }
    }
}

async fn credential_portal(
    stack: Stack<'static>,
    tcp_rx: &'static mut [u8],
    tcp_tx: &'static mut [u8],
    request_buf: &'static mut [u8],
    _spare: &'static mut [u8],
) -> ! {
    loop {
        let mut socket = TcpSocket::new(stack, &mut *tcp_rx, &mut *tcp_tx);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if socket.accept(80).await.is_err() {
            continue;
        }

        let mut filled = 0;
        let saved = loop {
            if filled == request_buf.len() {
                break false;
            }
            let Ok(read) = socket.read(&mut request_buf[filled..]).await else {
                break false;
            };
            if read == 0 {
                break false;
            }
            filled += read;
            if let Some(request) = captive::parse_request(&request_buf[..filled]) {
                break handle_portal_request(&request).await;
            }
        };

        let body = if saved { SAVED_PAGE } else { PORTAL_PAGE };
        let _ = write_http_page(&mut socket, body).await;
        socket.close();
        let _ = with_timeout(Duration::from_secs(2), socket.flush()).await;
    }
}

/// Routes one parsed request; true means credentials were captured and
/// the success page should answer.
async fn handle_portal_request(request: &captive::HttpRequest<'_>) -> bool {
    if request.method != "POST" || request.path != "/save" {
        return false;
    }
    let mut ssid_buf = [0u8; 32];
    let mut pass_buf = [0u8; 64];
    let ssid = captive::form_value(request.body, "ssid", &mut ssid_buf).unwrap_or("");
    let password = captive::form_value(request.body, "pass", &mut pass_buf).unwrap_or("");
    let Some(credentials) = WifiCredentials::from_strs(ssid, password) else {
        return false;
    };
    esp_println::println!("portal: credentials captured for '{}'", credentials.ssid());
    let ssid = credentials.ssid_message();
    STORAGE_COMMANDS
        .send(StorageCommand::StoreWifiCredentials(credentials))
        .await;
    send_event(SyncEvent::CredentialsSaved(ssid));
    true
}

/// Accepts an existing 8.3 catalog open-name verbatim: short, printable
/// ASCII, no path separators. Deletion must not invent or mangle names.
fn valid_short_name(raw: &[u8]) -> Option<crate::upload::UploadName> {
    if raw.is_empty() || raw.len() > 12 {
        return None;
    }
    let mut name = crate::upload::UploadName::new();
    for byte in raw.iter().copied() {
        if !byte.is_ascii_graphic() || byte == b'/' || byte == b'\\' {
            return None;
        }
        let _ = name.push(byte as char);
    }
    Some(name)
}

async fn write_http_page(
    socket: &mut TcpSocket<'_>,
    body: &str,
) -> Result<(), embassy_net::tcp::Error> {
    write_http_response(socket, "200 OK", body).await
}

async fn write_http_response(
    socket: &mut TcpSocket<'_>,
    status: &str,
    body: &str,
) -> Result<(), embassy_net::tcp::Error> {
    let mut length = [0u8; 8];
    let mut at = length.len();
    let mut value = body.len();
    loop {
        at -= 1;
        length[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    write_all(socket, b"HTTP/1.1 ").await?;
    write_all(socket, status.as_bytes()).await?;
    write_all(
        socket,
        b"\r\ncache-control: no-store\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: ",
    )
    .await?;
    write_all(socket, &length[at..]).await?;
    write_all(socket, b"\r\nconnection: close\r\n\r\n").await?;
    write_all(socket, body.as_bytes()).await
}

async fn write_all(
    socket: &mut TcpSocket<'_>,
    mut data: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        if written == 0 {
            return Err(embassy_net::tcp::Error::ConnectionReset);
        }
        data = &data[written..];
    }
    Ok(())
}

async fn park_until_exit() -> ! {
    loop {
        if let SyncCommand::Exit = SYNC_COMMANDS.receive().await {
            reset_now();
        }
    }
}

fn reset_now() -> ! {
    esp_println::println!("wifi: sync session over, resetting");
    // Let the message drain the UART before the reset takes the port.
    esp_hal::system::software_reset()
}

fn send_event(event: SyncEvent) {
    if SYNC_EVENTS.try_send(event).is_err() {
        esp_println::println!("wifi: sync event queue full");
    }
}

struct Session {
    controller: WifiController<'static>,
    stack: Stack<'static>,
    started: bool,
}

impl Session {
    /// One join attempt: associate, wait for DHCP, report the address.
    async fn attempt(&mut self, ssid: &str, password: &str) -> Result<[u8; 4], SyncError> {
        send_event(SyncEvent::Connecting);
        self.join(ssid, password).await?;

        let config = with_timeout(DHCP_TIMEOUT, async {
            loop {
                if let Some(config) = self.stack.config_v4() {
                    return config;
                }
                Timer::after_millis(100).await;
            }
        })
        .await
        .map_err(|_| SyncError::Dhcp)?;
        let ip = config.address.address().octets();
        esp_println::println!("wifi: up at {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        esp_println::println!(
            "wifi: heap used={} free={}",
            esp_alloc::HEAP.used(),
            esp_alloc::HEAP.free()
        );
        send_event(SyncEvent::Connected(ip));
        Ok(ip)
    }

    async fn join(&mut self, ssid: &str, password: &str) -> Result<(), SyncError> {
        if !self.started {
            let config = WifiConfig::Station(
                StationConfig::default()
                    .with_ssid(ssid)
                    .with_password(password.into())
                    .with_auth_method(if password.is_empty() {
                        AuthenticationMethod::None
                    } else {
                        AuthenticationMethod::Wpa2Personal
                    }),
            );
            self.controller
                .set_config(&config)
                .map_err(|_| SyncError::Join)?;
            self.started = true;
        }
        with_timeout(JOIN_TIMEOUT, self.controller.connect_async())
            .await
            .map_err(|_| SyncError::Join)?
            .map(|_| ())
            .map_err(|err| {
                esp_println::println!("wifi: join failed: {:?}", err);
                SyncError::Join
            })
    }
}
