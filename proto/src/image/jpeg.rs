//! Streaming baseline JPEG decoder: file bytes in, gray rows at a fitted
//! size out.
//!
//! Like [`super::png`], the decoder takes the file in chunks of any size,
//! borrows all of its working memory from the caller, and hands a
//! [`LumaSink`] each output row as soon as the source rows behind it are
//! in. It needs no inflater, so a deflated ZIP entry streams through the
//! ZIP's own inflater straight into it.
//!
//! # What it decodes
//!
//! Sequential Huffman-coded JPEG with 8-bit samples, baseline (SOF0) or
//! extended (SOF1): gray, or three components (YCbCr) whose sampling
//! factors are each 1 or 2 (4:4:4, 4:2:2, 4:2:0, 4:4:0, and odd mixes,
//! luma sampled more coarsely than chroma included), in one interleaved
//! scan or several, with or without restart intervals. APPn and COM
//! segments are skipped unread, so EXIF orientation is ignored.
//!
//! Refused with [`JpegError::Unsupported`], before any row goes out:
//! progressive, lossless, hierarchical and arithmetic-coded frames, 12-bit
//! samples, four components (CMYK, YCCK), two, a sampling factor of 3 or
//! 4, RGB (Adobe's APP14 with transform 0, or components named `R`, `G`,
//! `B` without JFIF), and files that keep more than four Huffman tables at
//! once (baseline allows four; extended allows eight, which no common
//! encoder writes).
//!
//! Only luma reaches the page. The Y component alone is dequantized and
//! inverse transformed; Cb and Cr coefficients are Huffman decoded only to
//! stay in step with the bit stream, then dropped, and a scan without luma
//! is passed over to its end. There is no colour conversion: JPEG's Y uses
//! the BT.601 weights the PNG decoder's luma rounds to.
//!
//! # Scaling
//!
//! A large photo shrinks before it is ever a pixel. The decoder picks the
//! smallest scale of 1/8, 1/4, 1/2 or 1 at which the image is still at
//! least the output size in both directions (see [`JpegInfo::block_side`]),
//! and turns each 8×8 block into 1×1, 2×2, 4×4 or 8×8 pixels. The reduced
//! transforms give each pixel the mean of the square of full-size pixels it
//! stands for, straight from the coefficients (`idct`), so the scaled image
//! is the full one box-filtered by that factor, up to rounding and to
//! clamping, which they apply to the mean rather than to each pixel. A box
//! filter then takes the scaled image to exactly `out_w × out_h`, as the
//! PNG decoder's does: spans stepped without division, means by reciprocals
//! computed once per image. It works across each MCU's pixels as the MCU
//! completes, keeping one byte per output column for each source row of
//! the current row of MCUs, and down that row once it is complete. A mean
//! of row means is the box mean, rounded twice.
//!
//! The decode ends at its last row: whatever follows is ignored unread.
//! Before that, it fails rather than reads on where a file has gone
//! wrong: at an EOI with rows still to come, past [`MAX_STRAY`] bytes in
//! all that belong to no segment (junk between segments, stray TEM and
//! RST markers), at anything but the next restart marker in sequence
//! where one is due, and at a restart interval with no data.
//!
//! Work is bounded per byte of input, not per image: every coefficient
//! symbol takes at least one bit of data, a block at least two symbols (DC
//! and an end of block) and at most 64, and an MCU's pixel work is fixed
//! by the scale, so a byte of scan data decodes at most four blocks. Data that runs into a marker part way
//! through an MCU finishes that MCU on zero bits, as libjpeg does, and the
//! next MCU fails, so a marker buys at most one MCU of made-up data; with
//! the empty-interval check, a scan of nothing but restart markers fails
//! at its first. A large image still costs work in proportion to its
//! data: about 64 KB of scan data can describe a 4096 × 4096 gray image
//! of flat blocks, and decoding it takes as long as that image does.
//!
//! # Scratch
//!
//! From a 4-aligned start ([`FIXED_SCRATCH`] counts up to 3 bytes to reach
//! it), with `k` the block side the scale gives:
//!
//! | bytes | holds |
//! |---|---|
//! | 512 | four quantization tables, 16 bits per entry |
//! | 4 × 784 | four Huffman tables: an 8-bit lookup, code counts, symbols |
//! | 256 | the luma block being decoded |
//! | 256 | one MCU's luma pixels, up to 16 × 16 |
//! | 64 | sums carried across MCUs, one per source row |
//! | 4 · out_w | `u32` sums for the output row being gathered |
//! | rows · out_w | the current row of MCUs filtered across: rows = `k` for gray, `k` · luma's vertical factor otherwise |
//!
//! So [`JpegInfo::scratch_bytes`] is `4,227 + (4 + rows) · out_w`, at most
//! `4,227 + 20 · out_w`, whatever the source width. At `out_w = 784` every
//! supported JPEG fits in 19,907 bytes; a 4,000-px-wide 4:2:0 photo fitted
//! to 784 decodes at 1/4 in 10,499.

use super::png::{divide, reciprocal, Spans};
use super::{LumaSink, SinkError};

mod idct;
#[cfg(test)]
mod tests;

/// Widest and tallest image the decoder accepts.
pub const MAX_DIMENSION: u32 = 16_384;
/// Most pixels the decoder accepts: about a 9,000 × 7,000 photo. Sums stay
/// far inside `u32` at any size; the cap bounds the work instead.
pub const MAX_PIXELS: u64 = 1 << 26;
/// Most bytes outside every segment and scan a file may hold in all: junk
/// between segments and stray TEM and RST markers. Common decoders pass
/// over a few with a warning; past this the file is corrupt, rather than
/// read to its end without ever finishing.
pub const MAX_STRAY: u16 = 256;

/// Bits a Huffman code is looked up by at once; longer codes are walked.
const FAST_BITS: u32 = 8;
const FAST_ENTRIES: usize = 1 << FAST_BITS;
/// Huffman tables kept at once, any mix of DC and AC.
const SLOTS: usize = 4;
const SLOT_COUNTS: usize = 2 * FAST_ENTRIES;
const SLOT_VALUES: usize = SLOT_COUNTS + 16;
const SLOT_BYTES: usize = SLOT_VALUES + 256;
const QUANT_AT: usize = 0;
const SLOTS_AT: usize = QUANT_AT + 4 * 64 * 2;
const BLOCK_AT: usize = SLOTS_AT + SLOTS * SLOT_BYTES;
const BLOCK_BYTES: usize = 64 * 4;
const TILE_AT: usize = BLOCK_AT + BLOCK_BYTES;
/// The most luma pixels an MCU holds along a side: two blocks of eight.
const TILE_SIDE: usize = 16;
const TILE_BYTES: usize = TILE_SIDE * TILE_SIDE;
const CARRY_AT: usize = TILE_AT + TILE_BYTES;
const SUMS_AT: usize = CARRY_AT + TILE_SIDE * 4;
const ALIGN_SLACK: usize = 3;
/// Scratch every decode needs before its output rows: tables and the
/// per-MCU work areas, with the slack to align them.
pub const FIXED_SCRATCH: usize = ALIGN_SLACK + SUMS_AT;
/// Bytes of a segment kept to read: all of SOS, SOF for up to three
/// components, DRI, and enough of APP0 and APP14 to recognise them.
const HELD: usize = 16;
/// Units (blocks) an MCU can hold: three components at 2 × 2.
const MAX_UNITS: usize = 12;
const UNIT_LUMA: u8 = 16;

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;
const DRI: u8 = 0xDD;
const DHT: u8 = 0xC4;
const APP0: u8 = 0xE0;
const APP14: u8 = 0xEE;
/// Hierarchical-mode markers: DHP and EXP.
const DHP: u8 = 0xDE;
const EXP: u8 = 0xDF;

/// The natural (row-major) position of each zigzag index. A named static,
/// not a `const`, so it stays in flash: unnamed constants join the merged
/// constant pools, which the ESP32-C3 build keeps in RAM.
static ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Why a JPEG did not decode. Callers show the same placeholder for every
/// variant; they exist for logs and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JpegError {
    /// It does not start with SOI: not a JPEG.
    NotJpeg,
    /// The file breaks the format: a bad segment, an undefined or invalid
    /// table, a restart marker out of place.
    Corrupt,
    /// A valid JPEG this decoder does not handle; see the module notes.
    Unsupported,
    /// Wider or taller than [`MAX_DIMENSION`], or over [`MAX_PIXELS`].
    TooLarge,
    /// The scratch is smaller than [`JpegInfo::scratch_bytes`].
    ScratchTooSmall,
    /// The output size is zero or larger than the image.
    BadOutputSize,
    /// The file, or a scan's data, ended early.
    Truncated,
    /// The sink stopped the decode.
    Sink(SinkError),
}

/// What a frame header (SOFn) says about an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JpegInfo {
    pub width: u32,
    pub height: u32,
    /// The SOF marker: 0xC0 baseline, 0xC1 extended sequential, 0xC2
    /// progressive, 0xC3 lossless, 0xC9 and up arithmetic, and so on.
    pub coding: u8,
    /// Bits per sample.
    pub precision: u8,
    pub components: u8,
    /// Luma's sampling factors (across, down), and the frame's largest.
    /// Both 1 × 1 for gray, whose factors do not matter.
    pub luma: (u8, u8),
    pub max: (u8, u8),
    /// Every component's factors are 1 or 2.
    pub factors_ok: bool,
    /// RGB rather than YCbCr: Adobe's transform 0, or components named R,
    /// G, B in a file without JFIF.
    pub rgb: bool,
}

impl JpegInfo {
    /// `Ok` when [`JpegDecoder`] can decode the image, or the error a decode
    /// would stop with.
    pub fn check(&self) -> Result<(), JpegError> {
        let sequential = self.coding == 0xC0 || self.coding == 0xC1;
        let components = self.components == 1 || self.components == 3;
        // A height of zero waits for a DNL marker, which nothing writes.
        if !sequential
            || self.precision != 8
            || !components
            || !self.factors_ok
            || self.rgb
            || self.height == 0
        {
            return Err(JpegError::Unsupported);
        }
        let pixels = u64::from(self.width) * u64::from(self.height);
        if self.width > MAX_DIMENSION || self.height > MAX_DIMENSION || pixels > MAX_PIXELS {
            return Err(JpegError::TooLarge);
        }
        Ok(())
    }

    pub fn is_supported(&self) -> bool {
        self.check().is_ok()
    }

    /// Pixels a side each 8 × 8 block becomes in a decode to `out_w ×
    /// out_h`: 1, 2, 4 or 8, the smallest at which the scaled image (each
    /// side rounded up, as libjpeg does) is still at least the output size
    /// both ways.
    pub fn block_side(&self, out_w: u32, out_h: u32) -> u32 {
        let mut side = 1;
        while side < 8 && (scaled(self.width, side) < out_w || scaled(self.height, side) < out_h) {
            side <<= 1;
        }
        side
    }

    /// Scratch bytes a decode to `out_w × out_h` needs: `4,227 + (4 + rows)
    /// · out_w`, where rows is [`JpegInfo::block_side`] times luma's
    /// vertical sampling factor, or just the side for gray. It depends on
    /// the output and the scale, never on the source width.
    pub fn scratch_bytes(&self, out_w: u32, out_h: u32) -> usize {
        let rows = u64::from(self.block_side(out_w, out_h)) * u64::from(self.group_blocks());
        let total = FIXED_SCRATCH as u64 + (4 + rows) * u64::from(out_w);
        usize::try_from(total).unwrap_or(usize::MAX)
    }

    /// Luma block rows in a row of MCUs, at most.
    fn group_blocks(&self) -> u32 {
        if self.components == 1 {
            1
        } else {
            u32::from(self.luma.1).clamp(1, 2)
        }
    }
}

/// A side of `side_px` pixels at `block_side` pixels per 8, rounded up.
fn scaled(side_px: u32, block_side: u32) -> u32 {
    (side_px * block_side).div_ceil(8)
}

/// Reads a JPEG's segments up to its frame header, which may come after
/// large APPn segments: an EXIF thumbnail can be 64 KB. Feed it the file
/// from its first byte; a caller that can seek may pass over what
/// [`JpegProbe::skippable`] says without reading it.
#[derive(Clone, Debug)]
pub struct JpegProbe {
    head: Head,
    result: Option<Result<JpegInfo, JpegError>>,
}

impl Default for JpegProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl JpegProbe {
    pub fn new() -> Self {
        Self {
            head: Head::new(true),
            result: None,
        }
    }

    /// Takes the file's next bytes. Some once the frame header is read, or
    /// the file is judged. A progressive or oversized image probes fine;
    /// [`JpegInfo::check`] says whether it will decode.
    pub fn feed(&mut self, mut bytes: &[u8]) -> Option<Result<JpegInfo, JpegError>> {
        while self.result.is_none() && !bytes.is_empty() {
            let (used, event) = match self.head.take(bytes) {
                Ok(step) => step,
                Err(error) => {
                    self.result = Some(Err(error));
                    break;
                }
            };
            bytes = bytes.get(used..).unwrap_or_default();
            self.result = match event {
                Event::Frame => self.head.info().map(Ok),
                // Probing passes over table segments, and a scan before
                // any frame is refused when its segment starts.
                Event::Tables | Event::Scan | Event::End => Some(Err(JpegError::Corrupt)),
                Event::More if used == 0 => Some(Err(JpegError::Corrupt)),
                Event::More => None,
            };
        }
        self.result
    }

    /// Bytes the probe would pass over before it reads again: the rest of
    /// a segment it does not read.
    pub fn skippable(&self) -> u32 {
        if self.head.mark == Mark::Skip && self.result.is_none() {
            u32::from(self.head.left)
        } else {
            0
        }
    }

    /// The caller passed over `n` bytes (at most [`JpegProbe::skippable`])
    /// without feeding them.
    pub fn skip(&mut self, n: u32) {
        if self.head.mark == Mark::Skip {
            let n = n.min(u32::from(self.head.left)) as u16;
            self.head.left -= n;
            if self.head.left == 0 {
                self.head.mark = Mark::Ff;
            }
        }
    }
}

/// Reads the frame header from a prefix of a file. [`JpegError::Truncated`]
/// when the prefix ends first.
pub fn probe(prefix: &[u8]) -> Result<JpegInfo, JpegError> {
    JpegProbe::new()
        .feed(prefix)
        .unwrap_or(Err(JpegError::Truncated))
}

/// Where the marker parser is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    /// Matching SOI: how many of its two bytes have.
    Soi(u8),
    /// Between segments: anything up to the next 0xFF is passed over.
    Ff,
    /// After 0xFF: a marker code, or more 0xFF fill.
    Code,
    /// A segment's length: how many of its two bytes are in.
    Length(u8),
    /// Keeping a segment's first bytes in `held`.
    Held,
    /// Passing over the rest of a segment.
    Skip,
    /// A DQT or DHT body, which the decoder reads into its tables.
    Tables,
    /// A scan's entropy-coded data, which the decoder reads.
    Scan,
    /// After EOI, or after the last row: nothing more is read.
    End,
}

/// What [`Head::take`] stopped for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    /// The bytes ran out.
    More,
    /// A frame header was read.
    Frame,
    /// A table segment's body begins.
    Tables,
    /// A scan header was read; its data follows.
    Scan,
    /// EOI.
    End,
}

/// What the decoder keeps of a frame header besides [`JpegInfo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Frame {
    info: JpegInfo,
    ids: [u8; 3],
    /// Each component's factors, across in the high nibble.
    factors: [u8; 3],
    /// Luma's quantization table.
    luma_table: u8,
}

/// Marker-level parsing, shared by the probe and the decoder: finds the
/// segments, keeps the first bytes of those that matter, and reads the
/// frame header.
#[derive(Clone, Copy, Debug)]
struct Head {
    mark: Mark,
    code: u8,
    /// The current segment's body length, and how much of it is still to
    /// come.
    len: u16,
    left: u16,
    held: [u8; HELD],
    held_len: u8,
    /// MCUs per restart interval, 0 for none.
    restart: u16,
    jfif: bool,
    /// Adobe APP14's transform, or 0xFF without one.
    adobe: u8,
    frame: Option<Frame>,
    /// Stops at the frame header, and passes over table segments.
    probing: bool,
    /// Bytes so far that belong to no segment or scan.
    stray: u16,
}

impl Head {
    fn new(probing: bool) -> Self {
        Self {
            mark: Mark::Soi(0),
            code: 0,
            len: 0,
            left: 0,
            held: [0; HELD],
            held_len: 0,
            restart: 0,
            jfif: false,
            adobe: 0xFF,
            frame: None,
            probing,
            stray: 0,
        }
    }

    /// Counts `n` bytes that belong to no segment or scan.
    fn stray(&mut self, n: u16) -> Result<(), JpegError> {
        self.stray = self.stray.saturating_add(n);
        if self.stray > MAX_STRAY {
            return Err(JpegError::Corrupt);
        }
        Ok(())
    }

    /// The frame header with what is known now about RGB: an APP14 or
    /// APP0 may come after it.
    fn info(&self) -> Option<JpegInfo> {
        let frame = self.frame?;
        let mut info = frame.info;
        info.rgb = info.components == 3
            && (self.adobe == 0 || (self.adobe == 0xFF && !self.jfif && frame.ids == *b"RGB"));
        Some(info)
    }

    /// Reads markers and segments from `bytes` until they run out or
    /// something needs the caller: returns the bytes used and why it
    /// stopped. In [`Mark::Tables`], [`Mark::Scan`] and [`Mark::End`] it
    /// takes nothing.
    fn take(&mut self, bytes: &[u8]) -> Result<(usize, Event), JpegError> {
        let mut used = 0;
        while let Some(&byte) = bytes.get(used) {
            match self.mark {
                Mark::Soi(matched) => {
                    let want = if matched == 0 { 0xFF } else { SOI };
                    if byte != want {
                        return Err(JpegError::NotJpeg);
                    }
                    used += 1;
                    self.mark = if matched == 0 { Mark::Soi(1) } else { Mark::Ff };
                }
                Mark::Ff => {
                    // A few stray bytes between segments are passed over,
                    // as common decoders do.
                    used += 1;
                    if byte == 0xFF {
                        self.mark = Mark::Code;
                    } else {
                        self.stray(1)?;
                    }
                }
                Mark::Code => {
                    used += 1;
                    if let Some(event) = self.marker(byte)? {
                        return Ok((used, event));
                    }
                }
                Mark::Length(0) => {
                    used += 1;
                    self.len = u16::from(byte) << 8;
                    self.mark = Mark::Length(1);
                }
                Mark::Length(_) => {
                    used += 1;
                    let len = self.len | u16::from(byte);
                    if len < 2 {
                        return Err(JpegError::Corrupt);
                    }
                    if let Some(event) = self.begin_segment(len - 2)? {
                        return Ok((used, event));
                    }
                }
                Mark::Held => {
                    let want = usize::from(self.len).min(HELD);
                    let have = usize::from(self.held_len);
                    let rest = bytes.get(used..).unwrap_or_default();
                    let n = rest.len().min(want - have);
                    if let (Some(room), Some(part)) =
                        (self.held.get_mut(have..have + n), rest.get(..n))
                    {
                        room.copy_from_slice(part);
                    }
                    used += n;
                    self.held_len += n as u8;
                    self.left -= n as u16;
                    if usize::from(self.held_len) == want {
                        if let Some(event) = self.end_segment()? {
                            return Ok((used, event));
                        }
                    }
                }
                Mark::Skip => {
                    let n = (bytes.len() - used).min(usize::from(self.left));
                    used += n;
                    self.left -= n as u16;
                    if self.left == 0 {
                        self.mark = Mark::Ff;
                    }
                }
                Mark::Tables | Mark::Scan | Mark::End => break,
            }
        }
        Ok((used, Event::More))
    }

    /// A marker code after 0xFF.
    fn marker(&mut self, code: u8) -> Result<Option<Event>, JpegError> {
        self.code = code;
        if code == 0xFF {
            // Fill before a marker.
        } else if code == SOI {
            return Err(JpegError::Corrupt);
        } else if code == EOI {
            self.mark = Mark::End;
            return Ok(Some(Event::End));
        } else if code == 0x01 || code & 0xF8 == 0xD0 || code == 0x00 {
            // TEM and stray RSTn stand alone, without a length; a stuffed
            // byte outside a scan is stray data.
            self.stray(2)?;
            self.mark = Mark::Ff;
        } else {
            self.mark = Mark::Length(0);
        }
        Ok(None)
    }

    /// A segment of `len` body bytes begins: decides what to do with it.
    fn begin_segment(&mut self, len: u16) -> Result<Option<Event>, JpegError> {
        let code = self.code;
        self.len = len;
        self.left = len;
        self.held_len = 0;
        let sof = code & 0xF0 == 0xC0 && code != DHT && code != 0xC8 && code != 0xCC;
        self.mark = if sof {
            if self.frame.is_some() {
                return Err(JpegError::Corrupt);
            }
            Mark::Held
        } else if code == DQT || code == DHT {
            if self.probing {
                Mark::Skip
            } else {
                self.mark = Mark::Tables;
                return Ok(Some(Event::Tables));
            }
        } else if code == SOS {
            if self.frame.is_none() {
                return Err(JpegError::Corrupt);
            }
            Mark::Held
        } else if code == DRI || code == APP0 || code == APP14 {
            Mark::Held
        } else if code == DHP || code == EXP {
            return Err(JpegError::Unsupported);
        } else {
            Mark::Skip
        };
        if self.mark == Mark::Held && len == 0 {
            return self.end_segment();
        }
        if self.mark == Mark::Skip && len == 0 {
            self.mark = Mark::Ff;
        }
        Ok(None)
    }

    /// The bytes of a segment it keeps are in: reads them. The rest of the
    /// segment, if any, is passed over.
    fn end_segment(&mut self) -> Result<Option<Event>, JpegError> {
        let code = self.code;
        self.mark = if self.left > 0 { Mark::Skip } else { Mark::Ff };
        let held = self.held;
        if code == SOS {
            if self.left > 0 {
                return Err(JpegError::Corrupt);
            }
            self.mark = Mark::Scan;
            return Ok(Some(Event::Scan));
        }
        if code == DRI {
            if self.len != 2 {
                return Err(JpegError::Corrupt);
            }
            self.restart = u16::from_be_bytes([held[0], held[1]]);
        } else if code == APP0 {
            self.jfif |= self.len >= 5 && held[..5] == *b"JFIF\0";
        } else if code == APP14 {
            if self.len >= 12 && held[..5] == *b"Adobe" {
                self.adobe = held[11];
            }
        } else {
            self.frame = Some(self.read_frame()?);
            return Ok(Some(Event::Frame));
        }
        Ok(None)
    }

    /// Reads SOFn from `held`. Validates the fields every frame has, and
    /// each component's for the one and three component frames the
    /// decoder takes; says nothing about support (see [`JpegInfo::check`]).
    fn read_frame(&self) -> Result<Frame, JpegError> {
        let held = &self.held;
        let len = usize::from(self.len);
        if len < 6 {
            return Err(JpegError::Corrupt);
        }
        let count = held[5];
        if len != 6 + 3 * usize::from(count) || count == 0 {
            return Err(JpegError::Corrupt);
        }
        let height = u32::from(u16::from_be_bytes([held[1], held[2]]));
        let width = u32::from(u16::from_be_bytes([held[3], held[4]]));
        if width == 0 {
            return Err(JpegError::Corrupt);
        }
        let mut frame = Frame {
            info: JpegInfo {
                width,
                height,
                coding: self.code,
                precision: held[0],
                components: count,
                luma: (1, 1),
                max: (1, 1),
                factors_ok: true,
                rgb: false,
            },
            ids: [0; 3],
            factors: [0x11; 3],
            luma_table: 0,
        };
        if count != 1 && count != 3 {
            return Ok(frame);
        }
        let mut max = (1, 1);
        for index in 0..usize::from(count) {
            let at = 6 + 3 * index;
            let (id, factors, table) = (held[at], held[at + 1], held[at + 2]);
            let (across, down) = (factors >> 4, factors & 15);
            if !(1..=4).contains(&across) || !(1..=4).contains(&down) || table > 3 {
                return Err(JpegError::Corrupt);
            }
            if frame.ids[..index].contains(&id) {
                return Err(JpegError::Corrupt);
            }
            frame.ids[index] = id;
            frame.factors[index] = factors;
            max = (max.0.max(across), max.1.max(down));
        }
        frame.luma_table = held[8];
        if count == 3 {
            frame.info.luma = (frame.factors[0] >> 4, frame.factors[0] & 15);
            frame.info.max = max;
            frame.info.factors_ok = max.0 <= 2 && max.1 <= 2;
        } else {
            // A lone component's factors cancel out.
            frame.factors[0] = 0x11;
        }
        Ok(frame)
    }
}

/// Where a DQT or DHT body is.
#[derive(Clone, Copy, Debug, Default)]
struct TableState {
    /// Bytes of the current table read, its header byte included; 0 before
    /// its header.
    at: u16,
    /// Bytes of the current table after its header: for DHT, known once
    /// its counts are in.
    size: u16,
    /// The quantization table's id, or the Huffman table's slot.
    target: u8,
    /// 16-bit quantization entries; for DHT, an AC table.
    wide: bool,
    /// A 16-bit entry's high byte.
    high: u8,
}

/// A block's decode, part way: kept across calls so a block can stop at
/// any byte of input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BlockState {
    /// The next coefficient, in zigzag order; 0 is DC.
    coef: u8,
    /// A symbol whose extra bits had not all arrived.
    pending: Option<u8>,
    /// A non-zero AC coefficient was stored.
    ac: bool,
}

/// The scan being decoded.
#[derive(Clone, Copy, Debug, Default)]
struct Scan {
    /// Per scan component: its DC and AC table slots.
    tables: [[u8; 2]; 3],
    preds: [i32; 3],
    /// Each block of an MCU: its scan component (bits 0 and 1), column
    /// (bit 2) and row (bit 3) in the MCU, and [`UNIT_LUMA`].
    units: [u8; MAX_UNITS],
    unit_count: u8,
    unit: u8,
    mcus_x: u32,
    mcus_y: u32,
    mcu_x: u32,
    mcu_y: u32,
    /// MCUs left before the next restart marker, and that marker's number.
    restart_left: u16,
    next_restart: u8,
    block: BlockState,
    /// Luma pixels in an MCU, across and down.
    tile_w: u8,
    tile_h: u8,
    /// 1 where luma is sampled more coarsely than the frame's finest
    /// component, so each luma pixel covers two of the image's.
    double_x: u8,
    double_y: u8,
    /// Luma's quantization table.
    quant: u8,
}

/// The bit reader's state between calls.
#[derive(Clone, Copy, Debug, Default)]
struct BitState {
    /// Bits not yet used, from the top.
    acc: u32,
    have: u32,
    /// How many of the last `have` bits are zeros made up past a marker.
    padded: u32,
    /// The last byte read was 0xFF; the next says what it was.
    ff: bool,
    /// The marker that ended the data, read.
    marker: Option<u8>,
}

/// The bit reader over one call's input.
struct Bits<'i> {
    state: BitState,
    input: &'i [u8],
    pos: usize,
}

impl Bits<'_> {
    /// Tops up to more than 24 bits while input lasts. Past a marker the
    /// data has ended, and zeros stand in, as in libjpeg, so the last
    /// symbols can be looked up by more bits than they have.
    #[inline(always)]
    fn refill(&mut self) {
        let s = &mut self.state;
        while s.have <= 24 {
            let byte = if s.marker.is_some() {
                s.padded += 8;
                0
            } else {
                let Some(&byte) = self.input.get(self.pos) else {
                    return;
                };
                self.pos += 1;
                if s.ff {
                    if byte == 0xFF {
                        // Fill.
                        continue;
                    }
                    s.ff = false;
                    if byte != 0 {
                        s.marker = Some(byte);
                        continue;
                    }
                    0xFF
                } else if byte == 0xFF {
                    s.ff = true;
                    continue;
                } else {
                    byte
                }
            };
            s.acc |= u32::from(byte) << (24 - s.have);
            s.have += 8;
        }
    }

    #[inline(always)]
    fn consume(&mut self, n: u32) {
        let s = &mut self.state;
        s.acc <<= n;
        s.have -= n;
        s.padded = s.padded.min(s.have);
    }

    /// The next Huffman symbol, or None until more input arrives.
    #[inline(always)]
    fn decode(&mut self, table: &Huffman<'_>) -> Result<Option<u8>, JpegError> {
        if self.state.have < 16 {
            self.refill();
        }
        let entry = table.fast[(self.state.acc >> (32 - FAST_BITS)) as usize];
        let len = u32::from(entry >> 8);
        if len != 0 && len <= self.state.have {
            self.consume(len);
            return Ok(Some(entry as u8));
        }
        self.decode_slow(table)
    }

    /// Codes longer than the lookup, a length at a time.
    #[inline(never)]
    fn decode_slow(&mut self, table: &Huffman<'_>) -> Result<Option<u8>, JpegError> {
        let (mut first, mut index) = (0u32, 0u32);
        for (len, &count) in (1..=16u32).zip(table.counts) {
            if len > self.state.have {
                return Ok(None);
            }
            let count = u32::from(count);
            let offset = (self.state.acc >> (32 - len)).wrapping_sub(first);
            if offset < count {
                self.consume(len);
                let at = (index + offset) as usize;
                return table
                    .values
                    .get(at)
                    .map(|&value| Some(value))
                    .ok_or(JpegError::Corrupt);
            }
            index += count;
            first = (first + count) << 1;
        }
        Err(JpegError::Corrupt)
    }

    /// The `size` (1 to 15) extra bits of a coefficient, as its value, or
    /// None until more input arrives.
    #[inline(always)]
    fn receive(&mut self, size: u32) -> Option<i32> {
        if self.state.have < size {
            self.refill();
            if self.state.have < size {
                return None;
            }
        }
        let raw = (self.state.acc >> (32 - size)) as i32;
        self.consume(size);
        // Values whose top bit is clear are negative.
        Some(if raw < 1 << (size - 1) {
            raw - (1 << size) + 1
        } else {
            raw
        })
    }

    /// Drops what is left of the current byte's bits, as at the end of a
    /// restart interval or a scan.
    fn align(&mut self) {
        self.state.acc = 0;
        self.state.have = 0;
        self.state.padded = 0;
    }

    /// Takes the restart marker that ends an interval, which must be
    /// `RSTn` with `n` the `expected` number in the 0 to 7 cycle, after
    /// nothing but fill. Anything else is corrupt: another marker, the
    /// wrong number, or data where the marker belongs (decoders that try to
    /// resynchronise would guess). Ok(false) until it arrives.
    fn restart(&mut self, expected: u8) -> Result<bool, JpegError> {
        // Only the last byte's padding may be left of the interval: a
        // whole byte more read ahead is data where the marker belongs.
        if self.state.have.saturating_sub(self.state.padded) >= 8 {
            return Err(JpegError::Corrupt);
        }
        self.align();
        loop {
            if let Some(code) = self.state.marker {
                if code != 0xD0 | (expected & 7) {
                    return Err(JpegError::Corrupt);
                }
                self.state.marker = None;
                return Ok(true);
            }
            let Some(&byte) = self.input.get(self.pos) else {
                return Ok(false);
            };
            self.pos += 1;
            if byte == 0xFF {
                // The marker's first byte, or fill before it.
                self.state.ff = true;
            } else if self.state.ff && byte != 0 {
                self.state.ff = false;
                self.state.marker = Some(byte);
            } else {
                return Err(JpegError::Corrupt);
            }
        }
    }
}

/// A Huffman table in scratch.
struct Huffman<'t> {
    /// By the next [`FAST_BITS`] bits: the code's length in the high byte
    /// and its symbol in the low, or 0 for a longer code.
    fast: &'t [u16; FAST_ENTRIES],
    counts: &'t [u8; 16],
    values: &'t [u8; 256],
}

impl<'t> Huffman<'t> {
    fn of(scratch: &'t [u8], at: usize) -> Result<Self, JpegError> {
        let bytes = scratch.get(at..at + SLOT_BYTES).ok_or(JpegError::Corrupt)?;
        let (fast, rest) = bytes.split_at(SLOT_COUNTS);
        let (counts, values) = rest.split_at(SLOT_VALUES - SLOT_COUNTS);
        Ok(Self {
            fast: bytemuck::try_from_bytes(fast).map_err(|_| JpegError::Corrupt)?,
            counts: counts.try_into().map_err(|_| JpegError::Corrupt)?,
            values: values.try_into().map_err(|_| JpegError::Corrupt)?,
        })
    }
}

/// Builds a slot's lookup from its counts and symbols, checking that the
/// codes fit their lengths and that a DC table's sizes are at most 15.
fn build_table(slot: &mut [u8], dc: bool) -> Result<(), JpegError> {
    let (fast, rest) = slot
        .get_mut(..SLOT_BYTES)
        .ok_or(JpegError::Corrupt)?
        .split_at_mut(SLOT_COUNTS);
    let fast: &mut [u16] = bytemuck::try_cast_slice_mut(fast).map_err(|_| JpegError::Corrupt)?;
    let (counts, values) = rest.split_at(SLOT_VALUES - SLOT_COUNTS);
    fast.fill(0);
    let (mut code, mut index) = (0u32, 0usize);
    for (len, &count) in (1..=16u32).zip(counts) {
        for _ in 0..count {
            let value = values.get(index).copied().ok_or(JpegError::Corrupt)?;
            if code >= 1 << len || (dc && value > 15) {
                return Err(JpegError::Corrupt);
            }
            if len <= FAST_BITS {
                let spread = FAST_BITS - len;
                let from = (code << spread) as usize;
                let entries = fast
                    .get_mut(from..from + (1 << spread))
                    .ok_or(JpegError::Corrupt)?;
                entries.fill((len as u16) << 8 | u16::from(value));
            }
            code += 1;
            index += 1;
        }
        code <<= 1;
    }
    Ok(())
}

/// Decodes the rest of a block's coefficients: into `block` in natural
/// order for luma (it starts zeroed), or read past for chroma. Ok(true)
/// once the block is complete, Ok(false) when the input ran out first.
#[inline(never)]
fn decode_block(
    bits: &mut Bits<'_>,
    dc: &Huffman<'_>,
    ac: &Huffman<'_>,
    mut block: Option<&mut [i32; 64]>,
    pred: &mut i32,
    state: &mut BlockState,
) -> Result<bool, JpegError> {
    while state.coef < 64 {
        let k = state.coef;
        let symbol = match state.pending {
            Some(symbol) => symbol,
            None => {
                let table = if k == 0 { dc } else { ac };
                match bits.decode(table)? {
                    Some(symbol) => symbol,
                    None => return Ok(false),
                }
            }
        };
        let (run, size) = if k == 0 {
            (0, u32::from(symbol))
        } else {
            (symbol >> 4, u32::from(symbol & 15))
        };
        if size == 0 {
            state.pending = None;
            if k == 0 {
                if let Some(block) = block.as_deref_mut() {
                    block[0] = i32::from(*pred as i16);
                }
                state.coef = 1;
            } else if run == 15 {
                // Sixteen zeros.
                state.coef = k + 16;
            } else {
                // End of block.
                state.coef = 64;
            }
            continue;
        }
        let Some(value) = bits.receive(size) else {
            state.pending = Some(symbol);
            return Ok(false);
        };
        state.pending = None;
        if k == 0 {
            // A DC value is stored as a 16-bit coefficient, as in libjpeg.
            *pred = pred.wrapping_add(value);
            if let Some(block) = block.as_deref_mut() {
                block[0] = i32::from(*pred as i16);
            }
            state.coef = 1;
        } else {
            let at = k + run;
            if let (Some(block), Some(&natural)) =
                (block.as_deref_mut(), ZIGZAG.get(usize::from(at)))
            {
                block[usize::from(natural)] = value;
                state.ac = true;
            }
            // Past the 64th coefficient the block simply ends.
            state.coef = at + 1;
        }
    }
    Ok(true)
}

/// The box filter's place along a row of MCUs: the span of scaled columns
/// the next output column takes, and how much of it is still to come.
#[derive(Clone, Copy, Debug)]
struct Columns {
    spans: Spans,
    left: u32,
    long: bool,
    x: u32,
}

impl Columns {
    fn start(mut spans: Spans) -> Self {
        let long = spans.next_is_long();
        Self {
            left: spans.short + u32::from(long),
            spans,
            long,
            x: 0,
        }
    }
}

/// Decodes one baseline or extended sequential JPEG, fed in chunks of any
/// size, to `out_w × out_h` gray rows.
///
/// Feed the file from its first byte with [`JpegDecoder::feed`], then call
/// [`JpegDecoder::finish`]. Rows reach the sink as soon as their source
/// rows are in. The first error ends the decode, and every later call
/// returns it again.
pub struct JpegDecoder<'a> {
    scratch: &'a mut [u8],
    /// Where the 4-aligned layout starts in `scratch`.
    base: usize,
    out_w: u32,
    out_h: u32,
    failed: Option<JpegError>,
    head: Head,
    /// A scan without luma is being passed over.
    skipping: bool,
    /// Bit per quantization table id: defined.
    quant_defined: u8,
    /// Slot + 1 of each Huffman table (DC 0 to 3, then AC 0 to 3), 0 for
    /// none yet.
    slot_of: [u8; 8],
    slots_used: u8,
    /// Bit per slot: its table is complete.
    slots_defined: u8,
    table: TableState,
    /// The frame's geometry is set and its buffers carved.
    started: bool,
    /// Luma's scan has begun.
    luma_seen: bool,
    /// Scaled pixels per block side, and its log2.
    side: u8,
    shift: u8,
    scaled_w: u32,
    scaled_h: u32,
    scan: Scan,
    bits: BitState,
    /// Scaled columns per output column, fresh for every row of MCUs.
    col_spans: Spans,
    cols: Columns,
    /// The first scaled column of the next MCU.
    next_col: u32,
    /// [`reciprocal`] of a short and a long span of columns.
    col_inverse: [u32; 2],
    /// Scaled rows per output row, advanced once per output row.
    rows: Spans,
    row_inverse: [u32; 2],
    band_rows: u32,
    band_left: u32,
    /// Scaled rows already filtered down.
    next_row: u32,
    out_y: u32,
    /// Blocks decoded, so tests can bound the work.
    #[cfg(test)]
    blocks: u64,
}

impl<'a> JpegDecoder<'a> {
    /// A decoder emitting exactly `out_w × out_h`, a size the caller already
    /// fitted (see [`super::fit_inside`]); it is never refitted. Buffers are
    /// carved from `scratch` as the tables and the frame header arrive, and
    /// everything the decoder reads is written or cleared first, so the
    /// scratch may hold anything.
    pub fn new(scratch: &'a mut [u8], out_w: u32, out_h: u32) -> Result<Self, JpegError> {
        if out_w == 0 || out_h == 0 {
            return Err(JpegError::BadOutputSize);
        }
        let base = bytemuck::pod_align_to::<u8, u32>(scratch).0.len();
        Ok(Self {
            scratch,
            base,
            out_w,
            out_h,
            failed: None,
            head: Head::new(false),
            skipping: false,
            quant_defined: 0,
            slot_of: [0; 8],
            slots_used: 0,
            slots_defined: 0,
            table: TableState::default(),
            started: false,
            luma_seen: false,
            side: 8,
            shift: 3,
            scaled_w: 0,
            scaled_h: 0,
            scan: Scan::default(),
            bits: BitState::default(),
            col_spans: Spans::new(1, 1),
            cols: Columns::start(Spans::new(1, 1)),
            next_col: 0,
            col_inverse: [0; 2],
            rows: Spans::new(1, 1),
            row_inverse: [0; 2],
            band_rows: 0,
            band_left: 0,
            next_row: 0,
            out_y: 0,
            #[cfg(test)]
            blocks: 0,
        })
    }

    /// The image's frame header, once it has been fed.
    pub fn info(&self) -> Option<JpegInfo> {
        self.head.info()
    }

    /// Takes the next bytes of the file. The work a call does is bounded by
    /// the bytes it is given (see the module notes), so a caller that
    /// checks for input between calls bounds how long a cancel waits by its
    /// chunk size.
    pub fn feed(&mut self, bytes: &[u8], sink: &mut impl LumaSink) -> Result<(), JpegError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.take(bytes, sink);
        self.failed = result.err();
        result
    }

    /// Ends the decode: succeeds if every output row went to the sink. Rows
    /// the sink took are provisional until this returns `Ok`.
    pub fn finish(&mut self) -> Result<(), JpegError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = if self.is_complete() {
            Ok(())
        } else {
            Err(JpegError::Truncated)
        };
        self.failed = result.err();
        result
    }

    /// Whether every output row has gone to the sink. Once it has, the rest
    /// of the file need not be read: feeding it does nothing.
    pub fn is_complete(&self) -> bool {
        self.started && self.out_y >= self.out_h
    }

    fn take(&mut self, mut bytes: &[u8], sink: &mut dyn LumaSink) -> Result<(), JpegError> {
        while !bytes.is_empty() {
            let mark = self.head.mark;
            let used = match mark {
                Mark::Tables => self.table_bytes(bytes)?,
                Mark::Scan if self.skipping => self.skip_scan(bytes)?,
                Mark::Scan => self.entropy(bytes, sink)?,
                Mark::End => return self.ended(),
                _ => {
                    let (used, event) = self.head.take(bytes)?;
                    match event {
                        Event::Frame => self.start_frame()?,
                        Event::Tables => self.start_tables()?,
                        Event::Scan => self.start_scan()?,
                        Event::End | Event::More => {}
                    }
                    used
                }
            };
            if self.is_complete() {
                // Nothing after the last row is read.
                self.head.mark = Mark::End;
                return Ok(());
            }
            if self.head.mark == Mark::End {
                // EOI with rows still to come: nothing after it is read.
                return self.ended();
            }
            if used == 0 && self.head.mark == mark {
                // Every step takes a byte or moves on; stop rather than
                // spin if that ever fails to hold.
                return Err(JpegError::Corrupt);
            }
            bytes = bytes.get(used..).unwrap_or_default();
        }
        Ok(())
    }

    /// The file has ended (EOI, or the last row): fine once every row is
    /// out, and short otherwise.
    fn ended(&self) -> Result<(), JpegError> {
        if self.is_complete() {
            Ok(())
        } else {
            Err(JpegError::Truncated)
        }
    }

    /// A DQT or DHT body begins: its tables need their part of the scratch.
    fn start_tables(&mut self) -> Result<(), JpegError> {
        if self.scratch.len() < self.base + SUMS_AT {
            return Err(JpegError::ScratchTooSmall);
        }
        self.table = TableState::default();
        if self.head.left == 0 {
            self.head.mark = Mark::Ff;
        }
        Ok(())
    }

    fn table_bytes(&mut self, bytes: &[u8]) -> Result<usize, JpegError> {
        let n = bytes.len().min(usize::from(self.head.left));
        for &byte in bytes.get(..n).unwrap_or_default() {
            if self.head.code == DQT {
                self.quant_byte(byte)?;
            } else {
                self.huffman_byte(byte)?;
            }
        }
        self.head.left -= n as u16;
        if self.head.left == 0 {
            if self.table.at != 0 {
                // The segment ended inside a table.
                return Err(JpegError::Corrupt);
            }
            self.head.mark = Mark::Ff;
        }
        Ok(n)
    }

    fn quant_byte(&mut self, byte: u8) -> Result<(), JpegError> {
        let table = &mut self.table;
        if table.at == 0 {
            let (precision, id) = (byte >> 4, byte & 15);
            if precision > 1 || id > 3 {
                return Err(JpegError::Corrupt);
            }
            *table = TableState {
                at: 1,
                size: 64 << precision,
                target: id,
                wide: precision == 1,
                high: 0,
            };
            self.quant_defined &= !(1 << id);
            return Ok(());
        }
        let index = usize::from(table.at - 1);
        table.at += 1;
        let (entry, value) = if table.wide {
            if index & 1 == 0 {
                table.high = byte;
                return Ok(());
            }
            (index >> 1, u16::from_be_bytes([table.high, byte]))
        } else {
            (index, u16::from(byte))
        };
        let natural = usize::from(ZIGZAG.get(entry).copied().unwrap_or(0));
        let at = self.base + QUANT_AT + 2 * (64 * usize::from(table.target) + natural);
        if let Some(slot) = self.scratch.get_mut(at..at + 2) {
            slot.copy_from_slice(&value.to_ne_bytes());
        }
        if table.at - 1 == table.size {
            self.quant_defined |= 1 << table.target;
            table.at = 0;
        }
        Ok(())
    }

    fn huffman_byte(&mut self, byte: u8) -> Result<(), JpegError> {
        if self.table.at == 0 {
            let (class, id) = (byte >> 4, byte & 15);
            if class > 1 || id > 3 {
                return Err(JpegError::Corrupt);
            }
            let key = usize::from(class * 4 + id);
            let slot = match self.slot_of[key] {
                0 => {
                    if usize::from(self.slots_used) == SLOTS {
                        return Err(JpegError::Unsupported);
                    }
                    self.slots_used += 1;
                    self.slot_of[key] = self.slots_used;
                    self.slots_used - 1
                }
                taken => taken - 1,
            };
            self.slots_defined &= !(1 << slot);
            self.table = TableState {
                at: 1,
                size: 16,
                target: slot,
                wide: class == 1,
                high: 0,
            };
            return Ok(());
        }
        let table = &mut self.table;
        let index = usize::from(table.at - 1);
        table.at += 1;
        let at = self.base + SLOTS_AT + usize::from(table.target) * SLOT_BYTES;
        let slot = self
            .scratch
            .get_mut(at..at + SLOT_BYTES)
            .ok_or(JpegError::Corrupt)?;
        if let Some(stored) = slot.get_mut(SLOT_COUNTS + index) {
            *stored = byte;
        }
        if index == 15 {
            let counts = slot
                .get(SLOT_COUNTS..SLOT_VALUES)
                .ok_or(JpegError::Corrupt)?;
            let total: u16 = counts.iter().map(|&count| u16::from(count)).sum();
            if total > 256 {
                return Err(JpegError::Corrupt);
            }
            table.size = 16 + total;
        }
        if table.at - 1 == table.size {
            build_table(slot, !table.wide)?;
            self.slots_defined |= 1 << table.target;
            table.at = 0;
        }
        Ok(())
    }

    /// The frame header is in: checks the image against the limits and the
    /// scratch, picks the scale, and clears the buffers after the tables.
    fn start_frame(&mut self) -> Result<(), JpegError> {
        let info = self.head.info().ok_or(JpegError::Corrupt)?;
        info.check()?;
        if self.out_w > info.width || self.out_h > info.height {
            return Err(JpegError::BadOutputSize);
        }
        if self.scratch.len() < info.scratch_bytes(self.out_w, self.out_h) {
            return Err(JpegError::ScratchTooSmall);
        }
        let side = info.block_side(self.out_w, self.out_h);
        self.side = side as u8;
        self.shift = side.trailing_zeros() as u8;
        self.scaled_w = scaled(info.width, side);
        self.scaled_h = scaled(info.height, side);
        let rows = side * info.group_blocks();
        let end = self.base + SUMS_AT + ((4 + rows) * self.out_w) as usize;
        self.scratch
            .get_mut(self.base + BLOCK_AT..end)
            .ok_or(JpegError::ScratchTooSmall)?
            .fill(0);
        self.col_spans = Spans::new(self.scaled_w, self.out_w);
        self.cols = Columns::start(self.col_spans);
        self.rows = Spans::new(self.scaled_h, self.out_h);
        // The only divisions of the decode, four per image.
        let (cols, rows) = (self.col_spans.short, self.rows.short);
        self.col_inverse = [reciprocal(cols), reciprocal(cols + 1)];
        self.row_inverse = [reciprocal(rows), reciprocal(rows + 1)];
        self.band_rows = self.rows.next_len();
        self.band_left = self.band_rows;
        self.started = true;
        Ok(())
    }

    /// A scan header is in: sets up its MCUs, or passes over a scan
    /// without luma.
    fn start_scan(&mut self) -> Result<(), JpegError> {
        let (Some(frame), Some(info)) = (self.head.frame, self.head.info()) else {
            return Err(JpegError::Corrupt);
        };
        // An APP14 after the frame header can still make it RGB.
        info.check()?;
        let held = self.head.held;
        let count = usize::from(held[0]);
        let components = usize::from(info.components);
        if count == 0 || count > components || usize::from(self.head.len) != 4 + 2 * count {
            return Err(JpegError::Corrupt);
        }
        self.bits = BitState::default();
        let mut scan = Scan::default();
        let mut members = [0usize; 3];
        let mut luma = false;
        for index in 0..count {
            let id = held[1 + 2 * index];
            let tables = held[2 + 2 * index];
            let component = frame.ids[..components]
                .iter()
                .position(|&known| known == id)
                .ok_or(JpegError::Corrupt)?;
            if members[..index].contains(&component) {
                return Err(JpegError::Corrupt);
            }
            members[index] = component;
            luma |= component == 0;
            scan.tables[index] = [self.slot(tables >> 4, 0)?, self.slot(tables & 15, 4)?];
        }
        if !luma {
            self.skipping = true;
            return Ok(());
        }
        if self.luma_seen || self.quant_defined & (1 << frame.luma_table) == 0 {
            return Err(JpegError::Corrupt);
        }
        self.luma_seen = true;
        self.skipping = false;
        let (width, height) = (info.width, info.height);
        let (max_x, max_y) = (u32::from(info.max.0), u32::from(info.max.1));
        let (luma_x, luma_y) = (u32::from(info.luma.0), u32::from(info.luma.1));
        let side = u32::from(self.side);
        let units = &mut scan.units;
        if count > 1 {
            // Interleaved: MCUs cover the finest component's 8 × 8 blocks
            // at the coarsest sampling.
            scan.mcus_x = width.div_ceil(8 * max_x);
            scan.mcus_y = height.div_ceil(8 * max_y);
            let mut used = 0;
            for (index, &component) in members[..count].iter().enumerate() {
                let factors = frame.factors[component];
                for down in 0..factors & 15 {
                    for across in 0..factors >> 4 {
                        let unit = units.get_mut(used).ok_or(JpegError::Corrupt)?;
                        *unit = index as u8 | across << 2 | down << 3;
                        if component == 0 {
                            *unit |= UNIT_LUMA;
                        }
                        used += 1;
                    }
                }
            }
            scan.unit_count = used as u8;
            scan.tile_w = (luma_x * side) as u8;
            scan.tile_h = (luma_y * side) as u8;
        } else {
            // One component: an MCU is one of its blocks.
            let luma_w = (width * luma_x).div_ceil(max_x);
            let luma_h = (height * luma_y).div_ceil(max_y);
            scan.mcus_x = luma_w.div_ceil(8);
            scan.mcus_y = luma_h.div_ceil(8);
            units[0] = UNIT_LUMA;
            scan.unit_count = 1;
            scan.tile_w = side as u8;
            scan.tile_h = side as u8;
        }
        scan.double_x = u8::from(max_x > luma_x);
        scan.double_y = u8::from(max_y > luma_y);
        scan.quant = frame.luma_table;
        scan.restart_left = self.head.restart;
        self.scan = scan;
        Ok(())
    }

    /// The slot of the Huffman table `id` of a class (0 DC, 4 AC), which
    /// must be complete.
    fn slot(&self, id: u8, class: u8) -> Result<u8, JpegError> {
        let slot = self
            .slot_of
            .get(usize::from(class + id))
            .copied()
            .filter(|&slot| id < 4 && slot != 0)
            .ok_or(JpegError::Corrupt)?
            - 1;
        if self.slots_defined & (1 << slot) == 0 {
            return Err(JpegError::Corrupt);
        }
        Ok(slot)
    }

    /// Passes over a scan without luma to the marker that ends it.
    fn skip_scan(&mut self, bytes: &[u8]) -> Result<usize, JpegError> {
        for (at, &byte) in bytes.iter().enumerate() {
            if self.bits.ff && byte != 0xFF && byte != 0 && byte & 0xF8 != 0xD0 {
                self.bits = BitState::default();
                self.head.mark = Mark::Ff;
                self.head.marker(byte)?;
                return Ok(at + 1);
            }
            self.bits.ff = byte == 0xFF;
        }
        Ok(bytes.len())
    }

    /// Decodes a scan's data from `input`; returns the bytes used: all of
    /// them unless the scan ended or the image is complete.
    fn entropy(&mut self, input: &[u8], sink: &mut dyn LumaSink) -> Result<usize, JpegError> {
        let mut bits = Bits {
            state: self.bits,
            input,
            pos: 0,
        };
        let result = self.decode_mcus(&mut bits, sink);
        self.bits = bits.state;
        result?;
        if self.scan.mcu_y >= self.scan.mcus_y && !self.is_complete() {
            // The scan is over: whatever bits are left pad its last byte.
            let marker = self.bits.marker.take();
            let ff = self.bits.ff;
            self.bits = BitState::default();
            self.head.mark = if ff { Mark::Code } else { Mark::Ff };
            if let Some(code) = marker {
                self.head.marker(code)?;
            }
        }
        Ok(bits.pos)
    }

    fn decode_mcus(
        &mut self,
        bits: &mut Bits<'_>,
        sink: &mut dyn LumaSink,
    ) -> Result<(), JpegError> {
        let restart = self.head.restart;
        loop {
            let scan = &mut self.scan;
            if scan.unit == 0 && scan.block == BlockState::default() {
                if scan.mcu_y >= scan.mcus_y {
                    return Ok(());
                }
                if restart != 0 && scan.restart_left == 0 {
                    if !bits.restart(scan.next_restart)? {
                        return Ok(());
                    }
                    scan.preds = [0; 3];
                    scan.restart_left = restart;
                    scan.next_restart = (scan.next_restart + 1) & 7;
                }
                // An MCU needs data of its own: one that would start on
                // made-up bits (the data ran into a marker with MCUs still
                // to come, or a restart interval is empty) fails.
                bits.refill();
                if bits.state.marker.is_some() && bits.state.have <= bits.state.padded {
                    return Err(JpegError::Truncated);
                }
            }
            let unit = scan.units[usize::from(scan.unit) % MAX_UNITS];
            let member = usize::from(unit & 3) % 3;
            let luma = unit & UNIT_LUMA != 0;
            let [dc, ac] = scan.tables[member];
            let (tables, work) = self.scratch.split_at_mut(self.base + BLOCK_AT);
            let dc = Huffman::of(tables, self.base + SLOTS_AT + usize::from(dc) * SLOT_BYTES)?;
            let ac = Huffman::of(tables, self.base + SLOTS_AT + usize::from(ac) * SLOT_BYTES)?;
            let block: &mut [i32; 64] = work
                .get_mut(..BLOCK_BYTES)
                .and_then(|bytes| bytemuck::try_from_bytes_mut(bytes).ok())
                .ok_or(JpegError::Corrupt)?;
            let done = decode_block(
                bits,
                &dc,
                &ac,
                luma.then_some(block),
                &mut scan.preds[member],
                &mut scan.block,
            )?;
            if !done {
                return Ok(());
            }
            #[cfg(test)]
            {
                self.blocks += 1;
            }
            if luma {
                self.transform(unit)?;
            }
            let scan = &mut self.scan;
            scan.block = BlockState::default();
            scan.unit += 1;
            if scan.unit >= scan.unit_count {
                scan.unit = 0;
                scan.restart_left = scan.restart_left.wrapping_sub(1);
                self.mcu_done(sink)?;
                if self.is_complete() {
                    return Ok(());
                }
            }
        }
    }

    /// Writes the luma block's pixels into the MCU's tile.
    fn transform(&mut self, unit: u8) -> Result<(), JpegError> {
        let side = usize::from(self.side);
        let stride = usize::from(self.scan.tile_w);
        let at = usize::from(unit >> 3 & 1) * side * stride + usize::from(unit >> 2 & 1) * side;
        let (tables, work) = self.scratch.split_at_mut(self.base + BLOCK_AT);
        let quant_at = self.base + QUANT_AT + 128 * usize::from(self.scan.quant % 4);
        let quant: &[u16; 64] = tables
            .get(quant_at..quant_at + 128)
            .and_then(|bytes| bytemuck::try_from_bytes(bytes).ok())
            .ok_or(JpegError::Corrupt)?;
        let (block, rest) = work
            .split_at_mut_checked(BLOCK_BYTES)
            .ok_or(JpegError::Corrupt)?;
        let block: &mut [i32; 64] =
            bytemuck::try_from_bytes_mut(block).map_err(|_| JpegError::Corrupt)?;
        let tile = rest.get_mut(at..TILE_BYTES).ok_or(JpegError::Corrupt)?;
        idct::transform(
            block,
            quant,
            self.scan.block.ac,
            u32::from(self.side),
            tile,
            stride,
        );
        Ok(())
    }

    /// An MCU is complete: filters its luma across into the row buffer, and
    /// down once its row of MCUs is complete.
    fn mcu_done(&mut self, sink: &mut dyn LumaSink) -> Result<(), JpegError> {
        self.filter_across()?;
        self.scan.mcu_x += 1;
        if self.scan.mcu_x >= self.scan.mcus_x {
            self.scan.mcu_x = 0;
            self.scan.mcu_y += 1;
            self.filter_down(sink)?;
        }
        Ok(())
    }

    /// Adds the tile's pixels, each row into its own carried sum, across
    /// the scaled columns it covers; every output column a row completes
    /// takes its mean. Columns past the image's are dropped.
    fn filter_across(&mut self) -> Result<(), JpegError> {
        let scan = self.scan;
        let (tile_w, tile_h) = (usize::from(scan.tile_w), usize::from(scan.tile_h));
        let double = u32::from(scan.double_x);
        let first = self.next_col;
        self.next_col = first + (u32::from(scan.tile_w) << double);
        let count = (u32::from(scan.tile_w) << double).min(self.scaled_w.saturating_sub(first));
        if count == 0 {
            return Ok(());
        }
        let out_w = self.out_w as usize;
        let short = self.col_spans.short;
        let [short_inverse, long_inverse] = self.col_inverse;
        let region = self
            .scratch
            .get_mut(self.base + TILE_AT..)
            .ok_or(JpegError::Corrupt)?;
        let (tile, rest) = region
            .split_at_mut_checked(TILE_BYTES)
            .ok_or(JpegError::Corrupt)?;
        let (carry, rest) = rest
            .split_at_mut_checked(4 * TILE_SIDE)
            .ok_or(JpegError::Corrupt)?;
        let carry: &mut [u32] =
            bytemuck::try_cast_slice_mut(carry).map_err(|_| JpegError::Corrupt)?;
        let means = rest.get_mut(4 * out_w..).ok_or(JpegError::Corrupt)?;
        let start = self.cols;
        let mut end = start;
        for (row, sum) in carry.iter_mut().enumerate().take(tile_h) {
            let pixels = tile
                .get(row * tile_w..(row + 1) * tile_w)
                .unwrap_or_default();
            let out = means
                .get_mut(row * out_w..(row + 1) * out_w)
                .unwrap_or_default();
            let mut at = start;
            let mut total = *sum;
            let mut column = 0;
            while column < count {
                let n = at.left.min(count - column);
                for scaled_x in column..column + n {
                    let pixel = pixels.get((scaled_x >> double) as usize).copied();
                    total += u32::from(pixel.unwrap_or(0));
                }
                column += n;
                at.left -= n;
                if at.left == 0 {
                    let (span, inverse) = if at.long {
                        (short + 1, long_inverse)
                    } else {
                        (short, short_inverse)
                    };
                    if let Some(slot) = out.get_mut(at.x as usize) {
                        *slot = divide(total + span / 2, span, inverse) as u8;
                    }
                    total = 0;
                    at.x += 1;
                    at.long = at.spans.next_is_long();
                    at.left = short + u32::from(at.long);
                }
            }
            *sum = total;
            end = at;
        }
        self.cols = end;
        Ok(())
    }

    /// A row of MCUs is complete: adds each of its rows (each twice where
    /// luma is sampled coarser down) into the output row's sums, emitting
    /// every output row whose band of rows is done. Rows past the image's
    /// are dropped.
    fn filter_down(&mut self, sink: &mut dyn LumaSink) -> Result<(), JpegError> {
        let double = u32::from(self.scan.double_y);
        let height = u32::from(self.scan.tile_h) << double;
        let first = self.next_row;
        self.next_row = first + height;
        let count = height.min(self.scaled_h.saturating_sub(first));
        let out_w = self.out_w as usize;
        for row in 0..count {
            let source = (row >> double) as usize;
            let region = self
                .scratch
                .get_mut(self.base + SUMS_AT..)
                .ok_or(JpegError::Corrupt)?;
            let (sums, means) = region
                .split_at_mut_checked(4 * out_w)
                .ok_or(JpegError::Corrupt)?;
            let sums: &mut [u32] =
                bytemuck::try_cast_slice_mut(sums).map_err(|_| JpegError::Corrupt)?;
            let means = means
                .get(source * out_w..(source + 1) * out_w)
                .ok_or(JpegError::Corrupt)?;
            for (sum, &mean) in sums.iter_mut().zip(means) {
                *sum += u32::from(mean);
            }
            self.band_left = self.band_left.saturating_sub(1);
            if self.band_left == 0 && self.out_y < self.out_h {
                self.emit_row(sink)?;
                if self.is_complete() {
                    return Ok(());
                }
            }
        }
        // The next row of MCUs starts from the left, with nothing carried.
        self.cols = Columns::start(self.col_spans);
        self.next_col = 0;
        if let Some(carry) = self
            .scratch
            .get_mut(self.base + CARRY_AT..self.base + SUMS_AT)
        {
            carry.fill(0);
        }
        Ok(())
    }

    /// Turns the sums into means, in place over the sums' first bytes, hands
    /// the row to the sink, and clears the sums for the next band.
    fn emit_row(&mut self, sink: &mut dyn LumaSink) -> Result<(), JpegError> {
        let out_w = self.out_w as usize;
        let at = self.base + SUMS_AT;
        let Some(sums) = self
            .scratch
            .get_mut(at..at + 4 * out_w)
            .and_then(|bytes| bytemuck::try_cast_slice_mut::<u8, u32>(bytes).ok())
        else {
            return Err(JpegError::Corrupt);
        };
        let band = self.band_rows;
        let inverse = self.row_inverse[usize::from(band > self.rows.short)];
        // Means pack four to a word over sums already read: word `w` takes
        // the means of sums 4w to 4w + 3, so mean k lands on byte k.
        for w in 0..out_w.div_ceil(4) {
            let mut word = [0u8; 4];
            for (lane, byte) in word.iter_mut().enumerate() {
                if let Some(&sum) = sums.get(4 * w + lane) {
                    *byte = divide(sum + band / 2, band, inverse) as u8;
                }
            }
            if let Some(slot) = sums.get_mut(w) {
                *slot = u32::from_ne_bytes(word);
            }
        }
        let row = bytemuck::cast_slice::<u32, u8>(sums);
        sink.row(self.out_y, row.get(..out_w).unwrap_or_default())
            .map_err(JpegError::Sink)?;
        sums.fill(0);
        self.out_y += 1;
        if self.out_y < self.out_h {
            self.band_rows = self.rows.next_len();
            self.band_left = self.band_rows;
        }
        Ok(())
    }
}
