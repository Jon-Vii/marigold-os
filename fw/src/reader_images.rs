//! SD half of the PNG image pipeline for EPUB illustrations.
//!
//! The probe, the href keys, and the ZIP-entry decode live in
//! `proto::epub_images`, where host tests drive them end to end. This module
//! keeps what needs the card:
//!
//! - *Decode* — right after a spine's stream finishes (the zip inflate is
//!   idle again), each queued image decodes into a Bayer-dithered 1-bpp
//!   sidecar next to the section files. A deflated entry is inflated to a
//!   spool file first, so the ZIP layer and the PNG's IDAT stream use the
//!   one inflate state one after the other.
//! - *Render* — page draws stream the sidecar's packed rows into the
//!   framebuffer. A missing or mismatched sidecar draws an outlined box.
//!
//! Dithering is Bayer ordered: the pattern is a pure function of position,
//! so a static region repaints identical bits on the next fast refresh and
//! never accumulates e-ink ghosting.

use display::fb::Framebuffer;
use embedded_sdmmc::{BlockDevice, Directory, File, Mode, TimeSource};
use proto::epub::{EpubZipOps, MAX_ENTRY_NAME_BYTES};
use proto::epub_images::{image_hash, ImageSpool, PendingImages};
use proto::png::{LumaSink, PngError, PNG_MAX_OUTPUT_WIDTH, PNG_SCRATCH_BYTES};

use crate::reader_cache::READER_XHTML_SCRATCH;

/// Temp file for a deflated PNG's inflated bytes between the ZIP layer and
/// the IDAT decode. Lives in the book's SECTIONS dir; deleted after use.
const SPOOL_FILE: &str = "SPTMP.BIN";

const SIDECAR_MAGIC: [u8; 4] = *b"X4IM";
const SIDECAR_VERSION: u8 = 1;
const SIDECAR_HEADER_BYTES: usize = 12;

/// The decoder and the sidecar write buffer split the idle XHTML window.
/// The buffer must hold at least one packed row of the widest output.
const _: () = assert!(PNG_SCRATCH_BYTES + PNG_MAX_OUTPUT_WIDTH.div_ceil(8) <= READER_XHTML_SCRATCH);

/// Sidecar file name for an image hash, 8.3-safe: `I` plus six hex digits.
/// The 24 low bits name the file, so two images of one book collide roughly
/// once in 70k pairs.
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

/// Decodes every queued image into its sidecar. Runs while the zip inflate
/// is idle, directly after the spine's XHTML stream. A failed decode leaves
/// no sidecar, so the render path draws an outlined box. Returns the number
/// of images decoded.
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
    window: &mut [u8; READER_XHTML_SCRATCH],
    inflate_scratch: &mut proto::epub::ZipInflateScratch,
    pending: &PendingImages,
) -> usize
where
    Z: EpubZipOps,
    D: BlockDevice,
    T: TimeSource,
{
    let mut decoded = 0;
    for image in pending.as_slice() {
        // Always re-decode: a sidecar left by an interrupted earlier build
        // can be truncated, and the decode pass overwrites it cleanly.
        let ok = decode_one_image(
            zip,
            sections,
            image.path.as_str(),
            image.width,
            image.height,
            header_scratch,
            name_scratch,
            compressed,
            window,
            inflate_scratch,
        );
        if ok {
            decoded += 1;
        } else {
            esp_println::println!("epub: image {} not decoded", image.path.as_str());
        }
    }
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
    path: &str,
    width: u16,
    height: u16,
    header_scratch: &mut [u8; 46],
    name_scratch: &mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &mut [u8],
    window: &mut [u8; READER_XHTML_SCRATCH],
    inflate_scratch: &mut proto::epub::ZipInflateScratch,
) -> bool
where
    Z: EpubZipOps,
    D: BlockDevice,
    T: TimeSource,
{
    if width == 0 || height == 0 || width as usize > PNG_MAX_OUTPUT_WIDTH {
        return false;
    }
    let Ok(entry) = zip.find_entry(path, header_scratch, name_scratch) else {
        return false;
    };
    let mut name = heapless::String::<12>::new();
    sidecar_name(image_hash(path), &mut name);
    let Some(file) = open_sidecar(sections, name.as_str(), width, height) else {
        return false;
    };
    let (decoder_scratch, write_buf) = window.split_at_mut(PNG_SCRATCH_BYTES);
    let mut sink = SidecarSink {
        file: &file,
        buf: write_buf,
        fill: 0,
        width,
        height,
        stride: (width as usize).div_ceil(8),
        rows: 0,
    };
    let mut spool = SdSpool {
        dir: sections,
        file: None,
    };
    let decoded = proto::epub_images::decode_zip_png(
        zip,
        entry,
        &mut spool,
        header_scratch,
        name_scratch,
        compressed,
        decoder_scratch,
        inflate_scratch,
        width,
        height,
        &mut sink,
    )
    .is_ok()
        && sink.rows == height
        && sink.flush_buffer();
    drop(spool);
    drop(file);
    if !decoded {
        // A partial sidecar would draw a torn image; no sidecar draws the
        // outlined box instead.
        let _ = sections.delete_file_in_dir(name.as_str());
    }
    decoded
}

/// The spool file, created on the first write and deleted on drop.
struct SdSpool<
    'd,
    'v,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: BlockDevice,
    T: TimeSource,
{
    dir: &'d Directory<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    file: Option<File<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> ImageSpool
    for SdSpool<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: BlockDevice,
    T: TimeSource,
{
    fn write(&mut self, bytes: &[u8]) -> bool {
        if self.file.is_none() {
            match self
                .dir
                .open_file_in_dir(SPOOL_FILE, Mode::ReadWriteCreateOrTruncate)
            {
                Ok(file) => self.file = Some(file),
                Err(_) => return false,
            }
        }
        self.file
            .as_ref()
            .is_some_and(|file| file.write(bytes).is_ok())
    }

    fn rewind(&mut self) -> bool {
        self.file
            .as_ref()
            .is_some_and(|file| file.seek_from_start(0).is_ok())
    }

    fn read(&mut self, out: &mut [u8]) -> Option<usize> {
        self.file.as_ref()?.read(out).ok()
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> Drop
    for SdSpool<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: BlockDevice,
    T: TimeSource,
{
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            drop(file);
            let _ = self.dir.delete_file_in_dir(SPOOL_FILE);
        }
    }
}

/// Opens (or truncates) the sidecar and writes its header. The fitted size
/// is known before the decode starts, so no seek-back is needed.
fn open_sidecar<'d, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    sections: &'d Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
    width: u16,
    height: u16,
) -> Option<File<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>
where
    D: BlockDevice,
    T: TimeSource,
{
    let file = sections
        .open_file_in_dir(name, Mode::ReadWriteCreateOrTruncate)
        .ok()?;
    let stride = (width as usize).div_ceil(8) as u16;
    file.write(&sidecar_header(width, height, stride)).ok()?;
    Some(file)
}

/// Dither-and-write sink: Bayer-packs each luma row and buffers the bytes
/// into the borrowed write buffer, flushing to the sidecar file as it fills.
struct SidecarSink<
    'a,
    'f,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: BlockDevice,
    T: TimeSource,
{
    file: &'a File<'f, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: &'a mut [u8],
    fill: usize,
    width: u16,
    height: u16,
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
        if self.rows as u32 != y || self.rows >= self.height {
            return Err(PngError::Sink);
        }
        let stride = self.stride;
        if self.fill + stride > self.buf.len() && !self.flush_buffer() {
            return Err(PngError::Sink);
        }
        let row_bits = &mut self.buf[self.fill..self.fill + stride];
        row_bits.fill(0);
        for (x, &luma) in luma.iter().take(self.width as usize).enumerate() {
            if luma > bayer_threshold(x, y as usize) {
                row_bits[x / 8] |= 0x80 >> (x & 7);
            }
        }
        self.fill += stride;
        self.rows += 1;
        Ok(())
    }
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
