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
    pub source_hash: u32,
    pub source_size: u32,
}

impl AppStateRecord {
    pub const ENCODED_LEN: usize = 32;
    const V1_ENCODED_LEN: usize = 24;
    const MAGIC: u32 = 0x5834_4F53;
    const VERSION: u8 = 2;
    const V1_VERSION: u8 = 1;

    pub const fn new(book_id: u32) -> Self {
        Self {
            book_id,
            chapter: 0,
            screen: 0,
            shell_orientation: 3,
            reading_orientation: 0,
            refresh_policy: 1,
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
            Self::VERSION => {
                if bytes.len() < Self::ENCODED_LEN {
                    return None;
                }
                let expected = read_u32(bytes, 28);
                if checksum(&bytes[..28]) != expected {
                    return None;
                }
                Some(Self {
                    book_id: read_u32(bytes, 8),
                    chapter: read_u16(bytes, 12),
                    screen: read_u32(bytes, 14),
                    shell_orientation: bytes[5],
                    reading_orientation: bytes[6],
                    refresh_policy: bytes[7],
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
