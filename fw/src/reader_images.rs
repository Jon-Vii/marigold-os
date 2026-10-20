//! PNG image pipeline for EPUB illustrations.
//!
//! The build flow keeps one 43 KB `InflateState` alive for the whole book
//! build, so image bytes can never be decoded while a spine's XHTML streams
//! through it. The pipeline splits the work around that constraint:
//!
//! 1. *Probe* — before any spine streams, every PNG manifest item gets a
//!    33-byte prefix read from the idle zip. The fitted on-page size is
//!    derived here and stored in a bounded table, so layout can reserve
//!    image space during the stream without touching the zip again.
//! 2. *Record* — while the XHTML streams, `<img>` elements resolve their
//!    href, look the image up in the probe table, and push an image block.
//!    Images the probe could not handle fall back to the alt-text
//!    placeholder, as before.
//! 3. *Decode* — right after a spine's stream finishes (the zip inflate is
//!    idle again), each image block the spine added is decoded. A deflated
//!    entry is first spooled compressed to the card, so the ZIP layer and
//!    the PNG's own IDAT stream reuse the one inflate state at a time; a
//!    stored entry decodes straight from raw reads with no spool. The
//!    dithered 1-bpp result lands in a sidecar next to the section files.
//! 4. *Render* — page draws stream the sidecar's packed rows into the
//!    framebuffer. A missing or mismatched sidecar draws an outlined box,
//!    which only happens for images whose decode was interrupted.
//!
//! Dithering is Bayer ordered: the pattern is a pure function of position,
//! so a static region repaints identical bits on the next fast refresh and
//! never accumulates e-ink ghosting.

use display::fb::Framebuffer;
use embedded_sdmmc::{BlockDevice, Directory, File, Mode, TimeSource};
use proto::epub::{EpubPackage, EpubZipOps, OwnedZipEntry, MAX_ENTRY_NAME_BYTES};
use proto::png::{probe_png_header, LumaSink, PngDecoder, PngError, PNG_MAX_OUTPUT_WIDTH};

/// Bound on probe-table entries: images past this render as placeholders
/// rather than costing more static memory.
pub(crate) const MAX_IMAGES_PER_BOOK: usize = 48;

/// Temp file for a compressed PNG payload between its ZIP decompression and
/// its IDAT decode. Lives in the book's SECTIONS dir; deleted after use.
const SPOOL_FILE: &str = "SPTMP.BIN";

const SIDECAR_MAGIC: [u8; 4] = *b"X4IM";
const SIDECAR_VERSION: u8 = 1;
const SIDECAR_HEADER_BYTES: usize = 12;

/// FNV-1a 64 over the resolved zip path. The 24 low bits name the sidecar
/// file, so two images of one book collide roughly once in 70k pairs.
pub(crate) fn image_hash(href: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &byte in href.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Sidecar file name for an image hash, 8.3-safe: `I` plus six hex digits.
pub(crate) fn sidecar_name(hash: u64, out: &mut heapless::String<12>) {
    out.clear();
    let _ = out.push('I');
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for shift in (0..24).step_by(4).rev() {
        let _ = out.push(HEX[((hash >> shift) & 0xF) as usize] as char);
    }
    let _ = out.push_str(".BIN");
}

/// Sidecar header: magic, version, fitted pixel size, packed row stride.
fn sidecar_header(width: u16, height: u16, stride: u16) -> [u8; SIDECAR_HEADER_BYTES] {
    let mut bytes = [0u8; SIDECAR_HEADER_BYTES];
    bytes[..4].copy_from_slice(&SIDECAR_MAGIC);
    bytes[4] = SIDECAR_VERSION;
    bytes[5..7].copy_from_slice(&width.to_le_bytes());
    bytes[7..9].copy_from_slice(&height.to_le_bytes());
    bytes[9..11].copy_from_slice(&stride.to_le_bytes());
    bytes
}

fn decode_sidecar_header(bytes: &[u8; SIDECAR_HEADER_BYTES]) -> Option<(u16, u16, u16)> {
    if bytes[..4] != SIDECAR_MAGIC || bytes[4] != SIDECAR_VERSION || bytes[11] != 0 {
        return None;
    }
    Some((
        u16::from_le_bytes(bytes[5..7].try_into().ok()?),
        u16::from_le_bytes(bytes[7..9].try_into().ok()?),
        u16::from_le_bytes(bytes[9..11].try_into().ok()?),
    ))
}

/// Probe results for every decodable manifest PNG, keyed by resolved-path
/// hash. One static: the build is single-owner by construction.
pub(crate) struct ImageTable {
    hashes: [u64; MAX_IMAGES_PER_BOOK],
    widths: [u16; MAX_IMAGES_PER_BOOK],
    heights: [u16; MAX_IMAGES_PER_BOOK],
    count: usize,
}

impl ImageTable {
    pub(crate) const fn new() -> Self {
        Self {
            hashes: [0; MAX_IMAGES_PER_BOOK],
            widths: [0; MAX_IMAGES_PER_BOOK],
            heights: [0; MAX_IMAGES_PER_BOOK],
            count: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.count = 0;
    }

    fn insert(&mut self, hash: u64, width: u16, height: u16) {
        if self.lookup(hash).is_some() || self.count == MAX_IMAGES_PER_BOOK {
            return;
        }
        self.hashes[self.count] = hash;
        self.widths[self.count] = width;
        self.heights[self.count] = height;
        self.count += 1;
    }

    pub(crate) fn lookup(&self, hash: u64) -> Option<(u16, u16)> {
        let index = self.hashes[..self.count].iter().position(|&h| h == hash)?;
        Some((self.widths[index], self.heights[index]))
    }
}

static IMAGE_TABLE: static_cell::ConstStaticCell<ImageTable> =
    static_cell::ConstStaticCell::new(ImageTable::new());

pub(crate) fn image_table() -> &'static mut ImageTable {
    IMAGE_TABLE.take()
}

/// 8×8 Bayer matrix in 0..64; a pixel turns white when its luma exceeds the
/// scaled threshold at its position.
const BAYER8: [[u8; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

fn bayer_threshold(x: usize, y: usize) -> u8 {
    ((BAYER8[y & 7][x & 7] as u32 * 255 + 32) / 64) as u8
}

/// Probes every PNG manifest item while the zip inflate is idle, filling the
/// table the stream-time `<img>` handler consults. Failures are silent: the
/// affected images fall back to placeholders.
pub(crate) fn probe_book_images<Z>(
    zip: &mut Z,
    opf_path: &str,
    package: &EpubPackage<'_>,
    header_scratch: &mut [u8; 46],
    name_scratch: &mut [u8; proto::epub::MAX_ENTRY_NAME_BYTES],
    compressed_scratch: &mut [u8],
    inflate_scratch: &mut proto::epub::ZipInflateScratch,
) where
    Z: EpubZipOps,
{
    let table = image_table();
    table.clear();
    for span in &package.image_hrefs[..package.image_count] {
        let href = span.of(package.opf_text);
        let mut resolved = heapless::String::<MAX_ENTRY_NAME_BYTES>::new();
        if super::reader_cache::resolve_epub_href(opf_path, href, &mut resolved).is_err() {
            continue;
        }
        let Ok(entry) = zip.find_entry(resolved.as_str(), header_scratch, name_scratch) else {
            continue;
        };
        // 33 bytes carry the signature and the full IHDR chunk.
        let mut prefix = [0u8; 64];
        let Ok((read, complete)) =
            zip.read_entry_prefix_streamed(entry, compressed_scratch, &mut prefix, inflate_scratch)
        else {
            continue;
        };
        if !complete || read < 33 {
            continue;
        }
        if let Ok(header) = probe_png_header(&prefix[..read]) {
            table.insert(
                image_hash(resolved.as_str()),
                header.width as u16,
                header.height as u16,
            );
        }
    }
}

/// Streams the compressed payload of `entry` through `copy`, which must
/// persist every chunk it receives. Returns whether the whole payload was
/// copied.
fn copy_entry_payload<Z>(
    zip: &mut Z,
    entry: OwnedZipEntry,
    mut offset: u32,
    chunk: &mut [u8],
    mut copy: impl FnMut(&[u8]) -> bool,
) -> bool
where
    Z: EpubZipOps,
{
    let mut remaining = entry.compressed_size;
    while remaining > 0 {
        let want = (chunk.len() as u32).min(remaining) as usize;
        let Ok(read) = zip.read_raw_at(offset, &mut chunk[..want]) else {
            return false;
        };
        if read == 0 || !copy(&chunk[..read]) {
            return false;
        }
        offset = match offset.checked_add(read as u32) {
            Some(next) => next,
            None => return false,
        };
        remaining -= read as u32;
    }
    true
}

/// Images recorded by the current spine's `<img>` handler, decoded once the
/// spine's stream is done. Bounded; overflow renders as placeholder boxes.
struct PendingImages {
    hrefs: heapless::Vec<heapless::String<96>, 6>,
    widths: [u16; 6],
    heights: [u16; 6],
}

static PENDING_IMAGES: static_cell::ConstStaticCell<PendingImages> =
    static_cell::ConstStaticCell::new(PendingImages {
        hrefs: heapless::Vec::new(),
        widths: [0; 6],
        heights: [0; 6],
    });

fn pending_images() -> &'static mut PendingImages {
    PENDING_IMAGES.take()
}

pub(crate) fn push_pending_image(href: &str, width: u16, height: u16) {
    let pending = pending_images();
    if pending.hrefs.iter().any(|seen| seen.as_str() == href) {
        return;
    }
    let mut owned = heapless::String::<96>::new();
    if owned.push_str(href).is_err() {
        return;
    }
    if pending.hrefs.push(owned).is_ok() {
        let index = pending.hrefs.len() - 1;
        pending.widths[index] = width;
        pending.heights[index] = height;
    }
}

pub(crate) fn clear_pending_images() {
    pending_images().hrefs.clear();
}

/// Decodes every pending image into its sidecar. Runs while the zip inflate
/// is idle, directly after the spine's XHTML stream. Failures leave the
/// image undecoded, where the render path draws an outlined box. Returns
/// the number of images decoded.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_pending_images<
    Z,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    zip: &mut Z,
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    header_scratch: &mut [u8; 46],
    name_scratch: &mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &mut [u8],
    window: &mut [u8],
    inflate_scratch: &mut proto::epub::ZipInflateScratch,
) -> usize
where
    Z: EpubZipOps,
    D: BlockDevice,
    T: TimeSource,
{
    let pending = pending_images();
    let mut decoded = 0;
    for index in 0..pending.hrefs.len() {
        let href = pending.hrefs[index].as_str();
        // Always re-decode: a sidecar left by an interrupted earlier build
        // can be truncated, and the decode pass overwrites it cleanly.
        if decode_one_image(
            zip,
            sections,
            href,
            pending.widths[index],
            pending.heights[index],
            header_scratch,
            name_scratch,
            compressed,
            window,
            inflate_scratch,
        ) {
            decoded += 1;
        }
    }
    pending.hrefs.clear();
    decoded
}

#[allow(clippy::too_many_arguments)]
fn decode_one_image<
    Z,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    zip: &mut Z,
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    href: &str,
    fit_w: u16,
    fit_h: u16,
    header_scratch: &mut [u8; 46],
    name_scratch: &mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &mut [u8],
    window: &mut [u8],
    inflate_scratch: &mut proto::epub::ZipInflateScratch,
) -> bool
where
    Z: EpubZipOps,
    D: BlockDevice,
    T: TimeSource,
{
    let Ok(entry) = zip.find_entry(href, header_scratch, name_scratch) else {
        return false;
    };
    if entry.compressed_size == 0 || fit_w == 0 || fit_h == 0 {
        return false;
    }
    let Some(mut file) = open_sidecar(sections, href, fit_w, fit_h) else {
        return false;
    };

    let decoded = if entry.compression_method == 0 {
        match zip.entry_data_offset(entry, header_scratch, name_scratch) {
            Ok(mut offset) => {
                let mut read_chunk = |buf: &mut [u8]| match zip.read_raw_at(offset, buf) {
                    Ok(0) | Err(_) => Err(()),
                    Ok(read) => {
                        offset = match offset.checked_add(read as u32) {
                            Some(next) => next,
                            None => return Err(()),
                        };
                        Ok(read)
                    }
                };
                let mut sink = SidecarSink {
                    file: &mut file,
                    buf: compressed,
                    fill: 0,
                    fit_w,
                    fit_h,
                    stride: (fit_w as usize).div_ceil(8),
                    rows: 0,
                };
                decode_from_source(&mut read_chunk, &mut sink, window, inflate_scratch)
            }
            Err(_) => false,
        }
    } else if spool_entry(
        zip,
        sections,
        entry,
        header_scratch,
        name_scratch,
        compressed,
    ) {
        match sections.open_file_in_dir(SPOOL_FILE, Mode::ReadOnly) {
            Ok(spool) => {
                let mut read_chunk = |buf: &mut [u8]| match spool.read(buf) {
                    Ok(0) | Err(_) => Err(()),
                    Ok(read) => Ok(read),
                };
                let mut sink = SidecarSink {
                    file: &mut file,
                    buf: compressed,
                    fill: 0,
                    fit_w,
                    fit_h,
                    stride: (fit_w as usize).div_ceil(8),
                    rows: 0,
                };
                decode_from_source(&mut read_chunk, &mut sink, window, inflate_scratch)
            }
            Err(_) => false,
        }
    } else {
        false
    };
    let _ = sections.delete_file_in_dir(SPOOL_FILE);
    decoded
}

/// Copies an entry's compressed payload into the spool file.
#[allow(clippy::too_many_arguments)]
fn spool_entry<Z, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    zip: &mut Z,
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    entry: OwnedZipEntry,
    header_scratch: &mut [u8; 46],
    name_scratch: &mut [u8; proto::epub::MAX_ENTRY_NAME_BYTES],
    chunk: &mut [u8],
) -> bool
where
    Z: EpubZipOps,
    D: BlockDevice,
    T: TimeSource,
{
    let Ok(spool) = sections.open_file_in_dir(SPOOL_FILE, Mode::ReadWriteCreateOrTruncate) else {
        return false;
    };
    let Ok(data_offset) = zip.entry_data_offset(entry, header_scratch, name_scratch) else {
        drop(spool);
        let _ = sections.delete_file_in_dir(SPOOL_FILE);
        return false;
    };
    let copied = copy_entry_payload(zip, entry, data_offset, chunk, |bytes| {
        spool.write(bytes).is_ok()
    });
    drop(spool);
    if !copied {
        let _ = sections.delete_file_in_dir(SPOOL_FILE);
    }
    copied
}

/// Opens (or truncates) the hash-named sidecar and writes its header. The
/// fitted size is known before the decode starts, so no seek-back is needed.
fn open_sidecar<'d, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    sections: &'d Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    href: &str,
    fit_w: u16,
    fit_h: u16,
) -> Option<File<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>
where
    D: BlockDevice,
    T: TimeSource,
{
    let mut name = heapless::String::<12>::new();
    sidecar_name(image_hash(href), &mut name);
    let file = sections
        .open_file_in_dir(name.as_str(), Mode::ReadWriteCreateOrTruncate)
        .ok()?;
    let stride = (fit_w as usize).div_ceil(8) as u16;
    file.write(&sidecar_header(fit_w, fit_h, stride)).ok()?;
    Some(file)
}

/// Dither-and-write sink: Bayer-packs each luma row and buffers the bytes
/// into the borrowed write buffer, flushing to the sidecar file as it fills.
struct SidecarSink<
    'a,
    'b,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: BlockDevice,
    T: TimeSource,
{
    file: &'a mut File<'b, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: &'a mut [u8],
    fill: usize,
    fit_w: u16,
    fit_h: u16,
    stride: usize,
    rows: u16,
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    SidecarSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: BlockDevice,
    T: TimeSource,
{
    fn flush_buffer(&mut self) -> bool {
        if self.fill == 0 {
            return true;
        }
        let written = self.file.write(&self.buf[..self.fill]).is_ok();
        self.fill = 0;
        written
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> LumaSink
    for SidecarSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: BlockDevice,
    T: TimeSource,
{
    fn row(&mut self, y: u32, luma: &[u8]) -> Result<(), PngError> {
        if self.rows != y as u16 || self.rows >= self.fit_h {
            return Err(PngError::Sink);
        }
        let stride = self.stride;
        if self.fill + stride > self.buf.len() && !self.flush_buffer() {
            return Err(PngError::Sink);
        }
        let row_bits = &mut self.buf[self.fill..self.fill + stride];
        row_bits.fill(0);
        for (x, &luma) in luma.iter().enumerate() {
            if x as u32 >= self.fit_w as u32 {
                break;
            }
            if luma as u32 > bayer_threshold(x, y as usize) as u32 {
                row_bits[x / 8] |= 0x80 >> (x & 7);
            }
        }
        self.fill += stride;
        self.rows += 1;
        Ok(())
    }
}

/// Feeds a byte source through the PNG decoder into the dithering sidecar
/// sink. The source returns chunk lengths; any error stops the stream.
fn decode_from_source<
    S,
    E,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    source: &mut S,
    sidecar: &mut SidecarSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    scratch: &mut [u8],
    inflate_scratch: &mut proto::epub::ZipInflateScratch,
) -> bool
where
    S: FnMut(&mut [u8]) -> Result<usize, E>,
    D: BlockDevice,
    T: TimeSource,
{
    // The decoder carves its working buffers from the idle EPUB scratch
    // window: nothing image-sized is ever a static.
    let Ok(mut decoder) =
        PngDecoder::new(scratch, u32::from(sidecar.fit_w), u32::from(sidecar.fit_h))
    else {
        return false;
    };
    let mut chunk = [0u8; 512];
    let mut ok = true;
    loop {
        match source(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                if decoder
                    .feed(inflate_scratch.state_mut(), &chunk[..read], sidecar)
                    .is_err()
                {
                    ok = false;
                    break;
                }
            }
            Err(_) => {
                ok = false;
                break;
            }
        }
    }
    ok && decoder.finish(inflate_scratch.state_mut(), sidecar).is_ok()
        && sidecar.rows == sidecar.fit_h
}

/// Streams a cached sidecar's packed rows into the framebuffer at `rect`.
/// Returns false when the sidecar is missing, corrupt, or sized differently
/// from the rect the page plan reserved.
pub(crate) fn draw_image_from_sidecar<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    href: &str,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    fb: &mut Framebuffer,
) -> bool
where
    D: BlockDevice,
    T: TimeSource,
{
    let mut name = heapless::String::<12>::new();
    sidecar_name(image_hash(href), &mut name);
    let Ok(file) = sections.open_file_in_dir(name.as_str(), Mode::ReadOnly) else {
        return false;
    };
    let mut header = [0u8; SIDECAR_HEADER_BYTES];
    if super::reader_cache_files::read_exact_file(&file, &mut header).is_err() {
        return false;
    }
    let Some((car_w, car_h, stride)) = decode_sidecar_header(&header) else {
        return false;
    };
    if car_w != width || car_h != height || stride as usize != (width as usize).div_ceil(8) {
        return false;
    }
    let (x, y) = match (i32::from(x), i32::from(y)) {
        (x, y) if x >= 0 && y >= 0 => (x as usize, y as usize),
        _ => return false,
    };
    if width == 0
        || height == 0
        || x + width as usize > fb.frame_width()
        || y + height as usize > fb.frame_height()
    {
        return false;
    }
    let stride = stride as usize;
    let mut row = [0u8; (PNG_MAX_OUTPUT_WIDTH + 7) / 8];
    for dy in 0..height as usize {
        if super::reader_cache_files::read_exact_file(&file, &mut row[..stride]).is_err() {
            return false;
        }
        for dx in 0..width as usize {
            let byte = row[dx / 8];
            let white = byte & (0x80 >> (dx & 7)) != 0;
            fb.set_pixel(x + dx, y + dy, white);
        }
    }
    true
}

/// The graceful render fallback for an image whose sidecar never arrived.
pub(crate) fn draw_image_placeholder(
    fb: &mut Framebuffer,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) {
    if width == 0 || height == 0 || x < 0 || y < 0 {
        return;
    }
    let (x, y) = (x as usize, y as usize);
    if x + width as usize > fb.frame_width() || y + height as usize > fb.frame_height() {
        return;
    }
    let right = x + width as usize - 1;
    let bottom = y + height as usize - 1;
    for dx in 0..width as usize {
        fb.set_pixel(x + dx, y, false);
        fb.set_pixel(x + dx, bottom, false);
    }
    for dy in 0..height as usize {
        fb.set_pixel(x, y + dy, false);
        fb.set_pixel(right, y + dy, false);
    }
}
