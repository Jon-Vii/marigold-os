//! Framing for durable two-generation settings records.
//!
//! Small mutable state (reading position, app state, Wi-Fi credentials)
//! lives on FAT, where an in-place truncate-and-rewrite loses the record if
//! power fails between the truncate and the write landing. Writers instead
//! alternate between two sibling files (`…A`/`…B`), each holding one
//! self-validating record framed here; readers keep whichever side carries
//! the newest valid generation. A torn write corrupts only the side being
//! replaced, never the survivor.
//!
//! Record layout (little-endian):
//!
//! ```text
//! magic[4] | version u8 | generation u32 | payload[N] | checksum u32
//! ```
//!
//! The checksum is FNV-1a over everything before it. The layout matches
//! MarigoldOS v0.4.x byte-for-byte so cards move between the two firmwares
//! without losing state.

pub const DURABLE_VERSION: u8 = 1;
/// Bytes of framing around the payload: magic + version + generation + checksum.
pub const DURABLE_OVERHEAD: usize = 13;
/// Ceiling on a whole record; keeps every scratch buffer a small stack array.
pub const DURABLE_MAX_BYTES: usize = 128;

/// FNV-1a, the record checksum.
pub fn durable_checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

/// Serial-number arithmetic over the u32 generation counter: `candidate` is
/// newer when it sits in the half-window ahead of `current`, so the counter
/// survives wraparound.
pub fn generation_is_newer(candidate: u32, current: u32) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < 0x8000_0000
}

/// Frame `payload` as one durable record into `out`; returns the record
/// length. Fails when the payload cannot fit under [`DURABLE_MAX_BYTES`].
// The unit error mirrors fw's cache-file helpers (write_state_file et al.),
// where the only response to an oversized payload is failing the write.
#[allow(clippy::result_unit_err)]
pub fn encode_durable_record(
    magic: [u8; 4],
    generation: u32,
    payload: &[u8],
    out: &mut [u8; DURABLE_MAX_BYTES],
) -> Result<usize, ()> {
    let total = payload.len().checked_add(DURABLE_OVERHEAD).ok_or(())?;
    if total > DURABLE_MAX_BYTES {
        return Err(());
    }
    out[..4].copy_from_slice(&magic);
    out[4] = DURABLE_VERSION;
    out[5..9].copy_from_slice(&generation.to_le_bytes());
    out[9..9 + payload.len()].copy_from_slice(payload);
    let checksum = durable_checksum(&out[..total - 4]);
    out[total - 4..total].copy_from_slice(&checksum.to_le_bytes());
    Ok(total)
}

/// Validate one durable record and extract its payload. `bytes` must be the
/// whole file, exactly `payload.len() + DURABLE_OVERHEAD` long, with matching
/// magic, version, and checksum; returns the record's generation.
pub fn decode_durable_record(magic: [u8; 4], bytes: &[u8], payload: &mut [u8]) -> Option<u32> {
    let total = payload.len().checked_add(DURABLE_OVERHEAD)?;
    if total > DURABLE_MAX_BYTES || bytes.len() != total {
        return None;
    }
    if bytes[..4] != magic || bytes[4] != DURABLE_VERSION {
        return None;
    }
    let stored = u32::from_le_bytes(bytes[total - 4..total].try_into().ok()?);
    if durable_checksum(&bytes[..total - 4]) != stored {
        return None;
    }
    payload.copy_from_slice(&bytes[9..total - 4]);
    Some(u32::from_le_bytes(bytes[5..9].try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAGIC: [u8; 4] = *b"MGTS";

    fn record(generation: u32, payload: &[u8]) -> ([u8; DURABLE_MAX_BYTES], usize) {
        let mut out = [0u8; DURABLE_MAX_BYTES];
        let len = encode_durable_record(MAGIC, generation, payload, &mut out).unwrap();
        (out, len)
    }

    #[test]
    fn round_trips_payload_and_generation() {
        let payload = [0x11u8, 0x22, 0x33, 0x44, 0x55];
        let (bytes, len) = record(7, &payload);
        assert_eq!(len, payload.len() + DURABLE_OVERHEAD);
        let mut out = [0u8; 5];
        assert_eq!(
            decode_durable_record(MAGIC, &bytes[..len], &mut out),
            Some(7)
        );
        assert_eq!(out, payload);
    }

    #[test]
    fn rejects_any_single_flipped_byte() {
        let payload = [0xA0u8, 0xA1, 0xA2, 0xA3];
        let (bytes, len) = record(3, &payload);
        for at in 0..len {
            let mut torn = bytes;
            torn[at] ^= 0x01;
            let mut out = [0u8; 4];
            assert_eq!(
                decode_durable_record(MAGIC, &torn[..len], &mut out),
                None,
                "flipped byte {at} must invalidate the record"
            );
        }
    }

    #[test]
    fn rejects_wrong_magic_length_and_version() {
        let payload = [1u8, 2, 3];
        let (bytes, len) = record(1, &payload);
        let mut out = [0u8; 3];
        assert_eq!(
            decode_durable_record(*b"XXXX", &bytes[..len], &mut out),
            None
        );
        assert_eq!(
            decode_durable_record(MAGIC, &bytes[..len - 1], &mut out),
            None
        );
        let mut short = [0u8; 2];
        assert_eq!(
            decode_durable_record(MAGIC, &bytes[..len], &mut short),
            None
        );
        let mut wrong_version = bytes;
        wrong_version[4] = DURABLE_VERSION + 1;
        // A bumped version alone also breaks the checksum; rewrite it so the
        // version check is what rejects.
        let checksum = durable_checksum(&wrong_version[..len - 4]);
        wrong_version[len - 4..len].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(
            decode_durable_record(MAGIC, &wrong_version[..len], &mut out),
            None
        );
    }

    #[test]
    fn oversized_payload_cannot_encode() {
        let payload = [0u8; DURABLE_MAX_BYTES];
        let mut out = [0u8; DURABLE_MAX_BYTES];
        assert_eq!(encode_durable_record(MAGIC, 1, &payload, &mut out), Err(()));
    }

    #[test]
    fn generation_ordering_survives_wraparound() {
        assert!(generation_is_newer(2, 1));
        assert!(!generation_is_newer(1, 2));
        assert!(!generation_is_newer(5, 5));
        assert!(generation_is_newer(0, u32::MAX));
        assert!(!generation_is_newer(u32::MAX, 0));
        assert!(generation_is_newer(u32::MAX.wrapping_add(4), u32::MAX));
    }
}
