use heapless::String;

pub const FONT_PACK_MAGIC: &[u8; 4] = b"X4FT";
pub const FONT_PACK_VERSION: u16 = 1;
pub const FONT_PACK_HEADER_BYTES: usize = 64;
pub const FONT_PACK_FACE_RECORD_BYTES: usize = 64;
pub const FONT_PACK_METRIC_BYTES: usize = 12;
pub const FONT_PACK_KERNING_BYTES: usize = 6;
pub const FONT_PACK_MAX_NAME_BYTES: usize = 63;
pub const FONT_PACK_DIR: &str = "FONTS";
pub const FONT_PACK_FILE: &str = "CUSTOM.FNT";
pub const FONT_PACK_FACE_REGULAR: u8 = 0;
pub const FONT_PACK_FACE_ITALIC: u8 = 1;
pub const FONT_PACK_FACE_BOLD: u8 = 2;
pub const FONT_PACK_FACE_BOLD_ITALIC: u8 = 3;
pub const FONT_PACK_SIZE_SMALL: u8 = 19;
pub const FONT_PACK_SIZE_MEDIUM: u8 = 22;
pub const FONT_PACK_SIZE_LARGE: u8 = 26;
pub const FONT_PACK_CODEPOINT_COUNT: u16 = 1631;

const FONT_PACK_RANGES: &[(u16, u16)] = &[
    (0x0020, 0x007E),
    (0x00A0, 0x024F),
    (0x0370, 0x03FF),
    (0x0400, 0x04FF),
    (0x1E00, 0x1EFF),
    (0x2000, 0x206F),
    (0x20A0, 0x20CF),
    (0x2100, 0x214F),
    (0x2190, 0x21FF),
    (0x25A0, 0x25FF),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontPackError {
    BufferTooSmall,
    BadMagic,
    BadVersion,
    BadLength,
    BadHash,
    Utf8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontPackHeader {
    pub total_len: u32,
    pub identity: u64,
    pub face_count: u16,
    pub codepoint_count: u16,
    pub name_len: u16,
    pub style_bits: u16,
    pub face_table_offset: u32,
    pub codepoints_offset: u32,
    pub name_offset: u32,
    pub data_offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontPackFaceRecord {
    pub size_px: u8,
    pub style: u8,
    pub line_height: u8,
    pub baseline: u8,
    pub flags: u32,
    pub metrics_offset: u32,
    pub metric_count: u32,
    pub bitmap_offset: u32,
    pub bitmap_len: u32,
    pub kerning_offset: u32,
    pub kerning_count: u32,
}

impl FontPackHeader {
    pub fn decode(input: &[u8]) -> Result<Self, FontPackError> {
        if input.len() < FONT_PACK_HEADER_BYTES {
            return Err(FontPackError::BufferTooSmall);
        }
        if &input[..4] != FONT_PACK_MAGIC {
            return Err(FontPackError::BadMagic);
        }
        let version = read_u16(input, 4)?;
        if version != FONT_PACK_VERSION {
            return Err(FontPackError::BadVersion);
        }
        let header_len = read_u16(input, 6)?;
        if header_len as usize != FONT_PACK_HEADER_BYTES {
            return Err(FontPackError::BadLength);
        }
        let header = Self {
            total_len: read_u32(input, 8)?,
            identity: read_u64(input, 12)?,
            face_count: read_u16(input, 20)?,
            codepoint_count: read_u16(input, 22)?,
            name_len: read_u16(input, 24)?,
            style_bits: read_u16(input, 26)?,
            face_table_offset: read_u32(input, 28)?,
            codepoints_offset: read_u32(input, 32)?,
            name_offset: read_u32(input, 36)?,
            data_offset: read_u32(input, 40)?,
        };
        if header.name_len as usize > FONT_PACK_MAX_NAME_BYTES
            || header.face_table_offset < FONT_PACK_HEADER_BYTES as u32
            || header.codepoints_offset < header.face_table_offset
            || header.name_offset < header.codepoints_offset
            || header.data_offset < header.name_offset
            || header.total_len < header.data_offset
        {
            return Err(FontPackError::BadLength);
        }
        Ok(header)
    }
}

impl FontPackFaceRecord {
    pub const EMPTY: Self = Self {
        size_px: 0,
        style: 0,
        line_height: 0,
        baseline: 0,
        flags: 0,
        metrics_offset: 0,
        metric_count: 0,
        bitmap_offset: 0,
        bitmap_len: 0,
        kerning_offset: 0,
        kerning_count: 0,
    };

    pub fn decode(input: &[u8]) -> Result<Self, FontPackError> {
        if input.len() < FONT_PACK_FACE_RECORD_BYTES {
            return Err(FontPackError::BufferTooSmall);
        }
        Ok(Self {
            size_px: input[0],
            style: input[1],
            line_height: input[2],
            baseline: input[3],
            flags: read_u32(input, 4)?,
            metrics_offset: read_u32(input, 8)?,
            metric_count: read_u32(input, 12)?,
            bitmap_offset: read_u32(input, 16)?,
            bitmap_len: read_u32(input, 20)?,
            kerning_offset: read_u32(input, 24)?,
            kerning_count: read_u32(input, 28)?,
        })
    }
}

pub fn font_pack_codepoint_index(codepoint: u16) -> Option<usize> {
    let mut offset = 0usize;
    for &(start, end) in FONT_PACK_RANGES {
        if (start..=end).contains(&codepoint) {
            return Some(offset + usize::from(codepoint - start));
        }
        offset += usize::from(end - start) + 1;
    }
    None
}

pub fn decode_font_pack_name<const N: usize>(
    header: FontPackHeader,
    bytes: &[u8],
) -> Result<String<N>, FontPackError> {
    if bytes.len() != header.name_len as usize {
        return Err(FontPackError::BadLength);
    }
    let text = core::str::from_utf8(bytes).map_err(|_| FontPackError::Utf8)?;
    String::try_from(text).map_err(|_| FontPackError::BadLength)
}

fn read_u16(input: &[u8], offset: usize) -> Result<u16, FontPackError> {
    let bytes = input
        .get(offset..offset + 2)
        .ok_or(FontPackError::BufferTooSmall)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(input: &[u8], offset: usize) -> Result<u32, FontPackError> {
    let bytes = input
        .get(offset..offset + 4)
        .ok_or(FontPackError::BufferTooSmall)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(input: &[u8], offset: usize) -> Result<u64, FontPackError> {
    let bytes = input
        .get(offset..offset + 8)
        .ok_or(FontPackError::BufferTooSmall)?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes() -> [u8; FONT_PACK_HEADER_BYTES] {
        let mut out = [0u8; FONT_PACK_HEADER_BYTES];
        out[..4].copy_from_slice(FONT_PACK_MAGIC);
        out[4..6].copy_from_slice(&FONT_PACK_VERSION.to_le_bytes());
        out[6..8].copy_from_slice(&(FONT_PACK_HEADER_BYTES as u16).to_le_bytes());
        out[8..12].copy_from_slice(&8192u32.to_le_bytes());
        out[12..20].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        out[20..22].copy_from_slice(&12u16.to_le_bytes());
        out[22..24].copy_from_slice(&1631u16.to_le_bytes());
        out[24..26].copy_from_slice(&8u16.to_le_bytes());
        out[26..28].copy_from_slice(&0b1111u16.to_le_bytes());
        out[28..32].copy_from_slice(&64u32.to_le_bytes());
        out[32..36].copy_from_slice(&832u32.to_le_bytes());
        out[36..40].copy_from_slice(&4094u32.to_le_bytes());
        out[40..44].copy_from_slice(&4102u32.to_le_bytes());
        out
    }

    #[test]
    fn decodes_header() {
        let header = FontPackHeader::decode(&header_bytes()).unwrap();
        assert_eq!(header.identity, 0x1122_3344_5566_7788);
        assert_eq!(header.face_count, 12);
        assert_eq!(header.name_len, 8);
        assert_eq!(header.style_bits, 0b1111);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = header_bytes();
        bytes[0] = b'Y';
        assert_eq!(FontPackHeader::decode(&bytes), Err(FontPackError::BadMagic));
    }

    #[test]
    fn decodes_name() {
        let header = FontPackHeader::decode(&header_bytes()).unwrap();
        let name: String<16> = decode_font_pack_name(header, b"FontName").unwrap();
        assert_eq!(name.as_str(), "FontName");
    }

    #[test]
    fn maps_codepoints_to_pack_indices() {
        assert_eq!(font_pack_codepoint_index(0x0020), Some(0));
        assert_eq!(font_pack_codepoint_index(0x007E), Some(94));
        assert_eq!(font_pack_codepoint_index(0x00A0), Some(95));
        assert_eq!(font_pack_codepoint_index(0x25FF), Some(1630));
        assert_eq!(font_pack_codepoint_index(0x007F), None);
    }
}
