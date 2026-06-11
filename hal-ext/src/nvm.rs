#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgressRecord {
    pub book_id: u32,
    pub page: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppStateRecord {
    pub book_id: u32,
    pub chapter: u16,
    pub screen: u32,
    pub shell_orientation: u8,
    pub reading_orientation: u8,
    pub refresh_policy: u8,
    pub font_size: u8,
    pub line_spacing: u8,
    pub source_hash: u32,
    pub source_size: u32,
}

impl AppStateRecord {
    pub const ENCODED_LEN: usize = 32;
    const V1_ENCODED_LEN: usize = 24;
    const MAGIC: u32 = 0x5834_4F53;
    const VERSION: u8 = 3;
    const V2_VERSION: u8 = 2;
    const V1_VERSION: u8 = 1;
    /// FontSize::Medium / LineSpacing::Normal as u8 in app-core.
    const DEFAULT_FONT_SIZE: u8 = 1;
    const DEFAULT_LINE_SPACING: u8 = 1;

    pub const fn new(book_id: u32) -> Self {
        Self {
            book_id,
            chapter: 0,
            screen: 0,
            shell_orientation: 3,
            reading_orientation: 0,
            refresh_policy: 1,
            font_size: Self::DEFAULT_FONT_SIZE,
            line_spacing: Self::DEFAULT_LINE_SPACING,
            source_hash: 0,
            source_size: 0,
        }
    }

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        write_u32(&mut out, 0, Self::MAGIC);
        out[4] = Self::VERSION;
        out[5] = self.shell_orientation;
        out[6] = self.reading_orientation;
        out[7] = self.refresh_policy;
        write_u32(&mut out, 8, self.book_id);
        write_u16(&mut out, 12, self.chapter);
        write_u32(&mut out, 14, self.screen);
        write_u32(&mut out, 18, self.source_hash);
        write_u32(&mut out, 22, self.source_size);
        // V3 fills the two bytes V2 left as zero padding, keeping the
        // record length and checksum span unchanged.
        out[26] = self.font_size;
        out[27] = self.line_spacing;
        let checksum = checksum(&out[..28]);
        write_u32(&mut out, 28, checksum);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::V1_ENCODED_LEN {
            return None;
        }
        if read_u32(bytes, 0) != Self::MAGIC {
            return None;
        }
        match bytes[4] {
            Self::VERSION | Self::V2_VERSION => {
                if bytes.len() < Self::ENCODED_LEN {
                    return None;
                }
                let expected = read_u32(bytes, 28);
                if checksum(&bytes[..28]) != expected {
                    return None;
                }
                let (font_size, line_spacing) = if bytes[4] == Self::VERSION {
                    (bytes[26], bytes[27])
                } else {
                    (Self::DEFAULT_FONT_SIZE, Self::DEFAULT_LINE_SPACING)
                };
                Some(Self {
                    book_id: read_u32(bytes, 8),
                    chapter: read_u16(bytes, 12),
                    screen: read_u32(bytes, 14),
                    shell_orientation: bytes[5],
                    reading_orientation: bytes[6],
                    refresh_policy: bytes[7],
                    font_size,
                    line_spacing,
                    source_hash: read_u32(bytes, 18),
                    source_size: read_u32(bytes, 22),
                })
            }
            Self::V1_VERSION => {
                let expected = read_u32(bytes, 20);
                if checksum(&bytes[..20]) != expected {
                    return None;
                }
                Some(Self {
                    book_id: read_u32(bytes, 8),
                    chapter: read_u16(bytes, 12),
                    screen: read_u32(bytes, 14),
                    shell_orientation: bytes[5],
                    reading_orientation: bytes[6],
                    refresh_policy: bytes[7],
                    font_size: Self::DEFAULT_FONT_SIZE,
                    line_spacing: Self::DEFAULT_LINE_SPACING,
                    source_hash: 0,
                    source_size: 0,
                })
            }
            _ => None,
        }
    }
}

pub trait ProgressStore {
    type Error;

    fn load(&mut self) -> Result<Option<ProgressRecord>, Self::Error>;
    fn store(&mut self, record: ProgressRecord) -> Result<(), Self::Error>;
}

pub trait AppStateStore {
    type Error;

    fn load_app_state(&mut self) -> Result<Option<AppStateRecord>, Self::Error>;
    fn store_app_state(&mut self, record: AppStateRecord) -> Result<(), Self::Error>;
}

/// Station credentials at `/XTEINK/WIFI.BIN`, written by the onboarding
/// portal and read back ahead of every sync session. Same envelope as
/// `AppStateRecord`: magic, version, payload, checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiCredentialsRecord {
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub password: [u8; 64],
    pub password_len: u8,
}

impl WifiCredentialsRecord {
    pub const ENCODED_LEN: usize = 4 + 1 + 1 + 1 + 32 + 64 + 4;
    const MAGIC: u32 = 0x5834_5746; // "X4WF"
    const VERSION: u8 = 1;

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        write_u32(&mut out, 0, Self::MAGIC);
        out[4] = Self::VERSION;
        out[5] = self.ssid_len.min(32);
        out[6] = self.password_len.min(64);
        out[7..39].copy_from_slice(&self.ssid);
        out[39..103].copy_from_slice(&self.password);
        let checksum = checksum(&out[..103]);
        write_u32(&mut out, 103, checksum);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::ENCODED_LEN
            || read_u32(bytes, 0) != Self::MAGIC
            || bytes[4] != Self::VERSION
            || read_u32(bytes, 103) != checksum(&bytes[..103])
        {
            return None;
        }
        let mut record = Self {
            ssid: [0; 32],
            ssid_len: bytes[5].min(32),
            password: [0; 64],
            password_len: bytes[6].min(64),
        };
        record.ssid.copy_from_slice(&bytes[7..39]);
        record.password.copy_from_slice(&bytes[39..103]);
        if record.ssid_len == 0 {
            return None;
        }
        Some(record)
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut hash = 0x811C_9DC5u32;
    for byte in bytes {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset] = value as u8;
    out[offset + 1] = (value >> 8) as u8;
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset] = value as u8;
    out[offset + 1] = (value >> 8) as u8;
    out[offset + 2] = (value >> 16) as u8;
    out[offset + 3] = (value >> 24) as u8;
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    bytes[offset] as u16 | ((bytes[offset + 1] as u16) << 8)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    bytes[offset] as u32
        | ((bytes[offset + 1] as u32) << 8)
        | ((bytes[offset + 2] as u32) << 16)
        | ((bytes[offset + 3] as u32) << 24)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> AppStateRecord {
        AppStateRecord {
            book_id: 7,
            chapter: 3,
            screen: 41,
            shell_orientation: 2,
            reading_orientation: 1,
            refresh_policy: 2,
            font_size: 2,
            line_spacing: 0,
            source_hash: 0xDEAD_BEEF,
            source_size: 123_456,
        }
    }

    #[test]
    fn app_state_round_trips_with_type_settings() {
        let encoded = record().encode();
        assert_eq!(AppStateRecord::decode(&encoded), Some(record()));
    }

    #[test]
    fn v2_records_decode_with_default_type_settings() {
        // A V2 record is a V3 record with zeroed type bytes and version 2;
        // the checksum spans the same 28 bytes in both versions.
        let mut encoded = record().encode();
        encoded[4] = AppStateRecord::V2_VERSION;
        encoded[26] = 0;
        encoded[27] = 0;
        let checksum = checksum(&encoded[..28]);
        write_u32(&mut encoded, 28, checksum);

        let decoded = AppStateRecord::decode(&encoded).expect("v2 decodes");
        assert_eq!(decoded.font_size, AppStateRecord::DEFAULT_FONT_SIZE);
        assert_eq!(decoded.line_spacing, AppStateRecord::DEFAULT_LINE_SPACING);
        assert_eq!(decoded.book_id, 7);
        assert_eq!(decoded.source_hash, 0xDEAD_BEEF);
    }

    #[test]
    fn corrupt_checksum_is_rejected() {
        let mut encoded = record().encode();
        encoded[26] ^= 0xFF;
        assert_eq!(AppStateRecord::decode(&encoded), None);
    }
}
