//! Two committed book-index generations. A footer is written last and checksums
//! the complete body plus generation number. Section banks are retired before
//! reuse so recovery never selects an index into an overwritten bank.
use super::generation_is_newer;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use proto::cache::{book_v2_cache_size, decode_book_v2_header, BOOK_V2_HEADER_BYTES};

pub(super) const NAMES: [&str; 2] = ["BOOKA.BIN", "BOOKB.BIN"];
const FOOTER: usize = 16;
const MAGIC: &[u8; 4] = b"MGBK";

fn checksum_step(hash: u32, bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(hash, |h, b| (h ^ u32::from(*b)).wrapping_mul(0x0100_0193))
}

#[derive(Clone, Copy)]
struct Candidate {
    generation: u32,
    body_len: u32,
    checksum: u32,
}

// End-of-file is an invalid record; a device failure is an uncertain record
// that must never authorize overwriting a possibly healthy generation.
fn read_record<D, T, const ND: usize, const NF: usize, const NV: usize>(
    file: &File<'_, D, T, ND, NF, NV>,
    mut bytes: &mut [u8],
) -> Result<bool, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    while !bytes.is_empty() {
        let count = file.read(bytes).map_err(|_| ())?;
        if count == 0 {
            return Ok(false);
        }
        bytes = &mut bytes[count..];
    }
    Ok(true)
}

fn candidate<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    name: &str,
) -> Result<Option<Candidate>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match dir.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(()),
    };
    let Some(body_len) = file.length().checked_sub(FOOTER as u32) else {
        return Ok(None);
    };
    if body_len < BOOK_V2_HEADER_BYTES as u32 || body_len > 65536 {
        return Ok(None);
    }
    file.seek_from_start(body_len).map_err(|_| ())?;
    let mut footer = [0; FOOTER];
    if !read_record(&file, &mut footer)? {
        return Ok(None);
    }
    if &footer[..4] != MAGIC || u32::from_le_bytes(footer[8..12].try_into().unwrap()) != body_len {
        return Ok(None);
    }
    let generation = u32::from_le_bytes(footer[4..8].try_into().unwrap());
    file.seek_from_start(0).map_err(|_| ())?;
    let mut header = [0; BOOK_V2_HEADER_BYTES];
    if !read_record(&file, &mut header)? {
        return Ok(None);
    }
    let Ok(header) = decode_book_v2_header(&header) else {
        return Ok(None);
    };
    if header.section_count as usize > super::MAX_BOOK_SECTIONS
        || header.toc_count as usize > super::MAX_SD_TOC_ITEMS
        || header.toc_text_bytes as usize > super::MAX_SD_TOC_TEXT_BYTES
        || header.title_text_bytes > 64
        || header.author_text_bytes > 64
        || book_v2_cache_size(header) != body_len as usize
    {
        return Ok(None);
    }
    Ok(Some(Candidate {
        generation,
        body_len,
        checksum: u32::from_le_bytes(footer[12..16].try_into().unwrap()),
    }))
}

fn candidate_matches<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    name: &str,
    candidate: Candidate,
) -> Result<bool, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match dir.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(false),
        Err(_) => return Err(()),
    };
    Ok(body_checksum(&file, candidate.body_len, candidate.generation)? == Some(candidate.checksum))
}

pub(super) fn generation<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    name: &str,
    verify: bool,
) -> Option<u32>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let candidate = candidate(dir, name).ok()??;
    if verify && !candidate_matches(dir, name, candidate).ok()? {
        return None;
    }
    Some(candidate.generation)
}

/// Read both small commit records, then verify only the newest candidate.
/// A corrupt body permits fallback; an I/O failure leaves selection uncertain
/// and must abort any operation that might retire or truncate a generation.
pub(super) fn selected_checked<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    verify: bool,
) -> Result<Option<(usize, u32)>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let candidates = [candidate(dir, NAMES[0])?, candidate(dir, NAMES[1])?];
    let newest = match candidates {
        [None, None] => return Ok(None),
        [Some(_), None] => 0,
        [None, Some(_)] => 1,
        [Some(a), Some(b)] => usize::from(generation_is_newer(b.generation, a.generation)),
    };
    for slot in [newest, 1 - newest] {
        if let Some(candidate) = candidates[slot] {
            if !verify || candidate_matches(dir, NAMES[slot], candidate)? {
                return Ok(Some((slot, candidate.generation)));
            }
        }
    }
    Ok(None)
}

pub(super) fn selected<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    verify: bool,
) -> Option<(usize, u32)>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    selected_checked(dir, verify).ok().flatten()
}

fn body_checksum<D, T, const ND: usize, const NF: usize, const NV: usize>(
    file: &File<'_, D, T, ND, NF, NV>,
    body_len: u32,
    gen: u32,
) -> Result<Option<u32>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file.seek_from_start(0).map_err(|_| ())?;
    let mut hash = 0x811c_9dc5;
    let mut bytes = [0; 512];
    let mut remaining = body_len as usize;
    while remaining > 0 {
        let take = remaining.min(bytes.len());
        if !read_record(file, &mut bytes[..take])? {
            return Ok(None);
        }
        hash = checksum_step(hash, &bytes[..take]);
        remaining -= take;
    }
    Ok(Some(checksum_step(hash, &gen.to_le_bytes())))
}

pub(super) fn write<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    body: impl FnOnce(&File<'_, D, T, ND, NF, NV>) -> Option<u32>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(selected) = selected_checked(dir, true) else {
        return false;
    };
    let (slot, gen) = selected
        .map(|(s, g)| (1 - s, g.wrapping_add(1)))
        .unwrap_or((0, 1));
    let Ok(file) = dir.open_file_in_dir(NAMES[slot], Mode::ReadWriteCreateOrTruncate) else {
        return false;
    };
    let Some(body_hash) = body(&file) else {
        return false;
    };
    if file.flush().is_err() {
        return false;
    }
    let len = file.length();
    let hash = checksum_step(body_hash, &gen.to_le_bytes());
    let mut footer = [0; FOOTER];
    footer[..4].copy_from_slice(MAGIC);
    footer[4..8].copy_from_slice(&gen.to_le_bytes());
    footer[8..12].copy_from_slice(&len.to_le_bytes());
    footer[12..].copy_from_slice(&hash.to_le_bytes());
    if file.write(&footer).is_err() || file.close().is_err() {
        return false;
    }
    generation(dir, NAMES[slot], true) == Some(gen)
}
