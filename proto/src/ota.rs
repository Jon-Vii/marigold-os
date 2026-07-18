//! ESP-IDF application-image integrity validation.
//!
//! Before a candidate firmware is written into the inactive OTA slot — over the
//! SD card or the air — it must be checked as thoroughly as the bootloader
//! would check it, because the in-app update path writes the slot raw and flips
//! `otadata` directly (bypassing the ROM's `esp_image_verify`, which rejects our
//! wide-eFuse-range image). A truncated or corrupt `.bin` that reached `otadata`
//! would brick the device on next boot, so it is rejected here first.
//!
//! This mirrors the ESP-IDF image format and the FreeInk SDK / CrossPoint
//! `FirmwareFlasher::validateImageFile`: image magic, a walk of the segment
//! table, the trailing XOR checksum byte, and the appended SHA-256 (when the
//! header flags it). It streams the image in fixed chunks so it needs no heap
//! and only a few hundred bytes of stack — the whole image never sits in RAM.

use crc::{Algorithm, Crc};
use sha2::{Digest, Sha256};

/// Legacy SD boot triggers are one-shot control aliases, not firmware-picker
/// source names. Both boards reject both aliases so cards remain safe to move.
pub fn is_legacy_trigger_alias(name: &str) -> bool {
    name.eq_ignore_ascii_case("FWUPDATE.BIN") || name.eq_ignore_ascii_case("FWUPDX3.BIN")
}

/// First byte of every ESP-IDF application image.
pub const IMAGE_MAGIC: u8 = 0xE9;

const HEADER_LEN: usize = 24;
const SEG_HEADER_LEN: usize = 8;
const CHECKSUM_SEED: u8 = 0xEF;
const SHA_TRAILER_LEN: usize = 32;
const MIN_IMAGE_LEN: usize = 64 * 1024;
const STREAM_CHUNK: usize = 512;

/// Source of image bytes, read strictly forward. `read_exact` must fill the
/// whole buffer from the current offset or report an error (a short read at EOF
/// is an error — the validator already knows the expected length).
pub trait ImageSource {
    /// The error is deliberately unit: the validator maps any read failure
    /// to `ImageError::Read`, so a richer type would only be discarded.
    #[allow(clippy::result_unit_err)]
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    /// Smaller than any real firmware — almost certainly truncated.
    TooSmall,
    /// Larger than the destination OTA partition.
    TooLarge,
    /// First byte is not `0xE9`.
    BadMagic,
    /// Segment table is malformed or a segment runs past end-of-file.
    BadSegments,
    /// XOR checksum byte does not match the image body.
    BadChecksum,
    /// Appended SHA-256 does not match the computed hash.
    BadSha,
    /// Body + padding (+ SHA) length does not equal the file length.
    BadSize,
    /// The source reported a read error / short read.
    Read,
}

/// Validate a candidate ESP-IDF image end to end.
///
/// `image_len` is the exact byte length of the source. `partition_len`, when
/// given, bounds the image to the destination OTA partition size. On `Ok(())`
/// the entire source has been consumed and the image is safe to flash.
pub fn validate_image<S: ImageSource>(
    src: &mut S,
    image_len: usize,
    partition_len: Option<usize>,
) -> Result<(), ImageError> {
    if image_len < MIN_IMAGE_LEN {
        return Err(ImageError::TooSmall);
    }
    if let Some(limit) = partition_len {
        if image_len > limit {
            return Err(ImageError::TooLarge);
        }
    }

    let mut header = [0u8; HEADER_LEN];
    src.read_exact(&mut header).map_err(|_| ImageError::Read)?;
    if header[0] != IMAGE_MAGIC {
        return Err(ImageError::BadMagic);
    }
    let segment_count = header[1];
    // Byte 23 (`hash_appended`) flags a SHA-256 trailer over the whole image.
    let hash_appended = header[23] != 0;

    let mut sha = Sha256::new();
    sha.update(header);
    // The XOR checksum is seeded with 0xEF and covers segment *data* only.
    let mut checksum = CHECKSUM_SEED;
    let mut pos = HEADER_LEN;

    let mut buf = [0u8; STREAM_CHUNK];
    for _ in 0..segment_count {
        if pos + SEG_HEADER_LEN > image_len {
            return Err(ImageError::BadSegments);
        }
        let mut seg_header = [0u8; SEG_HEADER_LEN];
        src.read_exact(&mut seg_header)
            .map_err(|_| ImageError::Read)?;
        sha.update(seg_header);
        pos += SEG_HEADER_LEN;

        let data_len =
            u32::from_le_bytes([seg_header[4], seg_header[5], seg_header[6], seg_header[7]])
                as usize;
        if pos + data_len > image_len {
            return Err(ImageError::BadSegments);
        }

        let mut remaining = data_len;
        while remaining > 0 {
            let want = remaining.min(STREAM_CHUNK);
            let chunk = &mut buf[..want];
            src.read_exact(chunk).map_err(|_| ImageError::Read)?;
            sha.update(&chunk[..]);
            for &b in chunk.iter() {
                checksum ^= b;
            }
            remaining -= want;
        }
        pos += data_len;
    }

    // The image is padded up to the next 16-byte boundary; the stored checksum
    // byte sits at that boundary minus one. `pad_len` is always in 1..=16.
    let pad_end = (pos + 16) & !15usize;
    let expected_len = pad_end + if hash_appended { SHA_TRAILER_LEN } else { 0 };
    if expected_len != image_len {
        return Err(ImageError::BadSize);
    }
    let pad_len = pad_end - pos;
    if pad_len == 0 || pad_len > 16 {
        return Err(ImageError::BadSize);
    }
    let mut pad = [0u8; 16];
    src.read_exact(&mut pad[..pad_len])
        .map_err(|_| ImageError::Read)?;
    sha.update(&pad[..pad_len]);

    let stored_checksum = pad[pad_len - 1];
    if checksum != stored_checksum {
        return Err(ImageError::BadChecksum);
    }

    if hash_appended {
        let mut trailer = [0u8; SHA_TRAILER_LEN];
        src.read_exact(&mut trailer).map_err(|_| ImageError::Read)?;
        let computed = sha.finalize();
        if computed.as_slice() != trailer {
            return Err(ImageError::BadSha);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// otadata / OTA slot selection
//
// The stock bootloader chooses the app partition from `otadata`: two 32-byte
// "select entries", one per flash sector. The entry with the highest *valid*
// `ota_seq` wins, and `(ota_seq - 1) % ota_partition_count` is the app slot to
// boot. An in-app update writes the freshly-flashed slot's entry into the
// *other* otadata sector with a higher seq, so the next boot selects it —
// without the ROM's `esp_image_verify` (which rejects our wide-eFuse image).
//
// This mirrors `esp-bootloader-esp-idf`'s `Ota` and the FreeInk SDK's
// `RecoveryBoot`/`OtaBootSwitch`. Keeping a host-testable copy here lets the
// seq/CRC/slot math be verified without hardware.
// ---------------------------------------------------------------------------

/// Length of one otadata select entry, and of each otadata flash sector's used
/// prefix. 32 bytes is also flash-encryption friendly.
pub const SELECT_ENTRY_LEN: usize = 32;

/// A never-written otadata seq (erased flash).
pub const UNINITIALIZED_SEQ: u32 = 0xFFFF_FFFF;

// esp_ota_img_states_t values we care about.
/// Freshly written, not yet marked valid. What we write on a new flash — this
/// is the state the FreeInk SDK / CrossPoint switch uses and it boots on the X4.
pub const OTA_IMG_NEW: u32 = 0x0;
/// Bootloader rollback-enabled state: the new app has booted once and must
/// mark itself valid before the next reset, or the bootloader may roll back.
pub const OTA_IMG_PENDING_VERIFY: u32 = 0x1;
pub const OTA_IMG_VALID: u32 = 0x2;
pub const OTA_IMG_INVALID: u32 = 0x3;
pub const OTA_IMG_ABORTED: u32 = 0x4;

// esp-bootloader-esp-idf's otadata CRC: reflected CRC-32, poly 0x04C11DB7,
// init 0, xorout 0xFFFFFFFF, over the little-endian `ota_seq` bytes. Identical
// to the ROM's `crc32_le(u32::MAX, ..)`. Verified: seq 1 -> 0x4743989A, which
// matches a real on-device otadata dump.
const OTADATA_CRC: Algorithm<u32> = Algorithm {
    width: 32,
    poly: 0x04c1_1db7,
    init: 0,
    refin: true,
    refout: true,
    xorout: 0xffff_ffff,
    check: 0,
    residue: 0,
};

/// CRC of a 4-byte little-endian `ota_seq`, as the bootloader stores and checks.
pub fn seq_crc(ota_seq: u32) -> u32 {
    Crc::<u32>::new(&OTADATA_CRC).checksum(&ota_seq.to_le_bytes())
}

/// The fields of an otadata select entry we act on. `seq_label` (20 bytes,
/// unused by the bootloader) is written as 0xFF and otherwise ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectEntry {
    pub ota_seq: u32,
    pub ota_state: u32,
    pub crc: u32,
}

impl SelectEntry {
    /// A fresh entry with a correct CRC.
    pub fn new(ota_seq: u32, ota_state: u32) -> Self {
        Self {
            ota_seq,
            ota_state,
            crc: seq_crc(ota_seq),
        }
    }

    pub fn from_bytes(b: &[u8; SELECT_ENTRY_LEN]) -> Self {
        Self {
            ota_seq: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            ota_state: u32::from_le_bytes([b[24], b[25], b[26], b[27]]),
            crc: u32::from_le_bytes([b[28], b[29], b[30], b[31]]),
        }
    }

    pub fn to_bytes(&self) -> [u8; SELECT_ENTRY_LEN] {
        let mut b = [0u8; SELECT_ENTRY_LEN];
        b[0..4].copy_from_slice(&self.ota_seq.to_le_bytes());
        b[4..24].copy_from_slice(&[0xFF; 20]); // seq_label, unused
        b[24..28].copy_from_slice(&self.ota_state.to_le_bytes());
        b[28..32].copy_from_slice(&self.crc.to_le_bytes());
        b
    }

    /// A bootable entry: initialised, CRC intact, and not marked bad — exactly
    /// the bootloader's own validity test.
    pub fn is_valid(&self) -> bool {
        self.ota_seq != UNINITIALIZED_SEQ
            && self.crc == seq_crc(self.ota_seq)
            && self.ota_state != OTA_IMG_INVALID
            && self.ota_state != OTA_IMG_ABORTED
    }
}

/// The app OTA slot the bootloader is currently selecting, derived from the two
/// otadata sectors. `None` means otadata is uninitialised (erased), in which
/// case the bootloader falls back to the first app partition — treat it as slot
/// 0. Used to pick the *inactive* slot as an update's destination.
pub fn active_app_slot(
    sector0: &[u8; SELECT_ENTRY_LEN],
    sector1: &[u8; SELECT_ENTRY_LEN],
    ota_count: u32,
) -> Option<u32> {
    let e0 = SelectEntry::from_bytes(sector0);
    let e1 = SelectEntry::from_bytes(sector1);
    let s0 = e0.is_valid().then_some(e0.ota_seq);
    let s1 = e1.is_valid().then_some(e1.ota_seq);
    let active_seq = match (s0, s1) {
        (None, None) => return None,
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (Some(a), Some(b)) => a.max(b),
    };
    Some((active_seq - 1) % ota_count.max(1))
}

/// The active otadata entry and its sector, using the same "highest valid seq"
/// rule as the bootloader. `None` means both entries are erased or bad.
pub fn active_select_entry(
    sector0: &[u8; SELECT_ENTRY_LEN],
    sector1: &[u8; SELECT_ENTRY_LEN],
) -> Option<(usize, SelectEntry)> {
    let e0 = SelectEntry::from_bytes(sector0);
    let e1 = SelectEntry::from_bytes(sector1);
    let s0 = e0.is_valid().then_some(e0.ota_seq);
    let s1 = e1.is_valid().then_some(e1.ota_seq);
    match (s0, s1) {
        (None, None) => None,
        (Some(_), None) => Some((0, e0)),
        (None, Some(_)) => Some((1, e1)),
        (Some(a), Some(b)) if a >= b => Some((0, e0)),
        (Some(_), Some(_)) => Some((1, e1)),
    }
}

/// If the selected app is in a rollback trial state, return the otadata write
/// needed to acknowledge it as booted successfully.
pub fn plan_mark_app_valid(
    sector0: &[u8; SELECT_ENTRY_LEN],
    sector1: &[u8; SELECT_ENTRY_LEN],
) -> Option<OtaSwitch> {
    let (target_sector, active) = active_select_entry(sector0, sector1)?;
    if active.ota_state != OTA_IMG_NEW && active.ota_state != OTA_IMG_PENDING_VERIFY {
        return None;
    }
    Some(OtaSwitch {
        target_sector,
        entry: SelectEntry::new(active.ota_seq, OTA_IMG_VALID),
    })
}

/// The single otadata write that makes `dest_slot` the next boot target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OtaSwitch {
    /// Which otadata sector (0 or 1) to erase and overwrite.
    pub target_sector: usize,
    /// The 32-byte entry to write there.
    pub entry: SelectEntry,
}

/// Plan the otadata write that boots `dest_slot` (0-based app OTA index) next.
///
/// `sector0`/`sector1` are the two raw otadata entries as read from flash, and
/// `ota_count` is the number of OTA app partitions (2 for our layout). Mirrors
/// `OtaBootSwitch::switchTo`: find the active entry (highest valid seq), pick
/// the smallest higher seq that maps to `dest_slot`, and write it into the
/// *other* sector so the bootloader sees a newer, valid entry there.
pub fn plan_switch(
    sector0: &[u8; SELECT_ENTRY_LEN],
    sector1: &[u8; SELECT_ENTRY_LEN],
    dest_slot: u32,
    ota_count: u32,
) -> OtaSwitch {
    let e0 = SelectEntry::from_bytes(sector0);
    let e1 = SelectEntry::from_bytes(sector1);
    let s0 = e0.is_valid().then_some(e0.ota_seq);
    let s1 = e1.is_valid().then_some(e1.ota_seq);

    let (active_sector, active_seq) = match (s0, s1) {
        (None, None) => (None, 0),
        (Some(a), None) => (Some(0usize), a),
        (None, Some(b)) => (Some(1usize), b),
        (Some(a), Some(b)) if a >= b => (Some(0usize), a),
        (Some(_), Some(b)) => (Some(1usize), b),
    };

    let ota_count = ota_count.max(1);
    let mut new_seq = active_seq + 1;
    while (new_seq - 1) % ota_count != dest_slot % ota_count {
        new_seq += 1;
    }

    let target_sector = match active_sector {
        Some(0) => 1,
        Some(_) => 0,
        None => 0,
    };

    OtaSwitch {
        target_sector,
        entry: SelectEntry::new(new_seq, OTA_IMG_NEW),
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_trigger_aliases_cannot_apply_on_two_boots() {
        let picker_files = ["Marigold-1.2.3.bin", "FWUPDATE.BIN", "fwupdx3.bin"];
        let selectable: heapless::Vec<&str, 3> = picker_files
            .into_iter()
            .filter(|name| !is_legacy_trigger_alias(name))
            .collect();
        assert_eq!(selectable.as_slice(), &["Marigold-1.2.3.bin"]);

        // The picker consumes its pending record after boot one. Since a
        // trigger alias can never enter that record, the retained source file
        // cannot masquerade as the legacy trigger on boot two.
        let mut pending = selectable.first().copied();
        assert_eq!(pending.take(), Some("Marigold-1.2.3.bin"));
        assert_eq!(pending.take(), None);
    }
    use std::vec::Vec;

    /// Cursor over an owned byte buffer implementing `ImageSource`.
    struct Cursor {
        bytes: Vec<u8>,
        pos: usize,
    }
    impl ImageSource for Cursor {
        fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()> {
            let end = self.pos + buf.len();
            if end > self.bytes.len() {
                return Err(());
            }
            buf.copy_from_slice(&self.bytes[self.pos..end]);
            self.pos = end;
            Ok(())
        }
    }

    /// Build a structurally valid ESP-IDF image with the given segment data
    /// lengths, correct XOR checksum, and (optionally) a SHA-256 trailer — the
    /// same construction the bootloader validates against.
    fn build_image(segment_lens: &[usize], hash_appended: bool) -> Vec<u8> {
        let mut img = Vec::new();
        let mut header = [0u8; HEADER_LEN];
        header[0] = IMAGE_MAGIC;
        header[1] = segment_lens.len() as u8;
        header[23] = if hash_appended { 1 } else { 0 };
        img.extend_from_slice(&header);

        let mut checksum = CHECKSUM_SEED;
        for (i, &len) in segment_lens.iter().enumerate() {
            let mut seg_header = [0u8; SEG_HEADER_LEN];
            // load address (arbitrary, not validated) + data length
            seg_header[0..4].copy_from_slice(&(0x3c00_0000u32 + i as u32).to_le_bytes());
            seg_header[4..8].copy_from_slice(&(len as u32).to_le_bytes());
            img.extend_from_slice(&seg_header);
            for j in 0..len {
                let b = (i as u8).wrapping_mul(31).wrapping_add(j as u8);
                checksum ^= b;
                img.push(b);
            }
        }

        // Pad to the next 16-byte boundary; the last pad byte is the checksum.
        let pad_end = (img.len() + 16) & !15usize;
        while img.len() < pad_end - 1 {
            img.push(0);
        }
        img.push(checksum);

        if hash_appended {
            let mut sha = Sha256::new();
            sha.update(&img);
            img.extend_from_slice(&sha.finalize());
        }
        img
    }

    fn cursor(bytes: Vec<u8>) -> Cursor {
        Cursor { bytes, pos: 0 }
    }

    // A minimum-size image needs >= 64 KiB; use one fat segment plus a few small
    // ones so the segment walk and the streaming chunk boundary both get hit.
    fn valid_image(hash_appended: bool) -> Vec<u8> {
        build_image(&[70_000, 8, 513, 1], hash_appended)
    }

    fn image_with_project_name(project: &str) -> Vec<u8> {
        const APP_DESC_FILE_OFFSET: usize = HEADER_LEN + SEG_HEADER_LEN;
        const APP_DESC_PROJECT_OFFSET: usize = APP_DESC_FILE_OFFSET + 48;
        const APP_DESC_PROJECT_LEN: usize = 32;

        let mut img = valid_image(false);
        img[APP_DESC_FILE_OFFSET..APP_DESC_FILE_OFFSET + 4]
            .copy_from_slice(&0xABCD_5432u32.to_le_bytes());
        img[APP_DESC_PROJECT_OFFSET..APP_DESC_PROJECT_OFFSET + APP_DESC_PROJECT_LEN].fill(0);
        img[APP_DESC_PROJECT_OFFSET..APP_DESC_PROJECT_OFFSET + project.len()]
            .copy_from_slice(project.as_bytes());

        let mut checksum = CHECKSUM_SEED;
        let mut pos = HEADER_LEN;
        for _ in 0..img[1] {
            let len = u32::from_le_bytes([img[pos + 4], img[pos + 5], img[pos + 6], img[pos + 7]])
                as usize;
            pos += SEG_HEADER_LEN;
            for byte in &img[pos..pos + len] {
                checksum ^= *byte;
            }
            pos += len;
        }
        let checksum_at = ((pos + 16) & !15usize) - 1;
        img[checksum_at] = checksum;
        img
    }

    #[test]
    fn accepts_hash_appended_image() {
        let img = valid_image(true);
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, Some(0x640000)),
            Ok(())
        );
    }

    #[test]
    fn accepts_image_without_hash_trailer() {
        let img = valid_image(false);
        let len = img.len();
        assert_eq!(validate_image(&mut cursor(img), len, None), Ok(()));
    }

    #[test]
    fn accepts_valid_images_from_other_firmware_projects() {
        let img = image_with_project_name("crosspoint-reader");
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, Some(0x640000)),
            Ok(())
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut img = valid_image(true);
        img[0] = 0x00;
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::BadMagic)
        );
    }

    #[test]
    fn rejects_too_small() {
        let img = build_image(&[16], true);
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::TooSmall)
        );
    }

    #[test]
    fn rejects_image_larger_than_partition() {
        let img = valid_image(true);
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, Some(len - 1)),
            Err(ImageError::TooLarge)
        );
    }

    #[test]
    fn rejects_corrupt_body_via_checksum() {
        let mut img = valid_image(false);
        // Flip a byte inside the first segment's data (just past the 24-byte
        // header + 8-byte segment header). Without a SHA trailer the XOR
        // checksum is the gate that catches it.
        img[HEADER_LEN + SEG_HEADER_LEN + 3] ^= 0xFF;
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::BadChecksum)
        );
    }

    #[test]
    fn rejects_corrupt_body_via_sha_when_checksum_still_matches() {
        // Flip two bytes whose XOR cancels in the checksum but not in SHA-256,
        // proving the SHA trailer catches damage the byte-XOR misses.
        let mut img = valid_image(true);
        let a = HEADER_LEN + SEG_HEADER_LEN + 1;
        let b = HEADER_LEN + SEG_HEADER_LEN + 2;
        img[a] ^= 0x5A;
        img[b] ^= 0x5A;
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::BadSha)
        );
    }

    #[test]
    fn rejects_length_mismatch() {
        // A trailing byte the segment table + padding don't account for: the
        // structural length no longer equals the declared length.
        let mut img = valid_image(false);
        img.push(0);
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::BadSize)
        );
    }

    #[test]
    fn rejects_short_source() {
        // The declared length is structurally consistent, but the source runs
        // out before delivering it (e.g. a half-written SD file).
        let mut img = valid_image(true);
        let len = img.len();
        img.truncate(len - 10);
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::Read)
        );
    }

    #[test]
    fn rejects_segment_running_past_eof() {
        let mut img = valid_image(false);
        // Inflate the first segment's declared data length so it overruns EOF.
        let huge = 0x00FF_FFFFu32.to_le_bytes();
        img[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&huge);
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::BadSegments)
        );
    }

    // --- otadata -----------------------------------------------------------

    #[test]
    fn seq_crc_matches_rom_and_device() {
        // seq 1 -> 0x4743989A is a real on-device otadata CRC and the value the
        // authoritative esp-bootloader-esp-idf algorithm produces. The others
        // are independently computed from the same CRC parameters.
        assert_eq!(seq_crc(1), 0x4743_989A);
        assert_eq!(seq_crc(2), 0x55F6_3774);
        assert_eq!(seq_crc(3), 0xED4A_5011);
    }

    #[test]
    fn select_entry_round_trips_with_valid_crc() {
        let e = SelectEntry::new(5, OTA_IMG_NEW);
        let bytes = e.to_bytes();
        let back = SelectEntry::from_bytes(&bytes);
        assert_eq!(back, e);
        assert!(back.is_valid());
        // seq_label region is 0xFF, CRC sits in the last 4 bytes.
        assert_eq!(&bytes[4..24], &[0xFF; 20]);
        assert_eq!(
            u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]),
            seq_crc(5)
        );
    }

    #[test]
    fn uninitialised_and_corrupt_entries_are_invalid() {
        let erased = [0xFFu8; SELECT_ENTRY_LEN];
        assert!(!SelectEntry::from_bytes(&erased).is_valid());

        let mut bad = SelectEntry::new(7, OTA_IMG_NEW).to_bytes();
        bad[28] ^= 0xFF; // corrupt the stored CRC
        assert!(!SelectEntry::from_bytes(&bad).is_valid());

        let aborted = SelectEntry::new(7, OTA_IMG_ABORTED);
        assert!(!aborted.is_valid());
    }

    #[test]
    fn switch_from_erased_otadata_targets_requested_slot() {
        let erased = [0xFFu8; SELECT_ENTRY_LEN];
        // First boot from erased otadata into slot 0.
        let sw0 = plan_switch(&erased, &erased, 0, 2);
        assert_eq!(sw0.target_sector, 0);
        assert_eq!(sw0.entry.ota_seq, 1); // (1-1)%2 == 0
        assert!(sw0.entry.is_valid());

        // ...or into slot 1.
        let sw1 = plan_switch(&erased, &erased, 1, 2);
        assert_eq!(sw1.entry.ota_seq, 2); // (2-1)%2 == 1
        assert_eq!((sw1.entry.ota_seq - 1) % 2, 1);
    }

    #[test]
    fn switch_writes_other_sector_with_higher_seq() {
        // sector0 active at seq=3 (slot (3-1)%2 == 0). Switch to slot 1.
        let active = SelectEntry::new(3, OTA_IMG_NEW).to_bytes();
        let erased = [0xFFu8; SELECT_ENTRY_LEN];
        let sw = plan_switch(&active, &erased, 1, 2);

        assert_eq!(sw.target_sector, 1, "must write the inactive sector");
        assert!(sw.entry.ota_seq > 3, "new seq must exceed the active seq");
        assert_eq!((sw.entry.ota_seq - 1) % 2, 1, "must map to slot 1");
        assert!(sw.entry.is_valid());
    }

    #[test]
    fn active_slot_tracks_highest_valid_seq() {
        let erased = [0xFFu8; SELECT_ENTRY_LEN];
        assert_eq!(active_app_slot(&erased, &erased, 2), None);

        // seq 3 -> slot (3-1)%2 == 0
        let s3 = SelectEntry::new(3, OTA_IMG_NEW).to_bytes();
        assert_eq!(active_app_slot(&s3, &erased, 2), Some(0));

        // higher seq 4 in the other sector -> slot (4-1)%2 == 1 wins
        let s4 = SelectEntry::new(4, OTA_IMG_NEW).to_bytes();
        assert_eq!(active_app_slot(&s3, &s4, 2), Some(1));

        // an aborted higher seq is ignored -> falls back to seq 3
        let s9_aborted = SelectEntry::new(9, OTA_IMG_ABORTED).to_bytes();
        assert_eq!(active_app_slot(&s3, &s9_aborted, 2), Some(0));
    }

    #[test]
    fn mark_app_valid_only_rewrites_trial_state() {
        let old = SelectEntry::new(3, OTA_IMG_VALID).to_bytes();
        let new = SelectEntry::new(4, OTA_IMG_PENDING_VERIFY).to_bytes();
        let sw = plan_mark_app_valid(&old, &new).expect("pending app should be acknowledged");
        assert_eq!(sw.target_sector, 1);
        assert_eq!(sw.entry.ota_seq, 4);
        assert_eq!(sw.entry.ota_state, OTA_IMG_VALID);
        assert_eq!(sw.entry.crc, seq_crc(4));

        let valid = sw.entry.to_bytes();
        assert_eq!(plan_mark_app_valid(&old, &valid), None);
    }

    #[test]
    fn mark_app_valid_accepts_new_state() {
        let erased = [0xFFu8; SELECT_ENTRY_LEN];
        let new = SelectEntry::new(2, OTA_IMG_NEW).to_bytes();
        let sw = plan_mark_app_valid(&erased, &new).expect("new app should be acknowledged");
        assert_eq!(sw.target_sector, 1);
        assert_eq!(sw.entry.ota_seq, 2);
        assert_eq!(sw.entry.ota_state, OTA_IMG_VALID);
    }

    #[test]
    fn switch_ignores_invalidated_higher_seq() {
        // sector1 has a higher seq but is ABORTED, so sector0 (seq 3) is active.
        let active = SelectEntry::new(3, OTA_IMG_NEW).to_bytes();
        let aborted = SelectEntry::new(9, OTA_IMG_ABORTED).to_bytes();
        let sw = plan_switch(&active, &aborted, 0, 2);
        // Active is sector0, so we write sector1, seq just above 3 mapping slot 0.
        assert_eq!(sw.target_sector, 1);
        assert_eq!(sw.entry.ota_seq, 5); // 4->(3)%2=1 no; 5->(4)%2=0 yes
        assert_eq!((sw.entry.ota_seq - 1) % 2, 0);
    }
}
