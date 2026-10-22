//! Decoded images on the card: one pack file per orientation per book.
//!
//! A decoded image is a record in its orientation's pack: a 16-byte header,
//! then its rows. Records are only ever appended. The image index
//! ([`super::index`]) holds each image's decode status and the offset of its
//! record, and is updated only after every row is written, so a decode that
//! stops part way leaves an orphan tail that nothing points at.
//!
//! Rows run top to bottom in reading orientation, `stride` bytes each, one
//! bit per pixel with the leftmost pixel in the most significant bit and 1
//! for white, the framebuffer's convention. Padding bits at the end of a row
//! are white. Landscape and flipped landscape fit an image to the same box,
//! so they share the landscape pack; drawing applies the flip.
//!
//! | Offset | Bytes | Field |
//! |---|---|---|
//! | 0 | 4 | magic `X4PK` |
//! | 4 | 2 | width, pixels |
//! | 6 | 2 | height, rows |
//! | 8 | 2 | stride, bytes per row (`ceil(width / 8)`) |
//! | 10 | 2 | reserved, zero |
//! | 12 | 4 | low 32 bits of the image's path hash |
//!
//! A reader checks the header against what the index entry and the page's
//! image block expect (path hash, fitted width and height) before drawing a
//! single row, so a stale offset or a damaged pack draws the placeholder
//! instead of someone else's pixels.

/// The landscape pack, shared by flipped landscape.
pub const LANDSCAPE_PACK_FILE: &str = "IMGL.BIN";
/// The portrait pack.
pub const PORTRAIT_PACK_FILE: &str = "IMGP.BIN";
/// Where a deflated image is inflated before it decodes, beside the packs.
pub const SPOOL_FILE: &str = "SPOOL.TMP";
pub const PACK_RECORD_MAGIC: [u8; 4] = *b"X4PK";
pub const PACK_RECORD_HEADER_BYTES: usize = 16;
/// A pack never grows past this. Every record offset and length then fits
/// a `u32` with room to spare, and a book that keeps failing part way
/// cannot fill the card with orphan tails.
pub const MAX_PACK_BYTES: u32 = 64 * 1024 * 1024;

/// Both names in one named static. An eight-byte string literal on its own
/// joins the merged constant pools, which the ESP32-C3 build keeps in RAM.
static PACK_FILES: [u8; 16] = *b"IMGL.BINIMGP.BIN";

/// The pack file for an orientation: [`LANDSCAPE_PACK_FILE`] or
/// [`PORTRAIT_PACK_FILE`].
pub fn pack_file(portrait: bool) -> &'static str {
    let at = if portrait { 8 } else { 0 };
    PACK_FILES
        .get(at..at + 8)
        .and_then(|name| core::str::from_utf8(name).ok())
        .unwrap_or_default()
}

/// Bytes per row of a `width`-pixel image.
pub const fn pack_stride(width: u16) -> u16 {
    width.div_ceil(8)
}

/// The header of one decoded image in a pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackRecord {
    pub width: u16,
    pub height: u16,
    pub stride: u16,
    pub path_check: u32,
}

impl PackRecord {
    /// The header for a `width × height` decode of the image at `path_hash`.
    /// `None` for an empty size, which no fitted image has.
    pub fn new(path_hash: u64, width: u16, height: u16) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        Some(Self {
            width,
            height,
            stride: pack_stride(width),
            path_check: path_hash as u32,
        })
    }

    /// Bytes of rows after the header.
    pub fn rows_len(&self) -> u32 {
        u32::from(self.stride) * u32::from(self.height)
    }

    /// Header plus rows.
    pub fn record_len(&self) -> u32 {
        PACK_RECORD_HEADER_BYTES as u32 + self.rows_len()
    }

    /// Whether this record is the decode a page expects: the image at
    /// `path_hash` at exactly the fitted size.
    pub fn matches(&self, path_hash: u64, width: u16, height: u16) -> bool {
        self.path_check == path_hash as u32 && self.width == width && self.height == height
    }

    /// Whether this record is a decode of the image at `path_hash` that a
    /// rect of `width × height` holds: the fitted size, or a smaller one,
    /// as when an image shown alone in a paragraph has since been shown
    /// inside a line and decodes at its inline size.
    pub fn fits_in(&self, path_hash: u64, width: u16, height: u16) -> bool {
        self.path_check == path_hash as u32 && self.width <= width && self.height <= height
    }

    pub fn encode(&self) -> [u8; PACK_RECORD_HEADER_BYTES] {
        let mut out = [0; PACK_RECORD_HEADER_BYTES];
        out[0..4].copy_from_slice(&PACK_RECORD_MAGIC);
        out[4..6].copy_from_slice(&self.width.to_le_bytes());
        out[6..8].copy_from_slice(&self.height.to_le_bytes());
        out[8..10].copy_from_slice(&self.stride.to_le_bytes());
        out[12..16].copy_from_slice(&self.path_check.to_le_bytes());
        out
    }

    /// Reads a header from the card. `None` unless the magic, the reserved
    /// bytes, and the stride all agree, so a read at a wrong offset is not
    /// mistaken for a record.
    pub fn decode(bytes: &[u8; PACK_RECORD_HEADER_BYTES]) -> Option<Self> {
        if bytes[0..4] != PACK_RECORD_MAGIC || bytes[10..12] != [0, 0] {
            return None;
        }
        let width = u16::from_le_bytes([bytes[4], bytes[5]]);
        let height = u16::from_le_bytes([bytes[6], bytes[7]]);
        let stride = u16::from_le_bytes([bytes[8], bytes[9]]);
        let path_check = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        if width == 0 || height == 0 || stride != pack_stride(width) {
            return None;
        }
        Some(Self {
            width,
            height,
            stride,
            path_check,
        })
    }
}

/// Whether a record of `record_len` bytes can be appended at `pack_len`
/// without the pack passing [`MAX_PACK_BYTES`].
pub fn fits_in_pack(pack_len: u32, record_len: u32) -> bool {
    pack_len
        .checked_add(record_len)
        .is_some_and(|end| end <= MAX_PACK_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_round_trips() {
        let record = PackRecord::new(0x1234_5678_9abc_def0, 586, 440).unwrap();
        assert_eq!(record.stride, 74);
        assert_eq!(record.rows_len(), 74 * 440);
        assert_eq!(record.record_len(), 16 + 74 * 440);
        let bytes = record.encode();
        assert_eq!(&bytes[0..4], b"X4PK");
        assert_eq!(PackRecord::decode(&bytes), Some(record));
        assert!(record.matches(0x1234_5678_9abc_def0, 586, 440));
        assert!(!record.matches(0x1234_5678_9abc_def0, 586, 439));
        assert!(!record.matches(0x1234_5678_0000_0000, 586, 440));
        // A rect holds its own size or a larger one, of the same image.
        assert!(record.fits_in(0x1234_5678_9abc_def0, 586, 440));
        assert!(record.fits_in(0x1234_5678_9abc_def0, 700, 451));
        assert!(!record.fits_in(0x1234_5678_9abc_def0, 585, 451));
        assert!(!record.fits_in(0x1234_5678_9abc_def0, 700, 439));
        assert!(!record.fits_in(0x1234_5678_0000_0000, 700, 451));
    }

    #[test]
    fn strides_round_up_to_whole_bytes() {
        for (width, stride) in [(1, 1), (7, 1), (8, 1), (9, 2), (16, 2), (17, 3), (784, 98)] {
            assert_eq!(pack_stride(width), stride, "width {width}");
        }
    }

    #[test]
    fn empty_sizes_have_no_record() {
        assert_eq!(PackRecord::new(1, 0, 10), None);
        assert_eq!(PackRecord::new(1, 10, 0), None);
    }

    #[test]
    fn damaged_headers_are_refused() {
        let good = PackRecord::new(7, 100, 50).unwrap().encode();
        assert!(PackRecord::decode(&good).is_some());
        let mut bad_magic = good;
        bad_magic[0] = b'Y';
        assert_eq!(PackRecord::decode(&bad_magic), None);
        let mut bad_reserved = good;
        bad_reserved[10] = 1;
        assert_eq!(PackRecord::decode(&bad_reserved), None);
        let mut bad_stride = good;
        bad_stride[8] = 99;
        assert_eq!(PackRecord::decode(&bad_stride), None);
        let mut zero_width = good;
        zero_width[4] = 0;
        zero_width[5] = 0;
        assert_eq!(PackRecord::decode(&zero_width), None);
        assert_eq!(PackRecord::decode(&[0; PACK_RECORD_HEADER_BYTES]), None);
    }

    #[test]
    fn packs_stay_under_their_cap() {
        assert!(fits_in_pack(0, 16));
        assert!(fits_in_pack(MAX_PACK_BYTES - 16, 16));
        assert!(!fits_in_pack(MAX_PACK_BYTES - 15, 16));
        assert!(!fits_in_pack(u32::MAX, 1));
    }

    #[test]
    fn each_orientation_has_its_pack() {
        assert_eq!(pack_file(false), "IMGL.BIN");
        assert_eq!(pack_file(true), "IMGP.BIN");
        assert_eq!(pack_file(false), LANDSCAPE_PACK_FILE);
        assert_eq!(pack_file(true), PORTRAIT_PACK_FILE);
    }
}
