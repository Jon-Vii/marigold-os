//! Streaming PNG decoder: file bytes in, gray rows at a fitted size out.
//!
//! Firmware has no heap and cannot hold a decoded image, so the decoder takes
//! the file in chunks of any size, keeps one scanline, and hands a
//! [`LumaSink`] each output row as soon as its source rows are in. It borrows
//! everything it works with: buffers come from caller scratch, and the zlib
//! state is the caller's [`InflateState`] (on device, the shared ZIP
//! inflater, idle while images decode). It costs no static RAM.
//!
//! Every non-interlaced PNG decodes: gray, RGB, palette, gray+alpha and RGBA
//! at each bit depth the format allows, with tRNS. A pixel becomes luma
//! `(77 R + 151 G + 28 B + 128) >> 8`, and then its alpha composites that
//! luma onto the white page, rounded. 16-bit samples contribute their high
//! byte, but a tRNS color key matches on all 16 bits. Adam7 interlace is
//! refused, since it needs the whole image in RAM.
//!
//! Downscaling is a box filter: source column `x` lands in output column
//! `x * out_w / width` (rounded down), rows likewise, and each output pixel
//! is the rounded mean of the source pixels that land in it. The per-pixel
//! loops never divide. Column and row spans are stepped incrementally, and
//! each mean is a multiply by a reciprocal computed once per output row.
//!
//! The decode ends at its last row. Whatever follows it in the file, even
//! more image data, is skipped without being inflated, so a stream padded
//! with megabytes of zeros costs nothing extra, and neither IEND nor zlib's
//! Adler-32 is required. Chunk CRCs and the Adler-32 are never checked: that
//! is deliberate, for speed (common decoders skip the Adler-32 too). The
//! structure checks catch a damaged header, and damaged compressed data
//! still fails to inflate or leaves the image short.
//!
//! # Scratch
//!
//! From a 4-aligned start ([`PngInfo::scratch_bytes`] counts up to 3 bytes
//! to reach it):
//!
//! | bytes | holds |
//! |---|---|
//! | 256 | luma per gray level or palette index (gray up to 8 bits, palette) |
//! | 4 · out_w | `u32` sums for the output row being gathered |
//! | 4 | the current row's filter byte, in the last byte |
//! | stride rounded up to 4, + 512 | the scanline |
//!
//! The scanline buffer holds one row, not two. The row above sits 512 bytes
//! (`ROW_GAP`) to the right of the row being inflated, and zlib output lands
//! straight in the new row, a bounded step at a time. Each step is
//! unfiltered before the next is inflated, so the new row only overwrites
//! bytes of the old one that unfiltering has finished with. A finished row
//! is added into the sums and then copied right by the gap to become the row
//! above. The copy is cheap next to unfiltering, and holding one row instead
//! of two nearly halves the scratch a wide image needs: a 4096-pixel RGBA
//! image decodes in 20 KB at an output width of 800.

use super::{LumaSink, SinkError};
use miniz_oxide::inflate::stream::{self, InflateState};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

/// Widest and tallest image the decoder accepts.
pub const MAX_DIMENSION: u32 = 16_384;
/// Most pixels the decoder accepts. The cap also keeps every box sum inside
/// a `u32`: 255 · 2^24, plus rounding, is below 2^32.
pub const MAX_PIXELS: u64 = 1 << 24;
/// Bytes [`probe`] needs: the signature and the whole IHDR chunk.
pub const PROBE_BYTES: usize = 33;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];
const MAX_CHUNK_LEN: u32 = 0x7FFF_FFFF;
/// Distance from the row being inflated to the row above it.
const ROW_GAP: usize = 512;
/// Most bytes of the current row that may be inflated past the unfiltered
/// point. Unfiltering reads the row above from up to 11 bytes before that
/// point (8 bytes of context, rounded down to a word), so the inflated bytes
/// stay clear of everything in the row above that is still to be read.
const MAX_STEP: usize = ROW_GAP - 12;
const TABLE_BYTES: usize = 256;
const FILTER_SLOT: usize = 4;
const ALIGN_SLACK: usize = 3;
/// A color key no sample equals: the image has no tRNS.
const NO_KEY: u32 = 1 << 16;

/// Why a PNG did not decode. Callers show the same placeholder for every
/// variant; they exist for logs and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PngError {
    /// The signature is wrong: not a PNG.
    NotPng,
    /// The file breaks the format: bad IHDR fields, chunks out of order, an
    /// unknown filter type, a palette image without a palette.
    Corrupt,
    /// A valid PNG this decoder does not handle: Adam7 interlace, or a
    /// critical chunk it does not know.
    Unsupported,
    /// Wider or taller than [`MAX_DIMENSION`], or over [`MAX_PIXELS`].
    TooLarge,
    /// The scratch is smaller than [`PngInfo::scratch_bytes`].
    ScratchTooSmall,
    /// The output size is zero or larger than the image.
    BadOutputSize,
    /// The compressed image data is broken.
    Inflate,
    /// The file, or its image data, ended early.
    Truncated,
    /// The sink stopped the decode.
    Sink(SinkError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorType {
    Gray,
    Rgb,
    Palette,
    GrayAlpha,
    Rgba,
}

impl ColorType {
    fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Gray),
            2 => Some(Self::Rgb),
            3 => Some(Self::Palette),
            4 => Some(Self::GrayAlpha),
            6 => Some(Self::Rgba),
            _ => None,
        }
    }

    /// Samples per pixel.
    pub fn samples(self) -> u32 {
        match self {
            Self::Gray | Self::Palette => 1,
            Self::GrayAlpha => 2,
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }
}

/// What the IHDR chunk says about an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PngInfo {
    pub width: u32,
    pub height: u32,
    pub color_type: ColorType,
    pub bit_depth: u8,
    pub interlaced: bool,
}

impl PngInfo {
    /// `Ok` when [`PngDecoder`] can decode the image, or the error a decode
    /// would stop with.
    pub fn check(&self) -> Result<(), PngError> {
        if self.interlaced {
            return Err(PngError::Unsupported);
        }
        let pixels = u64::from(self.width) * u64::from(self.height);
        if self.width > MAX_DIMENSION || self.height > MAX_DIMENSION || pixels > MAX_PIXELS {
            return Err(PngError::TooLarge);
        }
        Ok(())
    }

    pub fn is_supported(&self) -> bool {
        self.check().is_ok()
    }

    /// Scratch bytes a decode to an output `out_w` pixels wide needs:
    ///
    /// `3 + 256 (gray up to 8 bits and palette only) + 4·out_w + 4
    /// + stride rounded up to 4 + 512`
    ///
    /// where stride = ⌈width · samples · bit depth / 8⌉. In 20 KB at
    /// `out_w = 800`, the widest images are: RGBA8 4190, RGBA16 2095, RGB8
    /// 5586, RGB16 2793, gray+alpha8 and gray16 8380, gray+alpha16 4190, and
    /// palette or gray up to 8 bits the full 16,384.
    pub fn scratch_bytes(&self, out_w: u32) -> usize {
        let table = if self.indexed() { TABLE_BYTES } else { 0 };
        let fixed = (ALIGN_SLACK + table + FILTER_SLOT + ROW_GAP) as u64;
        let total = fixed + 4 * u64::from(out_w) + self.stride().next_multiple_of(4);
        usize::try_from(total).unwrap_or(usize::MAX)
    }

    /// Bytes in one scanline, without its filter byte.
    fn stride(&self) -> u64 {
        let bits = u64::from(self.color_type.samples()) * u64::from(self.bit_depth);
        (u64::from(self.width) * bits).div_ceil(8)
    }

    /// Whether pixels map to luma through the 256-entry table.
    fn indexed(&self) -> bool {
        self.color_type == ColorType::Palette
            || (self.color_type == ColorType::Gray && self.bit_depth <= 8)
    }
}

/// Reads the signature and IHDR from the first [`PROBE_BYTES`] of a file
/// without decoding anything. An interlaced or oversized image probes fine;
/// [`PngInfo::check`] says whether it will decode.
pub fn probe(prefix: &[u8]) -> Result<PngInfo, PngError> {
    let Some(head) = prefix.first_chunk::<PROBE_BYTES>() else {
        let wrong = prefix
            .first_chunk::<8>()
            .is_some_and(|start| *start != SIGNATURE);
        return Err(if wrong {
            PngError::NotPng
        } else {
            PngError::Truncated
        });
    };
    let (signature, rest) = head.split_at(8);
    if signature != SIGNATURE {
        return Err(PngError::NotPng);
    }
    let (length, rest) = rest.split_at(4);
    let (kind, rest) = rest.split_at(4);
    if length != [0, 0, 0, 13] || kind != b"IHDR" {
        return Err(PngError::Corrupt);
    }
    let Some(fields) = rest.first_chunk::<13>() else {
        return Err(PngError::Truncated);
    };
    parse_header(fields)
}

/// Validates IHDR's fields against the format. Says nothing about whether
/// the decoder supports the image; see [`PngInfo::check`].
fn parse_header(data: &[u8; 13]) -> Result<PngInfo, PngError> {
    let [w0, w1, w2, w3, h0, h1, h2, h3, bit_depth, color, compression, filter, interlace] = *data;
    let width = u32::from_be_bytes([w0, w1, w2, w3]);
    let height = u32::from_be_bytes([h0, h1, h2, h3]);
    let color_type = ColorType::from_code(color).ok_or(PngError::Corrupt)?;
    let depth_ok = match color_type {
        ColorType::Gray => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        ColorType::Palette => matches!(bit_depth, 1 | 2 | 4 | 8),
        _ => matches!(bit_depth, 8 | 16),
    };
    if width == 0
        || height == 0
        || width > MAX_CHUNK_LEN
        || height > MAX_CHUNK_LEN
        || !depth_ok
        || compression != 0
        || filter != 0
        || interlace > 1
    {
        return Err(PngError::Corrupt);
    }
    Ok(PngInfo {
        width,
        height,
        color_type,
        bit_depth,
        interlaced: interlace == 1,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Signature,
    ChunkHeader,
    ChunkData,
    ChunkCrc,
    /// IEND arrived or every output row went to the sink. Anything after is
    /// ignored.
    Done,
}

/// What the current chunk's data is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chunk {
    Header,
    Palette,
    Transparency,
    ImageData,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageData {
    Before,
    Inside,
    After,
}

/// The pixel layout, picked once so the per-pixel loop never branches on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pixels {
    /// Gray up to 8 bits, or palette: through the table. Holds the depth.
    Indexed(u8),
    Gray16,
    GrayAlpha8,
    GrayAlpha16,
    Rgb8,
    Rgb16,
    Rgba8,
    Rgba16,
}

/// Splits `total` source pixels into `count` consecutive spans in the
/// pattern `floor(x * count / total)` gives, one span at a time and without
/// dividing: each span is `total / count` pixels, or one more.
#[derive(Clone, Copy, Debug)]
struct Spans {
    short: u32,
    extra: u32,
    count: u32,
    /// `x * count - span * total` at the next span's first pixel `x`: in
    /// `0..count` at all times.
    error: u32,
}

impl Spans {
    fn new(total: u32, count: u32) -> Self {
        let count = count.max(1);
        Self {
            short: total / count,
            extra: total % count,
            count,
            error: 0,
        }
    }

    /// Whether the next span is the longer one, advancing past it.
    #[inline(always)]
    fn next_is_long(&mut self) -> bool {
        let long = self.error < self.extra;
        // Never underflows: a short span leaves `error >= extra`, and a
        // long one adds `count > extra` first.
        self.error = self.error + if long { self.count } else { 0 } - self.extra;
        long
    }

    #[inline(always)]
    fn next_len(&mut self) -> u32 {
        self.short + u32::from(self.next_is_long())
    }
}

/// Decodes one PNG, fed in chunks of any size, to `out_w × out_h` gray rows.
///
/// Feed the file from its first byte with [`PngDecoder::feed`], then call
/// [`PngDecoder::finish`]. Rows reach the sink as soon as their source rows
/// are in. The first error ends the decode, and every later call returns
/// it again.
pub struct PngDecoder<'a> {
    scratch: &'a mut [u8],
    out_w: u32,
    out_h: u32,
    failed: Option<PngError>,
    info: Option<PngInfo>,

    phase: Phase,
    /// Bytes gathered so far in `held`, or matched of the signature, or
    /// skipped of a CRC.
    held_len: usize,
    /// A chunk header, or the data of IHDR or a gray or RGB tRNS.
    held: [u8; 13],
    chunk: Chunk,
    chunk_len: u32,
    chunk_left: u32,
    image_data: ImageData,
    /// The zlib stream ended.
    stream_done: bool,
    palette_entries: u32,
    seen_palette: bool,
    seen_transparency: bool,
    /// Gray or RGB color key, [`NO_KEY`] when there is none.
    key: [u32; 3],

    pixels: Pixels,
    table_at: usize,
    sums_at: usize,
    /// Start of the current row's bytes; its filter byte is just before.
    row_at: usize,
    stride: usize,
    /// Bytes per complete pixel, at least 1: the filters' left distance.
    bpp: usize,
    /// Unfiltering stops on multiples of this: whole words for the word-wide
    /// filters and whole pixels for the per-pixel ones (4, 8 or 12).
    unit: usize,
    /// Bytes of the current row inflated so far, filter byte included.
    fill: usize,
    /// Bytes of the current row unfiltered so far.
    clean: usize,
    src_y: u32,
    height: u32,
    /// Source columns per output column. Copied fresh for every row.
    cols: Spans,
    /// Source rows per output row, advanced once per output row.
    rows: Spans,
    /// [`reciprocal`] of every count a box can have: by short then long
    /// band of rows, by short then long span of columns.
    inverses: [[u32; 2]; 2],
    band_rows: u32,
    band_left: u32,
    out_y: u32,
    /// Inflate calls and bytes they wrote, so tests can bound the work.
    #[cfg(test)]
    inflate_work: (u32, u64),
}

impl<'a> PngDecoder<'a> {
    /// A decoder emitting exactly `out_w × out_h`, a size the caller already
    /// fitted (see [`super::fit_inside`]); it is never refitted. Buffers are
    /// carved from `scratch` once the header arrives, and everything the
    /// decoder reads is cleared first, so the scratch may hold anything.
    pub fn new(scratch: &'a mut [u8], out_w: u32, out_h: u32) -> Result<Self, PngError> {
        if out_w == 0 || out_h == 0 {
            return Err(PngError::BadOutputSize);
        }
        Ok(Self {
            scratch,
            out_w,
            out_h,
            failed: None,
            info: None,
            phase: Phase::Signature,
            held_len: 0,
            held: [0; 13],
            chunk: Chunk::Skip,
            chunk_len: 0,
            chunk_left: 0,
            image_data: ImageData::Before,
            stream_done: false,
            palette_entries: 0,
            seen_palette: false,
            seen_transparency: false,
            key: [NO_KEY; 3],
            pixels: Pixels::Indexed(8),
            table_at: 0,
            sums_at: 0,
            row_at: 0,
            stride: 0,
            bpp: 1,
            unit: 4,
            fill: 0,
            clean: 0,
            src_y: 0,
            height: 0,
            cols: Spans::new(1, 1),
            rows: Spans::new(1, 1),
            inverses: [[0; 2]; 2],
            band_rows: 0,
            band_left: 0,
            out_y: 0,
            #[cfg(test)]
            inflate_work: (0, 0),
        })
    }

    /// The image's header, once it has been fed.
    pub fn info(&self) -> Option<PngInfo> {
        self.info
    }

    /// Takes the next bytes of the file. The decoder resets `inflate` to zlib
    /// when the image data starts, so its earlier state does not matter.
    ///
    /// The work a call does is bounded by the bytes it is given: deflate
    /// expands at most about 1,032 times. A caller that checks for input
    /// between calls therefore bounds how long a cancel waits by its chunk
    /// size, even where the sink is called rarely (a tall image scaled to a
    /// few rows).
    pub fn feed(
        &mut self,
        inflate: &mut InflateState,
        bytes: &[u8],
        sink: &mut impl LumaSink,
    ) -> Result<(), PngError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.take(inflate, bytes, sink);
        self.failed = result.err();
        result
    }

    /// Ends the decode: flushes anything the inflater still holds, then
    /// succeeds if every output row went to the sink. Rows the sink took are
    /// provisional until this returns `Ok`.
    pub fn finish(
        &mut self,
        inflate: &mut InflateState,
        sink: &mut impl LumaSink,
    ) -> Result<(), PngError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.complete(inflate, sink);
        self.failed = result.err();
        result
    }

    /// Whether every output row has gone to the sink. Once it has, the rest
    /// of the file need not be read: feeding it does nothing.
    pub fn is_complete(&self) -> bool {
        self.info.is_some() && self.out_y >= self.out_h
    }

    fn complete(
        &mut self,
        inflate: &mut InflateState,
        sink: &mut dyn LumaSink,
    ) -> Result<(), PngError> {
        if !self.is_complete() && self.image_data != ImageData::Before && !self.stream_done {
            // Feeding already drains the inflater after each input, so this
            // finds nothing in a well-formed file.
            self.inflate_rows(inflate, &[], MZFlush::Finish, sink)?;
        }
        if self.is_complete() {
            return Ok(());
        }
        if self.phase == Phase::Done && self.image_data == ImageData::Before {
            // IEND with no image data at all.
            return Err(PngError::Corrupt);
        }
        Err(PngError::Truncated)
    }

    fn take(
        &mut self,
        inflate: &mut InflateState,
        mut bytes: &[u8],
        sink: &mut dyn LumaSink,
    ) -> Result<(), PngError> {
        while !bytes.is_empty() {
            let used = match self.phase {
                Phase::Signature => self.take_signature(bytes)?,
                Phase::ChunkHeader => {
                    let used = self.hold(bytes, 8);
                    if self.held_len == 8 {
                        self.begin_chunk(inflate)?;
                    }
                    used
                }
                Phase::ChunkData => {
                    let used = bytes.len().min(self.chunk_left as usize);
                    let data = bytes.get(..used).unwrap_or_default();
                    self.chunk_data(inflate, data, sink)?;
                    if self.is_complete() {
                        // The last row went out mid-chunk.
                        return Ok(());
                    }
                    self.chunk_left -= used as u32;
                    if self.chunk_left == 0 {
                        self.end_chunk()?;
                    }
                    used
                }
                Phase::ChunkCrc => {
                    // Skipped unchecked; see the module notes.
                    let used = bytes.len().min(4 - self.held_len);
                    self.held_len += used;
                    if self.held_len == 4 {
                        self.held_len = 0;
                        self.phase = Phase::ChunkHeader;
                    }
                    used
                }
                Phase::Done => return Ok(()),
            };
            if used == 0 {
                // Every phase takes at least one byte; stop rather than spin
                // if that ever fails to hold.
                return Err(PngError::Corrupt);
            }
            bytes = bytes.get(used..).unwrap_or_default();
        }
        Ok(())
    }

    fn take_signature(&mut self, bytes: &[u8]) -> Result<usize, PngError> {
        let expected = SIGNATURE.get(self.held_len..).unwrap_or_default();
        let used = bytes.len().min(expected.len());
        if bytes.get(..used) != expected.get(..used) {
            return Err(PngError::NotPng);
        }
        self.held_len += used;
        if self.held_len == SIGNATURE.len() {
            self.held_len = 0;
            self.phase = Phase::ChunkHeader;
        }
        Ok(used)
    }

    /// Gathers bytes into `held` until it has `want`; returns how many it took.
    fn hold(&mut self, bytes: &[u8], want: usize) -> usize {
        let room = self.held.get_mut(self.held_len..want).unwrap_or_default();
        let used = room.len().min(bytes.len());
        room[..used].copy_from_slice(&bytes[..used]);
        self.held_len += used;
        used
    }

    fn begin_chunk(&mut self, inflate: &mut InflateState) -> Result<(), PngError> {
        let [l0, l1, l2, l3, k0, k1, k2, k3, ..] = self.held;
        let len = u32::from_be_bytes([l0, l1, l2, l3]);
        let kind = [k0, k1, k2, k3];
        self.held_len = 0;
        if len > MAX_CHUNK_LEN || !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err(PngError::Corrupt);
        }
        if self.info.is_none() && kind != *b"IHDR" {
            return Err(PngError::Corrupt);
        }
        if kind != *b"IDAT" && self.image_data == ImageData::Inside {
            self.image_data = ImageData::After;
        }
        self.chunk = match &kind {
            b"IHDR" if self.info.is_none() && len == 13 => Chunk::Header,
            b"IHDR" => return Err(PngError::Corrupt),
            b"PLTE" => self.begin_palette(len)?,
            b"tRNS" => self.begin_transparency(len),
            b"IDAT" => {
                self.begin_image_data(inflate)?;
                Chunk::ImageData
            }
            b"IEND" => {
                self.phase = Phase::Done;
                return Ok(());
            }
            // Bit 5 of the first letter clear: critical, so it cannot be
            // skipped.
            _ if kind[0].is_ascii_uppercase() => return Err(PngError::Unsupported),
            _ => Chunk::Skip,
        };
        self.chunk_len = len;
        self.chunk_left = len;
        self.phase = Phase::ChunkData;
        if len == 0 {
            self.end_chunk()?;
        }
        Ok(())
    }

    fn color_type(&self) -> Option<ColorType> {
        self.info.map(|info| info.color_type)
    }

    fn begin_palette(&mut self, len: u32) -> Result<Chunk, PngError> {
        // Only a palette image uses PLTE. Others may carry a suggested
        // palette, which a gray page has no use for.
        if self.color_type() != Some(ColorType::Palette) || self.image_data != ImageData::Before {
            return Ok(Chunk::Skip);
        }
        if self.seen_palette || len == 0 || !len.is_multiple_of(3) || len > 3 * 256 {
            return Err(PngError::Corrupt);
        }
        self.seen_palette = true;
        self.palette_entries = len / 3;
        Ok(Chunk::Palette)
    }

    /// Ignores a tRNS that is out of place, repeated, or the wrong size, as
    /// common decoders do, rather than failing the image over it.
    fn begin_transparency(&mut self, len: u32) -> Chunk {
        if self.image_data != ImageData::Before || self.seen_transparency {
            return Chunk::Skip;
        }
        let fits = match self.color_type() {
            Some(ColorType::Palette) => self.seen_palette && len <= self.palette_entries,
            Some(ColorType::Gray) => len == 2,
            Some(ColorType::Rgb) => len == 6,
            _ => false,
        };
        if !fits {
            return Chunk::Skip;
        }
        self.seen_transparency = true;
        Chunk::Transparency
    }

    fn begin_image_data(&mut self, inflate: &mut InflateState) -> Result<(), PngError> {
        match self.image_data {
            ImageData::Inside => {}
            // IDAT chunks must be consecutive.
            ImageData::After => return Err(PngError::Corrupt),
            ImageData::Before => {
                if self.color_type() == Some(ColorType::Palette) && !self.seen_palette {
                    return Err(PngError::Corrupt);
                }
                // The decode stops at its last row, before any checksum.
                inflate.reset(DataFormat::ZLibIgnoreChecksum);
                self.image_data = ImageData::Inside;
            }
        }
        Ok(())
    }

    fn chunk_data(
        &mut self,
        inflate: &mut InflateState,
        data: &[u8],
        sink: &mut dyn LumaSink,
    ) -> Result<(), PngError> {
        let at = (self.chunk_len - self.chunk_left) as usize;
        match self.chunk {
            Chunk::Header => {
                self.hold(data, 13);
            }
            Chunk::Palette => {
                // Borrows only the scratch, leaving `held` free for the
                // entry being gathered.
                let table = self
                    .scratch
                    .get_mut(self.table_at..self.table_at + TABLE_BYTES)
                    .ok_or(PngError::Corrupt)?;
                for (offset, &byte) in data.iter().enumerate() {
                    let position = at + offset;
                    let lane = position % 3;
                    if let Some(slot) = self.held.get_mut(lane) {
                        *slot = byte;
                    }
                    if lane == 2 {
                        let [r, g, b, ..] = self.held;
                        let value = luma(u32::from(r), u32::from(g), u32::from(b));
                        if let Some(entry) = table.get_mut(position / 3) {
                            *entry = value as u8;
                        }
                    }
                }
            }
            Chunk::Transparency if self.color_type() == Some(ColorType::Palette) => {
                // Entry i takes alpha i; entries past the chunk stay opaque.
                let table = self.table_mut()?;
                let entries = table.get_mut(at..).unwrap_or_default();
                for (entry, &alpha) in entries.iter_mut().zip(data) {
                    *entry = over_white(u32::from(*entry), u32::from(alpha)) as u8;
                }
            }
            Chunk::Transparency => {
                self.hold(data, 6);
            }
            Chunk::ImageData => self.inflate_rows(inflate, data, MZFlush::None, sink)?,
            Chunk::Skip => {}
        }
        Ok(())
    }

    fn end_chunk(&mut self) -> Result<(), PngError> {
        match self.chunk {
            Chunk::Header => self.start()?,
            Chunk::Transparency => self.set_color_key()?,
            _ => {}
        }
        self.held_len = 0;
        self.phase = Phase::ChunkCrc;
        Ok(())
    }

    fn table_mut(&mut self) -> Result<&mut [u8], PngError> {
        self.scratch
            .get_mut(self.table_at..self.table_at + TABLE_BYTES)
            .ok_or(PngError::Corrupt)
    }

    /// Applies a gray or RGB tRNS. Below 16 bits only the low byte of each
    /// key sample counts, as in common decoders.
    fn set_color_key(&mut self) -> Result<(), PngError> {
        let Some(info) = self.info else {
            return Err(PngError::Corrupt);
        };
        let held = self.held;
        let sample = |i: usize| {
            let value = u32::from(u16::from_be_bytes([held[2 * i], held[2 * i + 1]]));
            if info.bit_depth == 16 {
                value
            } else {
                value & 0xFF
            }
        };
        match info.color_type {
            ColorType::Gray if info.bit_depth <= 8 => {
                let level = sample(0) as usize;
                if let Some(entry) = self.table_mut()?.get_mut(level) {
                    *entry = 255;
                }
            }
            ColorType::Gray => self.key = [sample(0), NO_KEY, NO_KEY],
            ColorType::Rgb => self.key = [sample(0), sample(1), sample(2)],
            _ => {}
        }
        Ok(())
    }

    /// IHDR is in: checks the image against the limits and the scratch, then
    /// carves and clears the buffers.
    fn start(&mut self) -> Result<(), PngError> {
        let info = parse_header(&self.held)?;
        info.check()?;
        if self.out_w > info.width || self.out_h > info.height {
            return Err(PngError::BadOutputSize);
        }
        if self.scratch.len() < info.scratch_bytes(self.out_w) {
            return Err(PngError::ScratchTooSmall);
        }
        // `check` capped the width, so these sizes all fit easily.
        let stride = info.stride() as usize;
        let base = bytemuck::pod_align_to::<u8, u32>(self.scratch).0.len();
        let table = if info.indexed() { TABLE_BYTES } else { 0 };
        self.table_at = base;
        self.sums_at = base + table;
        self.row_at = self.sums_at + 4 * self.out_w as usize + FILTER_SLOT;
        let end = self.row_at + stride.next_multiple_of(4) + ROW_GAP;
        let Some(carved) = self.scratch.get_mut(base..end) else {
            return Err(PngError::ScratchTooSmall);
        };
        // Clearing all of it covers the sums and the row above row 0.
        carved.fill(0);
        if info.color_type == ColorType::Gray && info.bit_depth <= 8 {
            // Gray levels stretch to 0..=255: 255 / (2^depth - 1) per level.
            let (levels, scale) = match info.bit_depth {
                1 => (2, 255),
                2 => (4, 85),
                4 => (16, 17),
                _ => (256, 1),
            };
            let table = self.table_mut()?;
            for (level, entry) in table.iter_mut().take(levels).enumerate() {
                *entry = (level * scale) as u8;
            }
        }
        // A palette table stays zero: entries the palette lacks are black.

        let bits = info.color_type.samples() * u32::from(info.bit_depth);
        self.bpp = (bits as usize / 8).max(1);
        self.unit = match self.bpp {
            3 | 6 => 12,
            8 => 8,
            _ => 4,
        };
        self.stride = stride;
        self.height = info.height;
        self.pixels = match (info.color_type, info.bit_depth) {
            (ColorType::Gray | ColorType::Palette, depth) if depth <= 8 => Pixels::Indexed(depth),
            (ColorType::Gray, _) => Pixels::Gray16,
            (ColorType::GrayAlpha, 8) => Pixels::GrayAlpha8,
            (ColorType::GrayAlpha, _) => Pixels::GrayAlpha16,
            (ColorType::Rgb, 8) => Pixels::Rgb8,
            (ColorType::Rgb, _) => Pixels::Rgb16,
            (ColorType::Rgba, 8) => Pixels::Rgba8,
            (_, _) => Pixels::Rgba16,
        };
        self.cols = Spans::new(info.width, self.out_w);
        self.rows = Spans::new(info.height, self.out_h);
        // The only divisions of the decode, four per image. Each count is
        // at most 16,385², so none overflows.
        let (cols, rows) = (self.cols.short, self.rows.short);
        self.inverses = [
            [reciprocal(cols * rows), reciprocal((cols + 1) * rows)],
            [
                reciprocal(cols * (rows + 1)),
                reciprocal((cols + 1) * (rows + 1)),
            ],
        ];
        self.band_rows = self.rows.next_len();
        self.band_left = self.band_rows;
        self.info = Some(info);
        Ok(())
    }

    /// Runs the inflater over `input` until it is consumed and the inflater
    /// has nothing more to give, handing every completed row on. Stops as
    /// soon as the last row is out.
    fn inflate_rows(
        &mut self,
        inflate: &mut InflateState,
        mut input: &[u8],
        flush: MZFlush,
        sink: &mut dyn LumaSink,
    ) -> Result<(), PngError> {
        // Not the phase: IEND can arrive before `finish` flushes the rest.
        while !self.stream_done && !self.is_complete() {
            let (from, to) = self.inflate_target();
            let Some(out) = self.scratch.get_mut(from..to) else {
                return Err(PngError::Corrupt);
            };
            let result = stream::inflate(inflate, input, out, flush);
            #[cfg(test)]
            {
                self.inflate_work.0 += 1;
                self.inflate_work.1 += result.bytes_written as u64;
            }
            input = input.get(result.bytes_consumed..).unwrap_or_default();
            let progress = result.bytes_consumed > 0 || result.bytes_written > 0;
            if result.bytes_written > 0 {
                self.inflated(result.bytes_written, sink)?;
                if self.is_complete() {
                    // Whatever the inflater says about the stream past the
                    // last row does not matter, however the file was cut.
                    return Ok(());
                }
            }
            match result.status {
                Ok(MZStatus::StreamEnd) => self.stream_done = true,
                // `Buf` is miniz for "no room or no input", not an error.
                Ok(_) | Err(MZError::Buf) if progress => {}
                Ok(_) | Err(MZError::Buf) if input.is_empty() => break,
                _ => return Err(PngError::Inflate),
            }
        }
        Ok(())
    }

    /// Where the next inflate call writes: on from the current row's filter
    /// byte, no more than [`MAX_STEP`] past the unfiltered point, and never
    /// past the row. Never empty, because every write is unfiltered to
    /// within a unit (at most 11 bytes) of its end.
    fn inflate_target(&self) -> (usize, usize) {
        let limit = (self.clean + MAX_STEP).min(self.stride);
        (self.row_at - 1 + self.fill, self.row_at + limit)
    }

    fn inflated(&mut self, written: usize, sink: &mut dyn LumaSink) -> Result<(), PngError> {
        let first = self.fill == 0;
        self.fill += written;
        let filter = self.scratch.get(self.row_at - 1).copied().unwrap_or(0xFF);
        if first && filter > 4 {
            return Err(PngError::Corrupt);
        }
        let have = self.fill - 1;
        let complete = have >= self.stride;
        // Per inflate call, not per pixel. (`unit` is never 0.)
        let upto = if complete {
            self.stride
        } else {
            have - have % self.unit.max(1)
        };
        if upto > self.clean {
            self.unfilter(filter, upto)?;
            self.clean = upto;
        }
        if complete {
            self.row_done(sink)?;
        }
        Ok(())
    }

    /// Unfilters the current row from `clean` up to `upto`. The window
    /// passed down starts a little before `clean` so the filters can see
    /// their left neighbours, and the row above's window is the same columns
    /// [`ROW_GAP`] further on, which the step limit keeps disjoint and
    /// intact.
    fn unfilter(&mut self, filter: u8, upto: usize) -> Result<(), PngError> {
        let low = self.clean.saturating_sub(8) & !3;
        let Some((head, tail)) = self
            .scratch
            .split_at_mut_checked(self.row_at + ROW_GAP + low)
        else {
            return Err(PngError::Corrupt);
        };
        let (Some(current), Some(above)) = (
            head.get_mut(self.row_at + low..self.row_at + upto),
            tail.get(..upto - low),
        ) else {
            return Err(PngError::Corrupt);
        };
        unfilter_window(filter, self.bpp, current, above, self.clean - low);
        Ok(())
    }

    /// The current row is complete and unfiltered: adds its luma into the
    /// sums, makes it the row above, and emits an output row when its band
    /// of source rows is done.
    fn row_done(&mut self, sink: &mut dyn LumaSink) -> Result<(), PngError> {
        let out_w = self.out_w as usize;
        let Some((front, back)) = self.scratch.split_at_mut_checked(self.row_at) else {
            return Err(PngError::Corrupt);
        };
        let (Some(row), Some((table, sums))) = (
            back.get(..self.stride),
            front.split_at_mut_checked(self.sums_at),
        ) else {
            return Err(PngError::Corrupt);
        };
        let table = table.get(self.table_at..).unwrap_or_default();
        let sums = sums.get_mut(..4 * out_w).ok_or(PngError::Corrupt)?;
        let sums: &mut [u32] = bytemuck::try_cast_slice_mut(sums).map_err(|_| PngError::Corrupt)?;
        accumulate(self.pixels, row, sums, self.cols, table, self.key);

        // The finished row moves on by the gap to become the row above. A
        // region of exactly gap + stride bytes lets the compiler see that the
        // copy stays inside it.
        let span = self.stride.checked_add(ROW_GAP).ok_or(PngError::Corrupt)?;
        let region = self
            .scratch
            .get_mut(self.row_at..)
            .and_then(|rest| rest.get_mut(..span))
            .ok_or(PngError::Corrupt)?;
        shift_row(region, self.stride)?;
        self.fill = 0;
        self.clean = 0;
        self.src_y += 1;
        self.band_left = self.band_left.saturating_sub(1);
        if self.band_left == 0 && self.out_y < self.out_h {
            self.emit_row(sink)?;
        }
        Ok(())
    }

    /// Turns the sums into means, in place over the sums' first bytes, hands
    /// the row to the sink, and clears the sums for the next band.
    fn emit_row(&mut self, sink: &mut dyn LumaSink) -> Result<(), PngError> {
        let out_w = self.out_w as usize;
        let Some(sums) = self
            .scratch
            .get_mut(self.sums_at..self.sums_at + 4 * out_w)
            .and_then(|bytes| bytemuck::try_cast_slice_mut::<u8, u32>(bytes).ok())
        else {
            return Err(PngError::Corrupt);
        };
        // Every pixel in this row averages `band_rows` source rows by a
        // short or long span of columns.
        let short = self.cols.short * self.band_rows;
        let long = short + self.band_rows;
        let [short_inverse, long_inverse] = if self.band_rows > self.rows.short {
            self.inverses[1]
        } else {
            self.inverses[0]
        };
        let mut cols = self.cols;
        let mut mean = |sum: u32| {
            let (count, inverse) = if cols.next_is_long() {
                (long, long_inverse)
            } else {
                (short, short_inverse)
            };
            divide(sum + count / 2, count, inverse) as u8
        };
        // Means pack four to a word over sums already read: word `w` takes
        // the means of sums 4w to 4w + 3, so mean k lands on byte k.
        for w in 0..out_w.div_ceil(4) {
            let mut word = [0u8; 4];
            for (lane, byte) in word.iter_mut().enumerate() {
                if let Some(&sum) = sums.get(4 * w + lane) {
                    *byte = mean(sum);
                }
            }
            if let Some(slot) = sums.get_mut(w) {
                *slot = u32::from_ne_bytes(word);
            }
        }
        let row = bytemuck::cast_slice::<u32, u8>(sums);
        sink.row(self.out_y, row.get(..out_w).unwrap_or_default())
            .map_err(PngError::Sink)?;
        sums.fill(0);
        self.out_y += 1;
        if self.out_y < self.out_h {
            self.band_rows = self.rows.next_len();
            self.band_left = self.band_rows;
        } else {
            // Nothing after the last row is inflated or even parsed.
            self.phase = Phase::Done;
        }
        Ok(())
    }
}

/// Copies `region[..stride]` to `region[ROW_GAP..]`, where `region` is
/// exactly `stride + ROW_GAP` bytes and starts on a word. The two overlap
/// once the stride passes the gap, so the copy runs from the top down:
/// first the bytes past the last whole word, which land beyond every source
/// byte, then whole words a gap's worth at a time. Each block's destination
/// lies above every word still to move, so it splits off cleanly.
///
/// `copy_within` would call `memmove`, which on device is the ROM's and
/// goes a byte at a time when copying backward.
fn shift_row(region: &mut [u8], stride: usize) -> Result<(), PngError> {
    const GAP_WORDS: usize = ROW_GAP / 4;
    let whole = stride & !3;
    for at in (whole..stride).rev() {
        let byte = region.get(at).copied().unwrap_or(0);
        if let Some(slot) = region.get_mut(at + ROW_GAP) {
            *slot = byte;
        }
    }
    let words = region
        .get_mut(..whole + ROW_GAP)
        .and_then(|bytes| bytemuck::try_cast_slice_mut::<u8, u32>(bytes).ok())
        .ok_or(PngError::Corrupt)?;
    let mut end = whole / 4;
    while end > 0 {
        let start = end.saturating_sub(GAP_WORDS);
        let Some((low, high)) = words.split_at_mut_checked(end) else {
            return Err(PngError::Corrupt);
        };
        let (Some(from), Some(to)) = (
            low.get(start..),
            high.get_mut(start + GAP_WORDS - end..GAP_WORDS),
        ) else {
            return Err(PngError::Corrupt);
        };
        copy_words(to, from);
        end = start;
    }
    Ok(())
}

/// `to.copy_from_slice(from)` for equal lengths, with two single-word
/// stores per step: the compiler turns a plain copy loop back into a call to
/// `memcpy`, which on device is the ROM's.
#[inline(always)]
fn copy_words(to: &mut [u32], from: &[u32]) {
    let (to_pairs, to_rest) = to.as_chunks_mut::<2>();
    let (from_pairs, from_rest) = from.as_chunks::<2>();
    for (to, from) in to_pairs.iter_mut().zip(from_pairs) {
        to[1] = from[1];
        to[0] = from[0];
    }
    for (to, &from) in to_rest.iter_mut().zip(from_rest) {
        *to = from;
    }
}

/// Luma of an 8-bit RGB triple. The weights sum to 256, so white stays 255.
#[inline(always)]
fn luma(r: u32, g: u32, b: u32) -> u32 {
    (77 * r + 151 * g + 28 * b + 128) >> 8
}

/// `value` at opacity `alpha` over white: `round(value·alpha / 255) + 255 -
/// alpha`, which equals `round((value·alpha + 255·(255 - alpha)) / 255)`.
/// The `/ 255` is Blinn's exact rounding form for products of two bytes.
#[inline(always)]
fn over_white(value: u32, alpha: u32) -> u32 {
    let t = value * alpha + 128;
    ((t + (t >> 8)) >> 8) + 255 - alpha
}

/// `(2^32 - 1) / d`, for [`divide`]: `floor(2^32 / d)`, or one less when `d`
/// is a power of two. Either way it is at least `2^32 / d - 1`, which is all
/// `divide` needs.
fn reciprocal(d: u32) -> u32 {
    u32::MAX / d.max(1)
}

/// `n / d` from `inverse = reciprocal(d)`, exact for every `u32` numerator.
/// The high word of `n · inverse` (one `mulhu` on RV32) is at most `n / d`
/// and, since `inverse >= 2^32 / d - 1`, at least `n / d - n / 2^32`, which
/// is more than `n / d - 1`: the quotient or one less. One comparison fixes
/// that.
#[inline(always)]
fn divide(n: u32, d: u32, inverse: u32) -> u32 {
    let q = ((u64::from(n) * u64::from(inverse)) >> 32) as u32;
    q + u32::from(n - q * d >= d)
}

/// Adds one unfiltered source row's luma into the output row's sums.
fn accumulate(
    pixels: Pixels,
    row: &[u8],
    sums: &mut [u32],
    cols: Spans,
    table: &[u8],
    key: [u32; 3],
) {
    let [k0, k1, k2] = key;
    let be = |hi: u8, lo: u8| u32::from(u16::from_be_bytes([hi, lo]));
    let byte = u32::from;
    match pixels {
        Pixels::Indexed(depth) => {
            // The indexed layouts always have a table.
            let Some(table) = table.first_chunk::<TABLE_BYTES>() else {
                return;
            };
            if depth == 8 {
                sum_spans(row, sums, cols, |&[i]: &[u8; 1]| {
                    byte(table[usize::from(i)])
                });
            } else {
                sum_packed(row, depth, sums, cols, table);
            }
        }
        Pixels::Gray16 if k0 == NO_KEY => sum_spans(row, sums, cols, |&[hi, _]: &[u8; 2]| byte(hi)),
        Pixels::Gray16 => sum_spans(row, sums, cols, |&[hi, lo]: &[u8; 2]| {
            if be(hi, lo) == k0 {
                255
            } else {
                byte(hi)
            }
        }),
        Pixels::GrayAlpha8 => sum_spans(row, sums, cols, |&[v, a]: &[u8; 2]| {
            over_white(byte(v), byte(a))
        }),
        Pixels::GrayAlpha16 => sum_spans(row, sums, cols, |&[v, _, a, _]: &[u8; 4]| {
            over_white(byte(v), byte(a))
        }),
        // The keyed forms compare every pixel, so images without tRNS skip
        // them.
        Pixels::Rgb8 if k0 == NO_KEY => sum_spans(row, sums, cols, |&[r, g, b]: &[u8; 3]| {
            luma(byte(r), byte(g), byte(b))
        }),
        Pixels::Rgb8 => sum_spans(row, sums, cols, |&[r, g, b]: &[u8; 3]| {
            if byte(r) == k0 && byte(g) == k1 && byte(b) == k2 {
                255
            } else {
                luma(byte(r), byte(g), byte(b))
            }
        }),
        Pixels::Rgb16 if k0 == NO_KEY => sum_spans(row, sums, cols, |p: &[u8; 6]| {
            let [r, _, g, _, b, _] = *p;
            luma(byte(r), byte(g), byte(b))
        }),
        Pixels::Rgb16 => sum_spans(row, sums, cols, |p: &[u8; 6]| {
            let [r0, r1, g0, g1, b0, b1] = *p;
            if be(r0, r1) == k0 && be(g0, g1) == k1 && be(b0, b1) == k2 {
                255
            } else {
                luma(byte(r0), byte(g0), byte(b0))
            }
        }),
        Pixels::Rgba8 => sum_spans(row, sums, cols, |&[r, g, b, a]: &[u8; 4]| {
            over_white(luma(byte(r), byte(g), byte(b)), byte(a))
        }),
        Pixels::Rgba16 => sum_spans(row, sums, cols, |p: &[u8; 8]| {
            let [r, _, g, _, b, _, a, _] = *p;
            over_white(luma(byte(r), byte(g), byte(b)), byte(a))
        }),
    }
}

/// Sums whole-byte pixels of `N` bytes span by span into `sums`.
#[inline(always)]
fn sum_spans<const N: usize>(
    row: &[u8],
    sums: &mut [u32],
    mut cols: Spans,
    luma: impl Fn(&[u8; N]) -> u32,
) {
    let (mut pixels, _) = row.as_chunks::<N>();
    for slot in sums {
        let len = (cols.next_len() as usize).min(pixels.len());
        let (span, rest) = pixels.split_at(len);
        pixels = rest;
        let mut sum = 0;
        for pixel in span {
            sum += luma(pixel);
        }
        *slot += sum;
    }
}

/// Sums 1, 2 or 4-bit pixels, most significant first in each byte, through
/// the table.
fn sum_packed(row: &[u8], depth: u8, sums: &mut [u32], mut cols: Spans, table: &[u8; 256]) {
    let depth = u32::from(depth);
    let mask = (1u32 << depth) - 1;
    let mut bytes = row.iter();
    let (mut current, mut left) = (0u32, 0u32);
    for slot in sums {
        let mut sum = 0;
        for _ in 0..cols.next_len() {
            if left == 0 {
                current = u32::from(bytes.next().copied().unwrap_or(0));
                left = 8;
            }
            left -= depth;
            let index = (current >> left) & mask & 0xFF;
            sum += u32::from(table[index as usize]);
        }
        *slot += sum;
    }
}

/// Unfilters `current[start..]` in place. `above` is the same columns of the
/// row above. Either the window begins at the row's first byte, or `start`
/// is at least 8, so every left neighbour a filter needs is in the window.
/// The window begins on a multiple of 4 in the row, `start` is a multiple
/// of 4 and of `bpp` there, and the window ends on a whole pixel.
fn unfilter_window(filter: u8, bpp: usize, current: &mut [u8], above: &[u8], start: usize) {
    match (filter, bpp) {
        (1, _) => unfilter_sub(current, start, bpp),
        (2, _) => unfilter_up(current, above, start),
        (3, 1) => unfilter_average::<1>(current, above, start),
        (3, 2) => unfilter_average::<2>(current, above, start),
        (3, 3) => unfilter_average::<3>(current, above, start),
        (3, 4) => unfilter_average::<4>(current, above, start),
        (3, 6) => unfilter_average::<6>(current, above, start),
        (3, _) => unfilter_average::<8>(current, above, start),
        (4, 1) => unfilter_paeth::<1>(current, above, start),
        (4, 2) => unfilter_paeth::<2>(current, above, start),
        (4, 3) => unfilter_paeth::<3>(current, above, start),
        (4, 4) => unfilter_paeth::<4>(current, above, start),
        (4, 6) => unfilter_paeth::<6>(current, above, start),
        (4, _) => unfilter_paeth::<8>(current, above, start),
        // None, or a type already rejected when the filter byte arrived.
        _ => {}
    }
}

const LOW_SEVEN: u32 = 0x7F7F_7F7F;
const HIGH_BITS: u32 = 0x8080_8080;

/// Four bytewise wrapping additions in one `u32`: the low seven bits of each
/// lane add without reaching the next lane, and the top bits are patched in
/// with XOR.
#[inline(always)]
fn add_lanes(a: u32, b: u32) -> u32 {
    ((a & LOW_SEVEN) + (b & LOW_SEVEN)) ^ ((a ^ b) & HIGH_BITS)
}

/// The whole words from `start`, as `(start, end)`.
fn word_span(start: usize, len: usize) -> (usize, usize) {
    (start, start + (len.saturating_sub(start) & !3))
}

fn unfilter_up(current: &mut [u8], above: &[u8], start: usize) {
    let len = current.len().min(above.len());
    let (from, to) = word_span(start, len);
    let mut done = start;
    if let (Some(cur), Some(up)) = (current.get_mut(from..to), above.get(from..to)) {
        if let (Ok(cur), Ok(up)) = (
            bytemuck::try_cast_slice_mut::<u8, u32>(cur),
            bytemuck::try_cast_slice::<u8, u32>(up),
        ) {
            for (c, &u) in cur.iter_mut().zip(up) {
                *c = add_lanes(*c, u);
            }
            done = to;
        }
    }
    let (Some(cur), Some(up)) = (current.get_mut(done..len), above.get(done..len)) else {
        return;
    };
    for (c, &u) in cur.iter_mut().zip(up) {
        *c = c.wrapping_add(u);
    }
}

fn unfilter_sub(current: &mut [u8], start: usize, bpp: usize) {
    let len = current.len();
    let (from, to) = word_span(start, len);
    let word_before = |back: usize| -> u32 {
        start
            .checked_sub(back)
            .and_then(|at| current.get(at..at + 4))
            .and_then(|bytes| bytes.try_into().ok())
            .map_or(0, u32::from_le_bytes)
    };
    let (one_back, two_back) = (word_before(4), word_before(8));
    let mut done = start;
    if let Some(words) = current
        .get_mut(from..to)
        .and_then(|bytes| bytemuck::try_cast_slice_mut::<u8, u32>(bytes).ok())
    {
        done = to;
        match bpp {
            1 => sub_words::<1>(words, one_back, two_back),
            2 => sub_words::<2>(words, one_back, two_back),
            3 => sub_words::<3>(words, one_back, two_back),
            4 => sub_words::<4>(words, one_back, two_back),
            6 => sub_words::<6>(words, one_back, two_back),
            8 => sub_words::<8>(words, one_back, two_back),
            _ => done = start,
        }
    }
    for i in done..len {
        let left = if i >= bpp { current[i - bpp] } else { 0 };
        current[i] = current[i].wrapping_add(left);
    }
}

/// Sub over whole words, four lanes at a time. The lanes hold bytes in
/// little-endian order, so shifting a word left moves bytes to the right in
/// the row. `one_back` and `two_back` are the unfiltered words before the
/// first one (zero before the row's start). Within a word, a left distance
/// under four becomes a prefix sum by shifted adds.
fn sub_words<const BPP: usize>(words: &mut [u32], mut one_back: u32, mut two_back: u32) {
    for word in words {
        let x = u32::from_le(*word);
        let x = match BPP {
            1 => {
                let x = add_lanes(x, one_back >> 24);
                let x = add_lanes(x, x << 8);
                add_lanes(x, x << 16)
            }
            2 => {
                let x = add_lanes(x, one_back >> 16);
                add_lanes(x, x << 16)
            }
            3 => {
                let x = add_lanes(x, one_back >> 8);
                add_lanes(x, x << 24)
            }
            4 => add_lanes(x, one_back),
            6 => add_lanes(x, (two_back >> 16) | (one_back << 16)),
            _ => add_lanes(x, two_back),
        };
        *word = x.to_le();
        two_back = one_back;
        one_back = x;
    }
}

/// The `BPP` bytes before `start`, or zeros at the row's start.
#[inline(always)]
fn pixel_before<const BPP: usize>(row: &[u8], start: usize) -> [u8; BPP] {
    start
        .checked_sub(BPP)
        .and_then(|at| row.get(at..start))
        .and_then(|pixel| pixel.try_into().ok())
        .unwrap_or([0; BPP])
}

/// Average and Paeth go a pixel at a time, with the neighbours to the left
/// carried in registers: each byte depends on the byte `BPP` before it, so
/// words do not help, but a whole-pixel stride needs no bounds checks.
fn unfilter_average<const BPP: usize>(current: &mut [u8], above: &[u8], start: usize) {
    let mut left = pixel_before::<BPP>(current, start);
    let len = current.len().min(above.len());
    let (Some(cur), Some(up)) = (current.get_mut(start..len), above.get(start..len)) else {
        return;
    };
    let (cur, _) = cur.as_chunks_mut::<BPP>();
    let (up, _) = up.as_chunks::<BPP>();
    for (c, u) in cur.iter_mut().zip(up) {
        for j in 0..BPP {
            let mean = (u16::from(left[j]) + u16::from(u[j])) >> 1;
            left[j] = c[j].wrapping_add(mean as u8);
        }
        *c = left;
    }
}

fn unfilter_paeth<const BPP: usize>(current: &mut [u8], above: &[u8], start: usize) {
    let mut left = pixel_before::<BPP>(current, start);
    let mut up_left = pixel_before::<BPP>(above, start);
    let len = current.len().min(above.len());
    let (Some(cur), Some(up)) = (current.get_mut(start..len), above.get(start..len)) else {
        return;
    };
    let (cur, _) = cur.as_chunks_mut::<BPP>();
    let (up, _) = up.as_chunks::<BPP>();
    for (c, u) in cur.iter_mut().zip(up) {
        for j in 0..BPP {
            left[j] = c[j].wrapping_add(paeth(left[j], u[j], up_left[j]));
        }
        *c = left;
        up_left = *u;
    }
}

/// The Paeth predictor: whichever of left, up and up-left is nearest `left +
/// up - up_left`, ties in that order. Written with masks; without
/// conditional moves the compiler may still pick short branches.
#[inline(always)]
fn paeth(left: u8, up: u8, up_left: u8) -> u8 {
    let (a, b, c) = (i32::from(left), i32::from(up), i32::from(up_left));
    let pa = (b - c).abs();
    let pb = (a - c).abs();
    let pc = (a + b - 2 * c).abs();
    // All ones when pb < pa or pc < pa, so `left` loses.
    let not_left = ((pb - pa) | (pc - pa)) >> 31;
    // All ones when pc < pb, so `up_left` beats `up`.
    let take_up_left = (pc - pb) >> 31;
    let up_or_up_left = b ^ ((b ^ c) & take_up_left);
    (a ^ ((a ^ up_or_up_left) & not_left)) as u8
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use ::png as reference;
    use miniz_oxide::deflate::compress_to_vec_zlib;
    use miniz_oxide::inflate::decompress_to_vec_zlib;
    use std::format;
    use std::vec;
    use std::vec::Vec;

    /// SplitMix64, so the tests need no PRNG dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    const FORMATS: [(ColorType, u8); 15] = [
        (ColorType::Gray, 1),
        (ColorType::Gray, 2),
        (ColorType::Gray, 4),
        (ColorType::Gray, 8),
        (ColorType::Gray, 16),
        (ColorType::Rgb, 8),
        (ColorType::Rgb, 16),
        (ColorType::Palette, 1),
        (ColorType::Palette, 2),
        (ColorType::Palette, 4),
        (ColorType::Palette, 8),
        (ColorType::GrayAlpha, 8),
        (ColorType::GrayAlpha, 16),
        (ColorType::Rgba, 8),
        (ColorType::Rgba, 16),
    ];
    const WIDTHS: [u32; 7] = [1, 2, 3, 7, 9, 31, 33];
    /// Each forced filter, then the encoder's adaptive choice (`None`).
    const FILTERS: [Option<reference::FilterType>; 6] = [
        Some(reference::FilterType::NoFilter),
        Some(reference::FilterType::Sub),
        Some(reference::FilterType::Up),
        Some(reference::FilterType::Avg),
        Some(reference::FilterType::Paeth),
        None,
    ];

    fn takes_trns(color: ColorType) -> bool {
        matches!(color, ColorType::Gray | ColorType::Rgb | ColorType::Palette)
    }

    /// An image for the reference encoder.
    #[derive(Clone)]
    struct Image {
        width: u32,
        height: u32,
        color: ColorType,
        depth: u8,
        /// Packed scanlines without filter bytes, 16-bit samples big-endian.
        data: Vec<u8>,
        palette: Option<Vec<u8>>,
        trns: Option<Vec<u8>>,
    }

    fn pack(values: &[u32], width: u32, color: ColorType, depth: u8) -> Vec<u8> {
        let per_row = width as usize * color.samples() as usize;
        let mut out = Vec::new();
        for row in values.chunks(per_row) {
            match depth {
                16 => row
                    .iter()
                    .for_each(|&v| out.extend((v as u16).to_be_bytes())),
                8 => row.iter().for_each(|&v| out.push(v as u8)),
                _ => {
                    let (mut acc, mut bits) = (0u32, 0u8);
                    for &v in row {
                        acc = (acc << depth) | v;
                        bits += depth;
                        if bits == 8 {
                            out.push(acc as u8);
                            (acc, bits) = (0, 0);
                        }
                    }
                    if bits > 0 {
                        out.push((acc << (8 - bits)) as u8);
                    }
                }
            }
        }
        out
    }

    fn random_image(
        rng: &mut Rng,
        color: ColorType,
        depth: u8,
        width: u32,
        height: u32,
        transparency: bool,
    ) -> Image {
        let samples = color.samples() as usize;
        let max = if depth == 16 {
            0xFFFF
        } else {
            (1u32 << depth) - 1
        };
        let pixels = (width * height) as usize;
        let entries = if color == ColorType::Palette {
            1 + rng.below(1 << depth)
        } else {
            0
        };
        let mut values = Vec::with_capacity(pixels * samples);
        for _ in 0..pixels {
            for s in 0..samples {
                let alpha = (color == ColorType::GrayAlpha && s == 1)
                    || (color == ColorType::Rgba && s == 3);
                values.push(if color == ColorType::Palette {
                    rng.below(entries) as u32
                } else if alpha {
                    // Plenty of fully clear and fully opaque pixels.
                    match rng.below(4) {
                        0 => 0,
                        1 => max,
                        _ => rng.next() as u32 & max,
                    }
                } else {
                    rng.next() as u32 & max
                });
            }
        }
        let (mut palette, mut trns) = (None, None);
        if color == ColorType::Palette {
            palette = Some((0..entries * 3).map(|_| rng.next() as u8).collect());
            if transparency {
                // Often shorter than the palette: the rest stay opaque.
                let count = 1 + rng.below(entries);
                trns = Some(
                    (0..count)
                        .map(|_| match rng.below(3) {
                            0 => 0,
                            1 => 255,
                            _ => rng.next() as u8,
                        })
                        .collect(),
                );
            }
        } else if transparency {
            // Key on a pixel that exists, so some pixels match.
            let at = rng.below(pixels) * samples;
            let key = values[at..at + samples].to_vec();
            if depth == 16 {
                // Near misses differing in one bit, often only in a low
                // byte: they must stay opaque.
                for _ in 0..pixels.div_ceil(3) {
                    let p = rng.below(pixels) * samples;
                    values[p..p + samples].copy_from_slice(&key);
                    values[p + rng.below(samples)] ^= 1 << rng.below(16);
                }
                let p = rng.below(pixels) * samples;
                values[p..p + samples].copy_from_slice(&key);
            }
            trns = Some(key.iter().flat_map(|&k| (k as u16).to_be_bytes()).collect());
        }
        Image {
            width,
            height,
            color,
            depth,
            data: pack(&values, width, color, depth),
            palette,
            trns,
        }
    }

    fn encode(image: &Image, filter: Option<reference::FilterType>) -> Vec<u8> {
        let mut file = Vec::new();
        let mut encoder = reference::Encoder::new(&mut file, image.width, image.height);
        encoder.set_color(match image.color {
            ColorType::Gray => reference::ColorType::Grayscale,
            ColorType::Rgb => reference::ColorType::Rgb,
            ColorType::Palette => reference::ColorType::Indexed,
            ColorType::GrayAlpha => reference::ColorType::GrayscaleAlpha,
            ColorType::Rgba => reference::ColorType::Rgba,
        });
        encoder.set_depth(match image.depth {
            1 => reference::BitDepth::One,
            2 => reference::BitDepth::Two,
            4 => reference::BitDepth::Four,
            8 => reference::BitDepth::Eight,
            _ => reference::BitDepth::Sixteen,
        });
        if let Some(palette) = &image.palette {
            encoder.set_palette(palette.clone());
        }
        if let Some(trns) = &image.trns {
            encoder.set_trns(trns.clone());
        }
        match filter {
            Some(filter) => {
                // The default `Fast` level stores small images unfiltered;
                // this level keeps the filter asked for.
                encoder.set_compression(reference::Compression::Default);
                encoder.set_filter(filter);
            }
            // fdeflate's streams, stored blocks included, for variety.
            None => encoder.set_adaptive_filter(reference::AdaptiveFilterType::Adaptive),
        }
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&image.data).unwrap();
        writer.finish().unwrap();
        file
    }

    fn chunks(file: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 8;
        while at + 12 <= file.len() {
            let len = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
            let kind = file[at + 4..at + 8].try_into().unwrap();
            out.push((kind, file[at + 8..at + 8 + len].to_vec()));
            at += 12 + len;
        }
        out
    }

    /// A file from chunks, with correct CRCs so the reference decoder
    /// accepts it too.
    fn assemble(chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
        let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
        let mut out = SIGNATURE.to_vec();
        for (kind, data) in chunks {
            out.extend((data.len() as u32).to_be_bytes());
            out.extend(kind);
            out.extend(data);
            let mut digest = crc.digest();
            digest.update(kind);
            digest.update(data);
            out.extend(digest.finalize().to_be_bytes());
        }
        out
    }

    fn header_chunk(width: u32, height: u32, depth: u8, color: u8) -> ([u8; 4], Vec<u8>) {
        let mut data = Vec::new();
        data.extend(width.to_be_bytes());
        data.extend(height.to_be_bytes());
        data.extend([depth, color, 0, 0, 0]);
        (*b"IHDR", data)
    }

    fn image_data(file: &[u8]) -> Vec<u8> {
        chunks(file)
            .into_iter()
            .filter(|(kind, _)| kind == b"IDAT")
            .flat_map(|(_, data)| data)
            .collect()
    }

    /// The filter type of every row, read back from the file.
    fn row_filters(file: &[u8]) -> Vec<u8> {
        let info = probe(file).unwrap();
        let raw = decompress_to_vec_zlib(&image_data(file)).unwrap();
        raw.chunks(info.stride() as usize + 1)
            .map(|row| row[0])
            .collect()
    }

    /// Full-size luma from the reference decoder's raw samples, converted
    /// the plain way: divisions, no tables.
    fn reference_luma(file: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut decoder = reference::Decoder::new(file);
        decoder.set_transformations(reference::Transformations::IDENTITY);
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut buf).unwrap();
        let info = reader.info();
        let (width, height) = (info.width as usize, info.height as usize);
        let depth = info.bit_depth as usize;
        let channels = info.color_type.samples();
        let palette = info.palette.as_deref().unwrap_or(&[]);
        let trns = info.trns.as_deref();
        let luma = |r: u32, g: u32, b: u32| (77 * r + 151 * g + 28 * b + 128) >> 8;
        let over = |v: u32, a: u32| (v * a + 255 * (255 - a) + 127) / 255;
        let to8 = |v: u32| {
            if depth == 16 {
                v >> 8
            } else {
                v * 255 / ((1 << depth) - 1)
            }
        };
        let mut out = Vec::with_capacity(width * height);
        for y in 0..height {
            let line = &buf[y * frame.line_size..][..frame.line_size];
            for x in 0..width {
                let sample = |c: usize| -> u32 {
                    let i = x * channels + c;
                    match depth {
                        16 => u32::from(u16::from_be_bytes([line[2 * i], line[2 * i + 1]])),
                        8 => u32::from(line[i]),
                        _ => {
                            let bit = i * depth;
                            u32::from(line[bit / 8] >> (8 - depth - bit % 8)) & ((1 << depth) - 1)
                        }
                    }
                };
                // The reference keeps 16-bit keys whole and others as one
                // byte per sample.
                let keyed = |count: usize| {
                    trns.is_some_and(|key| {
                        (0..count).all(|c| {
                            let k = if depth == 16 {
                                u32::from(u16::from_be_bytes([key[2 * c], key[2 * c + 1]]))
                            } else {
                                u32::from(key[c])
                            };
                            sample(c) == k
                        })
                    })
                };
                let value = match info.color_type {
                    reference::ColorType::Grayscale if keyed(1) => 255,
                    reference::ColorType::Grayscale => to8(sample(0)),
                    reference::ColorType::Rgb if keyed(3) => 255,
                    reference::ColorType::Rgb => {
                        luma(to8(sample(0)), to8(sample(1)), to8(sample(2)))
                    }
                    reference::ColorType::Indexed => {
                        let i = sample(0) as usize;
                        match palette.get(3 * i..3 * i + 3) {
                            Some(rgb) => {
                                let alpha = trns.and_then(|t| t.get(i)).copied().unwrap_or(255);
                                let y = luma(rgb[0].into(), rgb[1].into(), rgb[2].into());
                                over(y, alpha.into())
                            }
                            None => 0,
                        }
                    }
                    reference::ColorType::GrayscaleAlpha => over(to8(sample(0)), to8(sample(1))),
                    reference::ColorType::Rgba => over(
                        luma(to8(sample(0)), to8(sample(1)), to8(sample(2))),
                        to8(sample(3)),
                    ),
                };
                out.push(value as u8);
            }
        }
        (width as u32, height as u32, out)
    }

    /// The box filter written the obvious way.
    fn box_filter(full: &[u8], width: u32, height: u32, out_w: u32, out_h: u32) -> Vec<u8> {
        let (w, h, ow, oh) = (
            width as usize,
            height as usize,
            out_w as usize,
            out_h as usize,
        );
        let mut sum = vec![0u64; ow * oh];
        let mut count = vec![0u64; ow * oh];
        for y in 0..h {
            for x in 0..w {
                let at = (y * oh / h) * ow + x * ow / w;
                sum[at] += u64::from(full[y * w + x]);
                count[at] += 1;
            }
        }
        sum.iter()
            .zip(&count)
            .map(|(&s, &c)| ((s + c / 2) / c) as u8)
            .collect()
    }

    struct Collect {
        width: usize,
        rows: u32,
        luma: Vec<u8>,
    }

    impl LumaSink for Collect {
        fn row(&mut self, y: u32, luma: &[u8]) -> Result<(), SinkError> {
            assert_eq!(y, self.rows, "rows arrive in order");
            assert_eq!(luma.len(), self.width, "rows are exactly the output width");
            self.rows += 1;
            self.luma.extend_from_slice(luma);
            Ok(())
        }
    }

    /// Feeds `file` in pieces whose sizes `piece` picks from the bytes left.
    fn decode_in(
        file: &[u8],
        (out_w, out_h): (u32, u32),
        scratch: &mut [u8],
        mut piece: impl FnMut(usize) -> usize,
    ) -> Result<Vec<u8>, PngError> {
        // Starts raw, like the firmware's ZIP inflater.
        let mut inflate = InflateState::new_boxed(DataFormat::Raw);
        let mut sink = Collect {
            width: out_w as usize,
            rows: 0,
            luma: Vec::new(),
        };
        let mut decoder = PngDecoder::new(scratch, out_w, out_h)?;
        let mut rest = file;
        while !rest.is_empty() {
            let n = piece(rest.len()).clamp(1, rest.len());
            let (part, tail) = rest.split_at(n);
            decoder.feed(&mut inflate, part, &mut sink)?;
            rest = tail;
        }
        decoder.finish(&mut inflate, &mut sink)?;
        assert_eq!(sink.rows, out_h, "a decode that succeeds emits every row");
        Ok(sink.luma)
    }

    /// Decodes whole, in exactly the scratch the probe asks for, once over
    /// each of three junk fills, which must all agree.
    fn decode(file: &[u8], out: (u32, u32)) -> Result<Vec<u8>, PngError> {
        let info = probe(file)?;
        let mut scratch = vec![0xA5; info.scratch_bytes(out.0)];
        let result = decode_in(file, out, &mut scratch, |n| n);
        for junk in [0x00, 0xFF] {
            scratch.fill(junk);
            let again = decode_in(file, out, &mut scratch, |n| n);
            assert_eq!(again, result, "scratch filled with {junk:#04x}");
        }
        result
    }

    #[test]
    fn every_format_and_filter_matches_the_reference_at_full_size() {
        let mut rng = Rng(1);
        let mut cases = 0;
        for (color, depth) in FORMATS {
            for transparency in [false, true] {
                if transparency && !takes_trns(color) {
                    continue;
                }
                for width in WIDTHS {
                    for height in [1, 3, 8] {
                        for filter in FILTERS {
                            let image =
                                random_image(&mut rng, color, depth, width, height, transparency);
                            let file = encode(&image, filter);
                            if let Some(filter) = filter {
                                let types = row_filters(&file);
                                assert!(types.iter().all(|&t| t == filter as u8), "{types:?}");
                            }
                            let (w, h, expected) = reference_luma(&file);
                            assert_eq!((w, h), (width, height));
                            let what = format!(
                                "{color:?} {depth}-bit trns {transparency} {width}x{height} {filter:?}"
                            );
                            let ours = decode(&file, (width, height))
                                .unwrap_or_else(|e| panic!("{what}: {e:?}"));
                            assert_eq!(ours, expected, "{what}");
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 26 * WIDTHS.len() * 3 * FILTERS.len());
    }

    #[test]
    fn sixteen_bit_color_keys_compare_all_sixteen_bits() {
        let gray = Image {
            width: 4,
            height: 1,
            color: ColorType::Gray,
            depth: 16,
            data: [0x1234u16, 0x1235, 0x1334, 0x1234]
                .iter()
                .flat_map(|v| v.to_be_bytes())
                .collect(),
            palette: None,
            trns: Some(vec![0x12, 0x34]),
        };
        let file = encode(&gray, Some(reference::FilterType::Sub));
        assert_eq!(decode(&file, (4, 1)).unwrap(), vec![255, 0x12, 0x13, 255]);
        assert_eq!(reference_luma(&file).2, vec![255, 0x12, 0x13, 255]);

        let pixels: [[u16; 3]; 3] = [
            [0x1020, 0x3040, 0x5060],
            [0x1020, 0x3040, 0x5061],
            [0x1020, 0x3041, 0x5060],
        ];
        let rgb = Image {
            width: 3,
            height: 1,
            color: ColorType::Rgb,
            depth: 16,
            data: pixels
                .iter()
                .flatten()
                .flat_map(|v| v.to_be_bytes())
                .collect(),
            palette: None,
            trns: Some(vec![0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        };
        let file = encode(&rgb, Some(reference::FilterType::Paeth));
        let opaque = luma(0x10, 0x30, 0x50) as u8;
        assert_eq!(decode(&file, (3, 1)).unwrap(), vec![255, opaque, opaque]);
        assert_eq!(reference_luma(&file).2, vec![255, opaque, opaque]);
    }

    #[test]
    fn palette_alpha_defaults_to_opaque_and_missing_entries_to_black() {
        // Four entries, alpha for the first two only; index 5 has no entry.
        let image = Image {
            width: 5,
            height: 1,
            color: ColorType::Palette,
            depth: 4,
            data: pack(&[0, 1, 2, 3, 5], 5, ColorType::Palette, 4),
            palette: Some(vec![0, 0, 0, 255, 255, 255, 10, 20, 30, 200, 100, 50]),
            trns: Some(vec![0, 128]),
        };
        let file = encode(&image, Some(reference::FilterType::NoFilter));
        let expected = vec![
            255,
            255,
            luma(10, 20, 30) as u8,
            luma(200, 100, 50) as u8,
            0,
        ];
        assert_eq!(decode(&file, (5, 1)).unwrap(), expected);
        assert_eq!(reference_luma(&file).2, expected);

        // A tRNS longer than the palette is ignored, as the reference does.
        let mut parts = chunks(&file);
        let at = parts.iter().position(|(kind, _)| kind == b"tRNS").unwrap();
        parts[at].1 = vec![0, 0, 0, 0, 0];
        let ignored = assemble(&parts);
        let opaque = vec![0, 255, luma(10, 20, 30) as u8, luma(200, 100, 50) as u8, 0];
        assert_eq!(decode(&ignored, (5, 1)).unwrap(), opaque);
    }

    #[test]
    fn feeding_in_any_pieces_gives_the_same_rows() {
        let mut rng = Rng(2);
        let shapes = [(33, 8), (70, 5), (9, 31), (130, 3)];
        for (color, depth) in FORMATS {
            for (width, height) in shapes {
                let image = random_image(&mut rng, color, depth, width, height, takes_trns(color));
                let file = encode(&image, None);
                let info = probe(&file).unwrap();
                for out in [(width, height), (1 + width / 3, 1 + height / 2)] {
                    let mut scratch = vec![0xA5; info.scratch_bytes(out.0)];
                    let whole = decode_in(&file, out, &mut scratch, |n| n).unwrap();
                    scratch.fill(0xA5);
                    let bytes = decode_in(&file, out, &mut scratch, |_| 1).unwrap();
                    scratch.fill(0xA5);
                    let mut sizes = Rng(rng.next());
                    let random =
                        decode_in(&file, out, &mut scratch, |n| 1 + sizes.below(n.min(300)))
                            .unwrap();
                    assert_eq!(bytes, whole, "{color:?} {depth} {out:?}");
                    assert_eq!(random, whole, "{color:?} {depth} {out:?}");
                }
            }
        }
    }

    #[test]
    fn rows_longer_than_one_step_unfilter_in_pieces() {
        // Strides of 1-4 KB take several inflate steps per row, so every
        // filter runs on windows that start mid-row.
        let mut rng = Rng(17);
        for (color, depth) in FORMATS {
            let bits = color.samples() * u32::from(depth);
            let width = (8 * 1500 / bits).min(MAX_DIMENSION) + rng.below(400) as u32;
            for filter in FILTERS {
                let image = random_image(&mut rng, color, depth, width, 4, takes_trns(color));
                let file = encode(&image, filter);
                if let Some(filter) = filter {
                    assert!(row_filters(&file).iter().all(|&t| t == filter as u8));
                }
                let (_, _, full) = reference_luma(&file);
                let info = probe(&file).unwrap();
                let mut scratch = vec![0xA5; info.scratch_bytes(width)];
                let mut sizes = Rng(rng.next());
                let ours = decode_in(&file, (width, 4), &mut scratch, |n| {
                    1 + sizes.below(n.min(700))
                })
                .unwrap();
                assert_eq!(ours, full, "{color:?} {depth} {filter:?}");
            }
        }
    }

    #[test]
    fn the_row_above_survives_its_move_at_every_stride() {
        // Up, Average and Paeth read the row above after it has moved on by
        // the gap. Strides of every remainder mod 4, below the gap, around
        // it, and well past it, where the move overlaps itself.
        let mut rng = Rng(18);
        let mut shapes = Vec::new();
        for width in (5..=8).chain(509..=516).chain(2045..=2048) {
            shapes.push((ColorType::Gray, 8, width));
        }
        for width in (170..=173).chain(600..=603) {
            shapes.push((ColorType::Rgb, 8, width));
        }
        for stride in 512..=515 {
            shapes.push((ColorType::Gray, 1, 8 * stride - 7));
        }
        let mut remainders = [false; 4];
        for (color, depth, width) in shapes {
            let stride = PngInfo {
                width,
                height: 1,
                color_type: color,
                bit_depth: depth,
                interlaced: false,
            }
            .stride();
            remainders[(stride % 4) as usize] = true;
            for filter in [
                Some(reference::FilterType::Up),
                Some(reference::FilterType::Avg),
                Some(reference::FilterType::Paeth),
                None,
            ] {
                let image = random_image(&mut rng, color, depth, width, 6, false);
                let file = encode(&image, filter);
                if let Some(filter) = filter {
                    assert!(row_filters(&file).iter().all(|&t| t == filter as u8));
                }
                let (_, _, full) = reference_luma(&file);
                assert_eq!(
                    decode(&file, (width, 6)).as_ref(),
                    Ok(&full),
                    "{color:?} {depth} stride {stride} {filter:?}"
                );
            }
        }
        assert_eq!(remainders, [true; 4]);
    }

    #[test]
    fn the_row_move_matches_copy_within() {
        let mut rng = Rng(19);
        for stride in (0..=1100).chain(2040..=2060) {
            // A u32 backing puts the region on a word, as in the decoder.
            let mut backing = vec![0u32; (stride + ROW_GAP).div_ceil(4)];
            let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut backing);
            let region = &mut bytes[..stride + ROW_GAP];
            region.iter_mut().for_each(|b| *b = rng.next() as u8);
            let mut expected = region.to_vec();
            expected.copy_within(..stride, ROW_GAP);
            shift_row(region, stride).unwrap();
            assert_eq!(region, &expected[..], "stride {stride}");
        }
    }

    #[test]
    fn downscale_matches_a_plain_box_filter() {
        let mut rng = Rng(3);
        for round in 0..300 {
            let (color, depth) = FORMATS[round % FORMATS.len()];
            let width = 1 + rng.below(90) as u32;
            let height = 1 + rng.below(60) as u32;
            let image = random_image(&mut rng, color, depth, width, height, takes_trns(color));
            let file = encode(&image, FILTERS[rng.below(FILTERS.len())]);
            let (_, _, full) = reference_luma(&file);
            let out = match rng.below(4) {
                0 => (1, 1),
                1 => super::super::fit_inside(width, height, 1 + width / 2, 1 + height / 3),
                _ => (
                    1 + rng.below(width as usize) as u32,
                    1 + rng.below(height as usize) as u32,
                ),
            };
            let expected = box_filter(&full, width, height, out.0, out.1);
            assert_eq!(
                decode(&file, out).unwrap(),
                expected,
                "{color:?} {depth} {width}x{height} to {out:?}"
            );
        }
    }

    #[test]
    fn one_scratch_serves_consecutive_images() {
        let mut rng = Rng(4);
        let mut scratch = vec![0xA5u8; 64 * 1024];
        for round in 0..60 {
            let (color, depth) = FORMATS[(round * 7) % FORMATS.len()];
            let width = 1 + rng.below(120) as u32;
            let height = 1 + rng.below(40) as u32;
            let image = random_image(&mut rng, color, depth, width, height, takes_trns(color));
            let file = encode(&image, None);
            let (_, _, full) = reference_luma(&file);
            let out = (
                1 + rng.below(width as usize) as u32,
                1 + rng.below(height as usize) as u32,
            );
            // Whatever the last image left behind stays in the scratch.
            let ours = decode_in(&file, out, &mut scratch, |n| n).unwrap();
            assert_eq!(ours, box_filter(&full, width, height, out.0, out.1));
        }
    }

    #[test]
    fn widest_images_in_20_kb_at_800_wide() {
        let widest = [
            (ColorType::Rgba, 8, 4190),
            (ColorType::Rgba, 16, 2095),
            (ColorType::Rgb, 8, 5586),
            (ColorType::Rgb, 16, 2793),
            (ColorType::GrayAlpha, 8, 8380),
            (ColorType::GrayAlpha, 16, 4190),
            (ColorType::Gray, 16, 8380),
            (ColorType::Gray, 8, MAX_DIMENSION),
            (ColorType::Gray, 1, MAX_DIMENSION),
            (ColorType::Palette, 8, MAX_DIMENSION),
            (ColorType::Palette, 1, MAX_DIMENSION),
        ];
        let mut rng = Rng(5);
        for (color_type, bit_depth, width) in widest {
            let info = PngInfo {
                width,
                height: 1,
                color_type,
                bit_depth,
                interlaced: false,
            };
            assert!(
                info.scratch_bytes(800) <= 20 * 1024,
                "{color_type:?} {bit_depth}"
            );
            if width < MAX_DIMENSION {
                let wider = PngInfo {
                    width: width + 1,
                    ..info
                };
                assert!(
                    wider.scratch_bytes(800) > 20 * 1024,
                    "{color_type:?} {bit_depth}"
                );
            }
            // And it really decodes there, in exactly 20 KB.
            let image = random_image(&mut rng, color_type, bit_depth, width, 3, false);
            let file = encode(&image, None);
            let (_, _, full) = reference_luma(&file);
            let mut scratch = vec![0xA5; 20 * 1024];
            let ours = decode_in(&file, (800, 2), &mut scratch, |n| n.min(4096)).unwrap();
            assert_eq!(
                ours,
                box_filter(&full, width, 3, 800, 2),
                "{color_type:?} {bit_depth}"
            );
        }
    }

    #[test]
    fn interlaced_images_are_unsupported() {
        let mut rng = Rng(6);
        let image = random_image(&mut rng, ColorType::Rgb, 8, 9, 5, false);
        let mut file = encode(&image, None);
        // IHDR's interlace byte. CRCs go unchecked, so it need not match.
        file[28] = 1;
        let info = probe(&file).unwrap();
        assert!(info.interlaced);
        assert_eq!(info.check(), Err(PngError::Unsupported));
        assert!(!info.is_supported());
        assert_eq!(decode(&file, (9, 5)), Err(PngError::Unsupported));
    }

    #[test]
    fn oversized_images_are_rejected() {
        for (width, height) in [(16_385, 1), (1, 16_385), (4097, 4097), (16_384, 1025)] {
            let file = assemble(&[header_chunk(width, height, 8, 0), (*b"IEND", vec![])]);
            let info = probe(&file).unwrap();
            assert_eq!(info.check(), Err(PngError::TooLarge), "{width}x{height}");
            let mut scratch = vec![0u8; 1024];
            assert_eq!(
                decode_in(&file, (1, 1), &mut scratch, |n| n),
                Err(PngError::TooLarge)
            );
        }
        for (width, height) in [(4096, 4096), (16_384, 1024), (1, 16_384)] {
            let file = assemble(&[header_chunk(width, height, 8, 0)]);
            assert_eq!(probe(&file).unwrap().check(), Ok(()), "{width}x{height}");
        }
    }

    #[test]
    fn scratch_must_be_at_least_what_the_probe_says() {
        let mut rng = Rng(7);
        for (color, depth) in [
            (ColorType::Rgba, 16),
            (ColorType::Palette, 2),
            (ColorType::Rgb, 8),
        ] {
            let image = random_image(&mut rng, color, depth, 37, 6, false);
            let file = encode(&image, None);
            let out = (20, 4);
            let need = probe(&file).unwrap().scratch_bytes(out.0);
            let expected = decode(&file, out).unwrap();
            let mut short = vec![0xA5; need - 1];
            assert_eq!(
                decode_in(&file, out, &mut short, |n| n),
                Err(PngError::ScratchTooSmall)
            );
            // Exactly enough at every alignment.
            let mut backing = vec![0xA5; need + 4];
            for offset in 0..4 {
                let scratch = &mut backing[offset..offset + need];
                assert_eq!(
                    decode_in(&file, out, scratch, |n| n).as_ref(),
                    Ok(&expected)
                );
            }
        }
    }

    #[test]
    fn output_size_must_be_within_the_image() {
        let mut rng = Rng(8);
        let file = encode(
            &random_image(&mut rng, ColorType::Gray, 8, 10, 6, false),
            None,
        );
        let mut scratch = vec![0u8; 4096];
        assert_eq!(
            decode_in(&file, (11, 6), &mut scratch, |n| n),
            Err(PngError::BadOutputSize)
        );
        assert_eq!(
            decode_in(&file, (10, 7), &mut scratch, |n| n),
            Err(PngError::BadOutputSize)
        );
        assert!(matches!(
            PngDecoder::new(&mut scratch, 0, 5),
            Err(PngError::BadOutputSize)
        ));
        assert!(matches!(
            PngDecoder::new(&mut scratch, 5, 0),
            Err(PngError::BadOutputSize)
        ));
    }

    #[test]
    fn probe_reads_the_header_only() {
        let mut rng = Rng(9);
        let file = encode(
            &random_image(&mut rng, ColorType::Palette, 4, 21, 13, true),
            None,
        );
        let info = probe(&file[..PROBE_BYTES]).unwrap();
        assert_eq!(
            info,
            PngInfo {
                width: 21,
                height: 13,
                color_type: ColorType::Palette,
                bit_depth: 4,
                interlaced: false,
            }
        );
        assert_eq!(probe(&file[..PROBE_BYTES - 1]), Err(PngError::Truncated));
        assert_eq!(
            probe(b"GIF89a, not a PNG at all, honest!"),
            Err(PngError::NotPng)
        );
        assert_eq!(probe(b"GIF89a"), Err(PngError::Truncated));
        let mut bad_depth = file.clone();
        bad_depth[24] = 3;
        assert_eq!(probe(&bad_depth), Err(PngError::Corrupt));
        let mut bad_color = file.clone();
        bad_color[25] = 5;
        assert_eq!(probe(&bad_color), Err(PngError::Corrupt));
        let mut zero_width = file;
        zero_width[16..20].fill(0);
        assert_eq!(probe(&zero_width), Err(PngError::Corrupt));
    }

    #[test]
    fn chunk_rules() {
        let mut rng = Rng(10);
        let image = random_image(&mut rng, ColorType::Palette, 8, 30, 9, true);
        let file = encode(&image, None);
        let expected = decode(&file, (30, 9)).unwrap();
        let parts = chunks(&file);
        let at = |kind: &[u8; 4]| parts.iter().position(|(k, _)| k == kind).unwrap();
        let with = |index: usize, chunk: ([u8; 4], Vec<u8>)| {
            let mut changed = parts.clone();
            changed.insert(index, chunk);
            assemble(&changed)
        };
        let idat = at(b"IDAT");
        let iend = at(b"IEND");

        // Ancillary chunks anywhere outside the image data are skipped.
        let text = (*b"tEXt", b"Comment\0hello".to_vec());
        assert_eq!(
            decode(&with(1, text.clone()), (30, 9)).as_ref(),
            Ok(&expected)
        );
        assert_eq!(
            decode(&with(iend, text.clone()), (30, 9)).as_ref(),
            Ok(&expected)
        );
        assert_eq!(
            decode(&with(idat, (*b"zzZz", vec![7; 300])), (30, 9)).as_ref(),
            Ok(&expected)
        );
        // An unknown critical chunk is not.
        assert_eq!(
            decode(&with(idat, (*b"ABCD", vec![1, 2, 3])), (30, 9)),
            Err(PngError::Unsupported)
        );
        // Chunk types are letters.
        assert_eq!(
            decode(&with(idat, (*b"ab1d", vec![])), (30, 9)),
            Err(PngError::Corrupt)
        );

        // Image data split over many IDATs, some empty, decodes the same...
        let data = image_data(&file);
        let mut split: Vec<([u8; 4], Vec<u8>)> = parts[..idat].to_vec();
        split.push((*b"IDAT", vec![]));
        let mut rest = &data[..];
        while !rest.is_empty() {
            let n = rng.below(rest.len().min(40) + 1);
            split.push((*b"IDAT", rest[..n].to_vec()));
            rest = &rest[n..];
        }
        split.push((*b"IEND", vec![]));
        assert_eq!(decode(&assemble(&split), (30, 9)).as_ref(), Ok(&expected));
        // ...but IDATs must be consecutive.
        let middle = split.len() / 2;
        split.insert(middle, text);
        assert_eq!(decode(&assemble(&split), (30, 9)), Err(PngError::Corrupt));

        // A palette image needs its palette before the data.
        let mut no_palette = parts.clone();
        no_palette.retain(|(kind, _)| kind != b"PLTE");
        assert_eq!(
            decode(&assemble(&no_palette), (30, 9)),
            Err(PngError::Corrupt)
        );
        // IHDR comes first, once.
        let mut late_header = parts.clone();
        late_header.swap(0, 1);
        assert_eq!(
            decode(&assemble(&late_header), (30, 9)),
            Err(PngError::Corrupt)
        );
        assert_eq!(
            decode(&with(1, parts[0].clone()), (30, 9)),
            Err(PngError::Corrupt)
        );
        // IEND before any image data.
        let mut early_end = parts.clone();
        early_end.retain(|(kind, _)| kind != b"IDAT");
        assert_eq!(
            decode(&assemble(&early_end), (30, 9)),
            Err(PngError::Corrupt)
        );
    }

    #[test]
    fn a_decode_needs_every_row_and_nothing_after() {
        let mut rng = Rng(11);
        let image = random_image(&mut rng, ColorType::Rgb, 8, 40, 12, false);
        let file = encode(&image, None);
        let expected = decode(&file, (40, 12)).unwrap();
        // No IEND.
        assert_eq!(
            decode(&file[..file.len() - 12], (40, 12)).as_ref(),
            Ok(&expected)
        );
        // No Adler-32 either.
        let mut parts = chunks(&file);
        let idat = parts.iter().position(|(kind, _)| kind == b"IDAT").unwrap();
        let mut cut = parts[..=idat].to_vec();
        let data = &mut cut[idat].1;
        data.truncate(data.len() - 4);
        assert_eq!(decode(&assemble(&cut), (40, 12)).as_ref(), Ok(&expected));
        // A wrong Adler-32 goes unread.
        *parts[idat].1.last_mut().unwrap() ^= 1;
        assert_eq!(decode(&assemble(&parts), (40, 12)).as_ref(), Ok(&expected));
        // Cut inside the image data, the rows are short.
        assert_eq!(
            decode(&file[..file.len() / 2], (40, 12)),
            Err(PngError::Truncated)
        );
    }

    /// Deflate with the fixed codes, enough to write a stream by hand: a few
    /// literals and then any number of 258-byte repeats.
    struct FixedDeflate {
        out: Vec<u8>,
        bits: u64,
        count: u32,
    }

    impl FixedDeflate {
        fn new() -> Self {
            let mut deflate = Self {
                out: vec![0x78, 0x01],
                bits: 0,
                count: 0,
            };
            // The final block, with fixed codes.
            deflate.put(1, 1);
            deflate.put(1, 2);
            deflate
        }

        fn put(&mut self, value: u32, len: u32) {
            self.bits |= u64::from(value) << self.count;
            self.count += len;
            while self.count >= 8 {
                self.out.push(self.bits as u8);
                self.bits >>= 8;
                self.count -= 8;
            }
        }

        /// Huffman codes go most significant bit first.
        fn code(&mut self, code: u32, len: u32) {
            self.put(code.reverse_bits() >> (32 - len), len);
        }

        fn literal(&mut self, byte: u8) {
            match byte {
                0..=143 => self.code(0x30 + u32::from(byte), 8),
                _ => self.code(0x190 + u32::from(byte) - 144, 9),
            }
        }

        /// Repeats the last byte 258 times: length code 285, distance 1.
        fn repeat_258(&mut self) {
            self.code(0xC5, 8);
            self.code(0, 5);
        }

        fn finish(mut self, adler: u32) -> Vec<u8> {
            self.code(0, 7);
            self.put(0, (8 - self.count % 8) % 8);
            self.out.extend(adler.to_be_bytes());
            self.out
        }
    }

    /// One 1×1 gray row, `value`, then `repeats` · 258 + 1 zero bytes.
    fn padded_row(value: u8, repeats: u64) -> Vec<u8> {
        let mut deflate = FixedDeflate::new();
        deflate.literal(0);
        deflate.literal(value);
        deflate.literal(0);
        for _ in 0..repeats {
            deflate.repeat_258();
        }
        // Adler-32 of [0, value] and the zeros, in closed form.
        let zeros = 1 + 258 * repeats;
        let a = 1 + u64::from(value);
        let b = (2 + u64::from(value) + zeros % 65_521 * a) % 65_521;
        deflate.finish(((b << 16) | a) as u32)
    }

    #[test]
    fn nothing_after_the_last_row_is_inflated() {
        // The hand-made stream is real zlib.
        let mut expected = vec![0, 77];
        expected.resize(2 + 1 + 258 * 100, 0);
        assert_eq!(
            decompress_to_vec_zlib(&padded_row(77, 100)).unwrap(),
            expected
        );

        // A 1×1 image whose stream runs on through 64 MB of zeros.
        let stream = padded_row(77, (64 << 20) / 258);
        assert!(stream.len() < 450_000, "{}", stream.len());
        let file = assemble(&[
            header_chunk(1, 1, 8, 0),
            (*b"IDAT", stream),
            (*b"IEND", vec![]),
        ]);
        for piece in [file.len(), 4096, 1] {
            let mut scratch = vec![0xA5; 1024];
            let mut inflate = InflateState::new_boxed(DataFormat::Raw);
            let mut sink = Collect {
                width: 1,
                rows: 0,
                luma: Vec::new(),
            };
            let mut decoder = PngDecoder::new(&mut scratch, 1, 1).unwrap();
            let mut fed = 0;
            for part in file.chunks(piece) {
                decoder.feed(&mut inflate, part, &mut sink).unwrap();
                fed += part.len();
                if decoder.is_complete() {
                    break;
                }
            }
            // The row is out long before the file is.
            assert!(fed <= piece.max(100), "{fed}");
            for part in file[fed..].chunks(piece) {
                decoder.feed(&mut inflate, part, &mut sink).unwrap();
            }
            decoder.finish(&mut inflate, &mut sink).unwrap();
            assert_eq!(sink.luma, [77]);
            // The inflater wrote the row's two bytes, in a handful of calls.
            let (calls, written) = decoder.inflate_work;
            assert_eq!(written, 2);
            assert!(calls <= 16, "{calls} calls");
        }
    }

    #[test]
    fn the_largest_box_sums_do_not_overflow() {
        // 2^24 white pixels, the most an image may have, all into one output
        // pixel: the sum is 255 · 2^24 plus rounding, just under 2^32. Debug
        // builds check that arithmetic. One bit per pixel keeps it quick.
        let (width, height) = (16_384u32, 1024u32);
        assert_eq!(u64::from(width) * u64::from(height), MAX_PIXELS);
        let stride = width as usize / 8;
        let mut raw = Vec::with_capacity((stride + 1) * height as usize);
        raw.push(0);
        raw.resize(1 + stride, 0xFF);
        for _ in 1..height {
            // Up: the same as the row above.
            raw.push(2);
            raw.resize(raw.len() + stride, 0);
        }
        let file = assemble(&[
            header_chunk(width, height, 1, 0),
            (*b"IDAT", compress_to_vec_zlib(&raw, 1)),
            (*b"IEND", vec![]),
        ]);
        let mut scratch = vec![0xA5; probe(&file).unwrap().scratch_bytes(1)];
        assert_eq!(decode_in(&file, (1, 1), &mut scratch, |n| n), Ok(vec![255]));
    }

    /// A gray 8-bit image from hand-made scanlines (filter byte first).
    fn gray_from_scanlines(width: u32, height: u32, raw: &[u8]) -> Vec<u8> {
        assemble(&[
            header_chunk(width, height, 8, 0),
            (*b"IDAT", compress_to_vec_zlib(raw, 6)),
            (*b"IEND", vec![]),
        ])
    }

    #[test]
    fn row_data_must_be_well_formed() {
        let good = gray_from_scanlines(2, 2, &[0, 10, 20, 2, 1, 1]);
        assert_eq!(decode(&good, (2, 2)), Ok(vec![10, 20, 11, 21]));
        // Filter type 5 does not exist.
        let bad_filter = gray_from_scanlines(2, 2, &[0, 10, 20, 5, 1, 1]);
        assert_eq!(decode(&bad_filter, (2, 2)), Err(PngError::Corrupt));
        // Too few rows.
        let short = gray_from_scanlines(2, 2, &[0, 10, 20]);
        assert_eq!(decode(&short, (2, 2)), Err(PngError::Truncated));
        // Data after the last row is never inflated.
        let long = gray_from_scanlines(2, 2, &[0, 10, 20, 2, 1, 1, 0, 9, 9, 0, 9, 9]);
        assert_eq!(decode(&long, (2, 2)), Ok(vec![10, 20, 11, 21]));
    }

    #[test]
    fn random_scanlines_under_mixed_filters_match_the_reference() {
        let mut rng = Rng(16);
        for round in 0..400 {
            let (color, depth) = FORMATS[round % FORMATS.len()];
            let width = 1 + rng.below(40) as u32;
            let height = 1 + rng.below(12) as u32;
            let info = PngInfo {
                width,
                height,
                color_type: color,
                bit_depth: depth,
                interlaced: false,
            };
            // Any bytes under any filter type make a valid image.
            let mut raw = Vec::new();
            for _ in 0..height {
                raw.push(rng.below(5) as u8);
                raw.extend((0..info.stride()).map(|_| rng.next() as u8));
            }
            let code = match color {
                ColorType::Gray => 0,
                ColorType::Rgb => 2,
                ColorType::Palette => 3,
                ColorType::GrayAlpha => 4,
                ColorType::Rgba => 6,
            };
            let mut parts = vec![header_chunk(width, height, depth, code)];
            if color == ColorType::Palette {
                // Sometimes fewer entries than indices: those are black.
                let entries = 1 + rng.below(1 << depth);
                parts.push((
                    *b"PLTE",
                    (0..3 * entries).map(|_| rng.next() as u8).collect(),
                ));
            }
            parts.push((*b"IDAT", compress_to_vec_zlib(&raw, 1 + rng.below(9) as u8)));
            parts.push((*b"IEND", vec![]));
            let file = assemble(&parts);
            let (_, _, full) = reference_luma(&file);
            assert_eq!(
                decode(&file, (width, height)).unwrap(),
                full,
                "{color:?} {depth}"
            );
        }
    }

    #[test]
    fn a_sink_error_stops_the_decode_for_good() {
        let mut rng = Rng(12);
        let file = encode(
            &random_image(&mut rng, ColorType::Gray, 8, 8, 8, false),
            None,
        );
        let mut scratch = vec![0xA5; 4096];
        let mut inflate = InflateState::new_boxed(DataFormat::Raw);
        let mut seen = 0;
        let mut sink = |y: u32, _: &[u8]| {
            seen += 1;
            if y == 2 {
                Err(SinkError::Cancelled)
            } else {
                Ok(())
            }
        };
        let mut decoder = PngDecoder::new(&mut scratch, 8, 4).unwrap();
        let stopped = Err(PngError::Sink(SinkError::Cancelled));
        assert_eq!(decoder.feed(&mut inflate, &file, &mut sink), stopped);
        assert_eq!(decoder.feed(&mut inflate, &file, &mut sink), stopped);
        assert_eq!(decoder.finish(&mut inflate, &mut sink), stopped);
        assert_eq!(seen, 3);
    }

    #[test]
    fn damaged_and_random_input_never_panics() {
        let mut rng = Rng(13);
        let mut corpus = Vec::new();
        for (color, depth) in FORMATS {
            for _ in 0..2 {
                let width = 1 + rng.below(24) as u32;
                let height = 1 + rng.below(16) as u32;
                let image = random_image(&mut rng, color, depth, width, height, takes_trns(color));
                corpus.push(encode(&image, FILTERS[rng.below(FILTERS.len())]));
            }
        }
        // Rows longer than one inflate step and than the gap to the row
        // above, and a width at the limit.
        let large = [
            (ColorType::Rgb, 8, 600, 4),
            (ColorType::Palette, 8, 700, 3),
            (ColorType::Rgba, 16, 200, 3),
            (ColorType::Gray, 2, MAX_DIMENSION, 2),
        ];
        for (color, depth, width, height) in large {
            for filter in [None, Some(reference::FilterType::Paeth)] {
                let image = random_image(&mut rng, color, depth, width, height, takes_trns(color));
                corpus.push(encode(&image, filter));
            }
        }
        let small = corpus.len() - 2 * large.len();
        let (mut ok, mut failed) = (0, 0);
        for _ in 0..20_000 {
            // A quarter of the time, a large seed.
            let seed = if rng.below(4) == 0 {
                small + rng.below(corpus.len() - small)
            } else {
                rng.below(small)
            };
            let mut file = corpus[seed].clone();
            match rng.below(7) {
                0 => {
                    for _ in 0..1 + rng.below(4) {
                        let at = rng.below(file.len());
                        file[at] = rng.next() as u8;
                    }
                }
                1 => {
                    // Damage past the header, where the image data is.
                    for _ in 0..1 + rng.below(8) {
                        let at = 33 + rng.below(file.len() - 33);
                        file[at] ^= 1 << rng.below(8);
                    }
                }
                2 => file.truncate(rng.below(file.len())),
                3 => {
                    let other = &corpus[rng.below(corpus.len())];
                    let cut = rng.below(file.len());
                    file.truncate(cut);
                    file.extend_from_slice(&other[rng.below(other.len())..]);
                }
                4 => {
                    // A chunk length set to anything.
                    let starts: Vec<usize> = {
                        let mut at = 8;
                        let mut starts = Vec::new();
                        for (_, data) in chunks(&file) {
                            starts.push(at);
                            at += 12 + data.len();
                        }
                        starts
                    };
                    let at = starts[rng.below(starts.len())];
                    let len = match rng.below(3) {
                        0 => rng.next() as u32,
                        1 => rng.below(64) as u32,
                        _ => 0,
                    };
                    file[at..at + 4].copy_from_slice(&len.to_be_bytes());
                }
                5 => {
                    let at = rng.below(file.len());
                    if rng.below(2) == 0 {
                        let count = 1 + rng.below(16);
                        file.splice(at..at, (0..count).map(|_| rng.next() as u8));
                    } else {
                        let end = (at + 1 + rng.below(16)).min(file.len());
                        file.drain(at..end);
                    }
                }
                _ => {
                    // Noise behind a real signature and header.
                    file.truncate(33);
                    file.extend((0..rng.below(400)).map(|_| rng.next() as u8));
                }
            }
            let (out, scratch_len) = match probe(&file) {
                Ok(info) if info.is_supported() => {
                    let out = (
                        1 + rng.below(info.width.min(900) as usize) as u32,
                        1 + rng.below(info.height.min(48) as usize) as u32,
                    );
                    (out, info.scratch_bytes(out.0).min(1 << 18))
                }
                _ => (
                    (1 + rng.below(4) as u32, 1 + rng.below(4) as u32),
                    rng.below(4096),
                ),
            };
            let mut scratch = vec![0xA5; scratch_len];
            let mut sizes = Rng(rng.next());
            match decode_in(&file, out, &mut scratch, |n| 1 + sizes.below(n.min(97))) {
                Ok(luma) => {
                    assert_eq!(luma.len(), (out.0 * out.1) as usize);
                    ok += 1;
                }
                Err(_) => failed += 1,
            }
        }
        // Both paths ran plenty.
        assert!(
            ok > 1000 && failed > 10_000,
            "{ok} decoded, {failed} failed"
        );
    }

    #[test]
    fn decoder_state_stays_small() {
        // Locals and the decoder itself must stay well inside the X3's few
        // KB of spare stack.
        assert!(core::mem::size_of::<PngDecoder>() <= 256);
    }

    #[test]
    fn over_white_is_exact_for_every_byte_pair() {
        for value in 0..=255u32 {
            for alpha in 0..=255u32 {
                let exact = (value * alpha + 255 * (255 - alpha) + 127) / 255;
                assert_eq!(over_white(value, alpha), exact, "{value} at {alpha}");
            }
        }
    }

    #[test]
    fn reciprocal_division_is_exact() {
        let mut rng = Rng(14);
        let mut divisors: Vec<u32> = (1..=700).collect();
        divisors.extend([
            1 << 24,
            (1 << 24) - 1,
            65_535,
            65_536,
            16_384,
            16_385,
            3 << 20,
        ]);
        divisors.extend((0..3000).map(|_| 1 + rng.below(1 << 24) as u32));
        for d in divisors {
            let inverse = reciprocal(d);
            // The largest numerator a box mean forms.
            let top = 255 * d + d / 2;
            let mut numerators = vec![0, 1, d / 2, d - 1, d, d + 1, top - 1, top, u32::MAX];
            for k in [1, 2, 127, 128, 254, 255] {
                numerators.extend([k * d - 1, k * d, k * d + d / 2]);
            }
            numerators.extend((0..40).map(|_| rng.below(top as usize + 1) as u32));
            numerators.extend((0..10).map(|_| rng.next() as u32));
            for n in numerators {
                assert_eq!(divide(n, d, inverse), n / d, "{n} / {d}");
            }
        }
    }

    #[test]
    fn spans_follow_the_floor_mapping() {
        for total in 1..=80u32 {
            for count in 1..=total {
                let mut spans = Spans::new(total, count);
                let mut x = 0;
                for k in 0..count {
                    for _ in 0..spans.next_len() {
                        assert_eq!(x * count / total, k, "{total} into {count}");
                        x += 1;
                    }
                }
                assert_eq!(x, total);
            }
        }
    }

    #[test]
    fn branchless_paeth_matches_the_spec() {
        let spec = |a: u8, b: u8, c: u8| {
            let p = i16::from(a) + i16::from(b) - i16::from(c);
            let (pa, pb, pc) = (
                (p - i16::from(a)).abs(),
                (p - i16::from(b)).abs(),
                (p - i16::from(c)).abs(),
            );
            if pa <= pb && pa <= pc {
                a
            } else if pb <= pc {
                b
            } else {
                c
            }
        };
        let values: Vec<u8> = (0..=255)
            .step_by(3)
            .chain([1, 2, 127, 128, 254, 255])
            .collect();
        for &a in &values {
            for &b in &values {
                for &c in &values {
                    assert_eq!(paeth(a, b, c), spec(a, b, c), "{a} {b} {c}");
                }
            }
        }
    }

    #[test]
    fn word_filters_match_bytewise() {
        let mut rng = Rng(15);
        for bpp in [1, 2, 3, 4, 6, 8] {
            for len in [0usize, 1, 3, 4, 5, 8, 13, 64, 67] {
                for start in [0usize, 4, 8, 12] {
                    if start > len || (start > 0 && start < 8 && bpp > start) {
                        continue;
                    }
                    for misalign in [0usize, 1] {
                        // u32 backing keeps offset 0 word-aligned.
                        let mut cur_words = vec![0u32; len / 4 + 2];
                        let mut up_words = vec![0u32; len / 4 + 2];
                        let cur_all: &mut [u8] = bytemuck::cast_slice_mut(&mut cur_words);
                        let up_all: &mut [u8] = bytemuck::cast_slice_mut(&mut up_words);
                        let cur = &mut cur_all[misalign..misalign + len];
                        let up = &mut up_all[misalign..misalign + len];
                        cur.iter_mut().for_each(|b| *b = rng.next() as u8);
                        up.iter_mut().for_each(|b| *b = rng.next() as u8);
                        let original = cur.to_vec();

                        let mut sub = original.clone();
                        for i in start..len {
                            let left = if i >= bpp { sub[i - bpp] } else { 0 };
                            sub[i] = sub[i].wrapping_add(left);
                        }
                        unfilter_sub(cur, start, bpp);
                        assert_eq!(cur, &sub[..], "sub bpp {bpp} len {len} start {start}");

                        cur.copy_from_slice(&original);
                        let mut upped = original.clone();
                        for i in start..len {
                            upped[i] = upped[i].wrapping_add(up[i]);
                        }
                        unfilter_up(cur, up, start);
                        assert_eq!(cur, &upped[..], "up len {len} start {start}");
                    }
                }
            }
        }
    }
}
