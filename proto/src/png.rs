//! Streaming PNG decoder for embedded EPUB images.
//!
//! The device has no heap in the reading path and roughly 400 KB of DRAM, so
//! a decoded 800×480 frame (0.9–1.5 MB) can never sit in RAM. This decoder
//! eats the PNG file in arbitrary byte chunks and emits one downsampled
//! grayscale row at a time to a [`LumaSink`], keeping only two source
//! scanlines, one luma row, and one output row alive. Its buffers are carved
//! from a caller-provided scratch slice — firmware hands it the idle EPUB
//! XHTML window during the image decode pass — and the caller supplies the
//! inflate state (the ZIP `InflateState`, idle at that moment), so the
//! decoder costs no static memory and no heap.
//!
//! Accepted inputs are deliberately narrow, matching what EPUB illustration
//! PNGs actually contain: 8-bit non-interlaced grayscale, RGB, and palette
//! (transparency via tRNS, composited onto the white page). Alpha-carrying
//! color types, interlacing, 16-bit samples, and oversized images are
//! rejected up front so callers can fall back to the `alt`-text placeholder.

use core::convert::TryInto;
use miniz_oxide::inflate::stream::{inflate, InflateState};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

/// Widest source image column the decoder will touch: one and a half times
/// the X4 panel width, enough for the box filter to average while keeping
/// the two static scanlines small. Wider images fall back to the
/// placeholder instead of costing more static memory.
pub const PNG_MAX_SOURCE_WIDTH: u32 = 792;

/// Widest row the decoder will emit. Matches the X4 panel width; narrower
/// layout columns pass their own bound at construction.
pub const PNG_MAX_OUTPUT_WIDTH: usize = 792;

/// Sanity cap on source height; decodes are build-time work but must stay
/// bounded like every other resource in this firmware.
pub const PNG_MAX_SOURCE_HEIGHT: u32 = 8192;

/// Bytes in the widest source scanline (4-channel RGBA at the width cap);
/// sized together with [`PNG_MAX_SOURCE_WIDTH`] so buffer bounds stay
/// expressible as one constant.
pub const PNG_MAX_SCANLINE_BYTES: usize = PNG_MAX_SOURCE_WIDTH as usize * 3;

const SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
const INFLATE_WINDOW: usize = 512;
const SCAN_BYTES: usize = PNG_MAX_SCANLINE_BYTES;
const CUR_BYTES: usize = 1 + SCAN_BYTES;
const PALETTE_BYTES: usize = 768;
const TRNS_BYTES: usize = 256;

/// Scratch the decoder carves its buffers from. Firmware passes the idle
/// EPUB XHTML window (24 KB); host tests pass a buffer of this length.
pub const PNG_SCRATCH_BYTES: usize = CUR_BYTES
    + SCAN_BYTES
    + PNG_MAX_SOURCE_WIDTH as usize
    + PNG_MAX_OUTPUT_WIDTH
    + PNG_MAX_OUTPUT_WIDTH
    + PNG_MAX_OUTPUT_WIDTH
    + PNG_MAX_OUTPUT_WIDTH
    + PALETTE_BYTES
    + TRNS_BYTES
    + INFLATE_WINDOW;

/// Ways a PNG can fail to decode. Callers map every variant to the same
/// graceful fallback (the `alt` placeholder), so the variants exist for
/// diagnostics and tests, not for branching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PngError {
    BadMagic,
    MissingBytes,
    ScratchTooSmall,
    BadChunk,
    BadCrc,
    Corrupt,
    Unsupported,
    Decompress(MZError),
    Sink,
    /// The byte source feeding the decoder failed.
    Source,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PngColorType {
    Gray,
    Rgb,
    Palette,
}

impl PngColorType {
    fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(PngColorType::Gray),
            2 => Some(PngColorType::Rgb),
            3 => Some(PngColorType::Palette),
            // Alpha-carrying types (4, 6) are rejected in v1: the extra
            // scanline channel would cost 25% more static buffer for a
            // feature inline book images rarely need. Palette + tRNS keeps
            // transparency for the diagrams that do.
            _ => None,
        }
    }

    fn channels(self) -> usize {
        match self {
            PngColorType::Gray | PngColorType::Palette => 1,
            PngColorType::Rgb => 3,
        }
    }
}

/// Dimensions and shape of a decodable image, readable from the IHDR chunk
/// alone — cheap enough to probe while reserving layout space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PngHeader {
    pub width: u32,
    pub height: u32,
    pub color_type: PngColorType,
}

/// Fit a source image inside a target box without upscaling, preserving
/// aspect ratio. Layout and the decoder share this so the reserved rect and
/// the emitted rows always agree.
pub fn fit_inside(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let max_w = max_w.max(1);
    let max_h = max_h.max(1);
    let mut w = src_w.max(1).min(max_w);
    let mut h = ((src_h.max(1) as u64 * w as u64) / src_w.max(1) as u64) as u32;
    if h > max_h {
        h = max_h;
        w = ((src_w.max(1) as u64 * h as u64) / src_h.max(1) as u64) as u32;
    }
    (w.max(1), h.max(1))
}

/// Receives one grayscale output row at a time, top to bottom. `y` counts
/// output rows, not source rows, and `row` holds exactly the scaled width.
pub trait LumaSink {
    fn row(&mut self, y: u32, row: &[u8]) -> Result<(), PngError>;
}

/// CRC-32/ISO-HDLC step over a running state (start and end with `!0`/`!`).
/// PNG and ZIP share this polynomial; a bitwise loop avoids pulling the
/// `crc` crate's digest lifetimes into a long-lived decoder struct.
fn crc32_update(crc: u32, bytes: &[u8]) -> u32 {
    let mut crc = crc;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

/// Incremental decoder. Feed it PNG bytes in any chunking via [`Self::feed`],
/// then call [`Self::finish`]; rows arrive on the sink as they complete. The
/// inflate state is borrowed so firmware can reuse the ZIP scratch that is
/// idle while the decoder runs.
pub struct PngDecoder<'a> {
    target_w: u32,
    target_h: u32,
    exact_size: bool,
    header: Option<PngHeader>,

    phase: Phase,
    sig_pos: usize,
    hdr_fill: usize,
    hdr_buf: [u8; 8],
    chunk_len: u32,
    crc_fill: usize,
    stored_crc: u32,
    chunk_crc: u32,
    saw_ihdr: bool,
    saw_idat: bool,
    saw_iend: bool,
    idat_done: bool,

    ihdr_fill: usize,
    ihdr_buf: [u8; 13],
    palette: &'a mut [u8],
    palette_fill: usize,
    palette_entries: u16,
    trns: &'a mut [u8],
    trns_fill: usize,
    trns_gray: Option<u8>,
    trns_rgb: Option<[u8; 3]>,

    // Inflate side: a small window drains into the row assembler.
    window: &'a mut [u8],
    win_pos: usize,
    state_reset: bool,

    // Row assembly: `cur` holds the filter byte plus the raw scanline; `last`
    // is the previous unfiltered row the Up/Average/Paeth filters reference.
    cur: &'a mut [u8],
    last: &'a mut [u8],
    row_fill: usize,
    bpl: usize,
    channels: usize,

    // Luma pipeline: full-width luma row, horizontally downsampled row, and
    // per-output-column vertical box accumulators.
    luma_row: &'a mut [u8],
    hbox_row: &'a mut [u8],
    out_row: &'a mut [u8],
    // The vertical box accumulator is a u16 kept as lo/hi byte planes so it
    // can live in the borrowed byte scratch.
    acc_lo: &'a mut [u8],
    acc_hi: &'a mut [u8],
    acc_count: u32,
    cur_out_j: u32,

    out_w: u32,
    out_h: u32,
    out_y: u32,
    src_y: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Sig,
    ChunkHeader,
    ChunkData,
    ChunkCrc,
    Done,
}

impl<'a> PngDecoder<'a> {
    /// A decoder for an image that must fit inside `target_w × target_h`
    /// after the shared [`fit_inside`] sizing. Carves its buffers from
    /// `scratch` — see [`PNG_SCRATCH_BYTES`] for the required length.
    pub fn new(scratch: &'a mut [u8], target_w: u32, target_h: u32) -> Result<Self, PngError> {
        Self::with_sizing(scratch, target_w, target_h, false)
    }

    /// A decoder that emits exactly `width × height` rows, for a size that
    /// layout already fitted. Fitting a second time is not idempotent:
    /// `fit_inside` floors, so a fitted size can shrink by a pixel or more
    /// and the emitted rows would no longer match the reserved rect.
    pub fn with_output_size(
        scratch: &'a mut [u8],
        width: u32,
        height: u32,
    ) -> Result<Self, PngError> {
        Self::with_sizing(scratch, width, height, true)
    }

    fn with_sizing(
        scratch: &'a mut [u8],
        target_w: u32,
        target_h: u32,
        exact_size: bool,
    ) -> Result<Self, PngError> {
        if scratch.len() < PNG_SCRATCH_BYTES {
            return Err(PngError::ScratchTooSmall);
        }
        let (cur, rest) = scratch.split_at_mut(CUR_BYTES);
        let (last, rest) = rest.split_at_mut(SCAN_BYTES);
        let (luma_row, rest) = rest.split_at_mut(PNG_MAX_SOURCE_WIDTH as usize);
        let (hbox_row, rest) = rest.split_at_mut(PNG_MAX_OUTPUT_WIDTH);
        let (out_row, rest) = rest.split_at_mut(PNG_MAX_OUTPUT_WIDTH);
        let (acc_lo, rest) = rest.split_at_mut(PNG_MAX_OUTPUT_WIDTH);
        let (acc_hi, rest) = rest.split_at_mut(PNG_MAX_OUTPUT_WIDTH);
        let (palette, rest) = rest.split_at_mut(PALETTE_BYTES);
        let (trns, rest) = rest.split_at_mut(TRNS_BYTES);
        let (window, _) = rest.split_at_mut(INFLATE_WINDOW);
        Ok(Self {
            target_w,
            target_h,
            exact_size,
            header: None,
            phase: Phase::Sig,
            sig_pos: 0,
            hdr_fill: 0,
            hdr_buf: [0; 8],
            chunk_len: 0,
            crc_fill: 0,
            stored_crc: 0,
            chunk_crc: 0xFFFF_FFFF,
            saw_ihdr: false,
            saw_idat: false,
            saw_iend: false,
            idat_done: false,
            ihdr_fill: 0,
            ihdr_buf: [0; 13],
            palette,
            palette_fill: 0,
            palette_entries: 0,
            trns,
            trns_fill: 0,
            trns_gray: None,
            trns_rgb: None,
            window,
            win_pos: 0,
            state_reset: false,
            cur,
            last,
            row_fill: 0,
            bpl: 0,
            channels: 0,
            luma_row,
            hbox_row,
            out_row,
            acc_lo,
            acc_hi,
            acc_count: 0,
            cur_out_j: u32::MAX,
            out_w: 0,
            out_h: 0,
            out_y: 0,
            src_y: 0,
        })
    }

    /// The IHDR-derived geometry, available once the header chunk completes.
    pub fn header(&self) -> Option<PngHeader> {
        self.header
    }

    /// Output dimensions the sink will see, once the header has arrived.
    pub fn output_size(&self) -> Option<(u32, u32)> {
        self.header.as_ref()?;
        Some((self.out_w, self.out_h))
    }

    pub fn feed<S: LumaSink>(
        &mut self,
        state: &mut InflateState,
        chunk: &[u8],
        sink: &mut S,
    ) -> Result<(), PngError> {
        let mut rest = chunk;
        while !rest.is_empty() && self.phase != Phase::Done {
            let taken = self.step(state, rest, sink)?;
            rest = &rest[taken..];
        }
        Ok(())
    }

    /// Validates that the stream completed: IEND arrived and every expected
    /// output row was emitted. Bytes after IEND are ignored by `feed`.
    ///
    /// A zlib stream can hold its final bytes pending one more push when
    /// the IDAT payload ended exactly at a feed boundary, so `finish` first
    /// flushes the inflate with `MZFlush::Finish` before judging the stream.
    pub fn finish<S: LumaSink>(
        &mut self,
        state: &mut InflateState,
        sink: &mut S,
    ) -> Result<(), PngError> {
        if self.saw_idat && !self.idat_done {
            loop {
                self.drain_rows(sink)?;
                let res = inflate(
                    state,
                    &[],
                    &mut self.window[self.win_pos..],
                    MZFlush::Finish,
                );
                self.win_pos += res.bytes_written;
                self.drain_rows(sink)?;
                match res.status {
                    Ok(MZStatus::StreamEnd) => {
                        self.idat_done = true;
                        break;
                    }
                    // Anything left after the flush is a stalled or broken
                    // stream; whatever is missing is genuinely absent.
                    status => {
                        if res.bytes_written == 0 {
                            let _ = status;
                            break;
                        }
                    }
                }
            }
        }
        if !self.saw_iend || !self.saw_idat || !self.idat_done {
            return Err(PngError::Corrupt);
        }
        if self.out_y != self.out_h {
            // The zlib stream ended without producing every scanline.
            return Err(PngError::Corrupt);
        }
        if self.header.unwrap().color_type == PngColorType::Palette && self.palette_entries == 0 {
            return Err(PngError::Corrupt);
        }
        Ok(())
    }

    /// Consumes as much of `chunk` as it can, returning the bytes consumed.
    fn step<S: LumaSink>(
        &mut self,
        state: &mut InflateState,
        chunk: &[u8],
        sink: &mut S,
    ) -> Result<usize, PngError> {
        match self.phase {
            Phase::Sig => {
                let take = (SIG.len() - self.sig_pos).min(chunk.len());
                if chunk[..take] != SIG[self.sig_pos..self.sig_pos + take] {
                    return Err(PngError::BadMagic);
                }
                self.sig_pos += take;
                if self.sig_pos == SIG.len() {
                    self.phase = Phase::ChunkHeader;
                }
                Ok(take)
            }
            Phase::ChunkHeader => {
                let take = (self.hdr_buf.len() - self.hdr_fill).min(chunk.len());
                self.hdr_buf[self.hdr_fill..self.hdr_fill + take].copy_from_slice(&chunk[..take]);
                self.hdr_fill += take;
                if self.hdr_fill == self.hdr_buf.len() {
                    self.begin_chunk()?;
                }
                Ok(take)
            }
            Phase::ChunkData => {
                let take = (chunk.len() as u32).min(self.chunk_len) as usize;
                self.on_chunk_data(state, &chunk[..take], sink)?;
                Ok(take)
            }
            Phase::ChunkCrc => {
                let take = (4 - self.crc_fill).min(chunk.len());
                // The trailing CRC bytes are compared, not folded into the
                // running CRC, which covers only type + data.
                for &byte in &chunk[..take] {
                    self.stored_crc = (self.stored_crc << 8) | byte as u32;
                }
                self.crc_fill += take;
                if self.crc_fill == 4 {
                    self.end_chunk(sink)?;
                }
                Ok(take)
            }
            Phase::Done => Ok(chunk.len()),
        }
    }

    fn begin_chunk(&mut self) -> Result<(), PngError> {
        let len = u32::from_be_bytes(self.hdr_buf[0..4].try_into().unwrap());
        let ty: [u8; 4] = self.hdr_buf[4..8].try_into().unwrap();
        self.chunk_len = len;
        self.chunk_crc = crc32_update(0xFFFF_FFFF, &ty);
        self.crc_fill = 0;

        if !self.saw_ihdr && ty != *b"IHDR" {
            return Err(PngError::BadChunk);
        }
        if self.saw_ihdr && ty == *b"IHDR" {
            return Err(PngError::BadChunk);
        }
        match &ty {
            b"IHDR" => {
                if len != 13 {
                    return Err(PngError::BadChunk);
                }
                self.ihdr_fill = 0;
            }
            b"PLTE" => {
                if len == 0 || len % 3 != 0 || len > 768 {
                    return Err(PngError::BadChunk);
                }
                self.palette_fill = 0;
            }
            b"tRNS" => {
                self.trns_fill = 0;
            }
            b"IDAT" if len > 0 => {
                self.saw_idat = true;
            }
            b"IEND" if len != 0 => return Err(PngError::BadChunk),
            _ => {}
        }

        self.phase = if len == 0 {
            Phase::ChunkCrc
        } else {
            Phase::ChunkData
        };
        self.hdr_fill = 0;
        Ok(())
    }

    fn on_chunk_data<S: LumaSink>(
        &mut self,
        state: &mut InflateState,
        data: &[u8],
        sink: &mut S,
    ) -> Result<(), PngError> {
        self.chunk_crc = crc32_update(self.chunk_crc, data);
        let ty: [u8; 4] = self.hdr_buf[4..8].try_into().unwrap();
        match &ty {
            b"IHDR" => {
                let room = self.ihdr_buf.len() - self.ihdr_fill;
                if data.len() > room {
                    return Err(PngError::BadChunk);
                }
                self.ihdr_buf[self.ihdr_fill..self.ihdr_fill + data.len()].copy_from_slice(data);
                self.ihdr_fill += data.len();
            }
            b"PLTE" => {
                if self.palette_fill + data.len() > self.palette.len() {
                    return Err(PngError::BadChunk);
                }
                self.palette[self.palette_fill..self.palette_fill + data.len()]
                    .copy_from_slice(data);
                self.palette_fill += data.len();
            }
            b"tRNS" => {
                if self.trns_fill + data.len() > self.trns.len() {
                    return Err(PngError::BadChunk);
                }
                self.trns[self.trns_fill..self.trns_fill + data.len()].copy_from_slice(data);
                self.trns_fill += data.len();
            }
            b"IDAT" if !self.idat_done => {
                self.idat_feed(state, data, sink)?;
            }
            _ => {}
        }
        self.chunk_len -= data.len() as u32;
        if self.chunk_len == 0 {
            self.phase = Phase::ChunkCrc;
        }
        Ok(())
    }

    fn end_chunk<S: LumaSink>(&mut self, _sink: &mut S) -> Result<(), PngError> {
        let ty: [u8; 4] = self.hdr_buf[4..8].try_into().unwrap();
        if crc32_finish_val(self.chunk_crc) != self.stored_crc {
            return Err(PngError::BadCrc);
        }
        match &ty {
            b"IHDR" => {
                self.apply_ihdr()?;
                self.saw_ihdr = true;
            }
            b"PLTE" => {
                self.palette_entries = (self.palette_fill / 3) as u16;
            }
            b"tRNS" => self.apply_trns(ty)?,
            b"IEND" => {
                self.saw_iend = true;
                self.phase = Phase::Done;
            }
            _ => {}
        }
        if self.phase != Phase::Done {
            self.phase = Phase::ChunkHeader;
        }
        self.hdr_fill = 0;
        self.crc_fill = 0;
        self.stored_crc = 0;
        Ok(())
    }

    fn apply_ihdr(&mut self) -> Result<(), PngError> {
        let header = parse_ihdr(&self.ihdr_buf)?;
        self.channels = header.color_type.channels();
        let bpl = header.width as usize * self.channels;
        if 1 + bpl > self.cur.len() {
            return Err(PngError::Unsupported);
        }
        self.bpl = bpl;
        let max_w = self.target_w.min(PNG_MAX_OUTPUT_WIDTH as u32);
        let (out_w, out_h) = if self.exact_size {
            if max_w == 0
                || self.target_h == 0
                || max_w > header.width
                || self.target_h > header.height
            {
                return Err(PngError::Unsupported);
            }
            (max_w, self.target_h)
        } else {
            fit_inside(header.width, header.height, max_w, self.target_h)
        };
        // The vertical box accumulator is u16 per column; a span beyond 200
        // source rows per output row would overflow it, and such extreme
        // downscales look bad anyway. Reject to the placeholder path.
        if header.height / out_h > 200 {
            return Err(PngError::Unsupported);
        }
        self.out_w = out_w;
        self.out_h = out_h;
        self.header = Some(header);
        Ok(())
    }

    fn apply_trns(&mut self, ty: [u8; 4]) -> Result<(), PngError> {
        let data = &self.trns[..self.trns_fill];
        if ty == *b"tRNS" && self.header.map(|h| h.color_type) == Some(PngColorType::Palette) {
            if data.len() as u16 > self.palette_entries {
                return Err(PngError::Corrupt);
            }
            // Palette alphas stay in `trns`, indexed per entry.
            return Ok(());
        }
        match self.header.map(|h| h.color_type) {
            Some(PngColorType::Gray) => match data.len() {
                1 => self.trns_gray = Some(data[0]),
                2 => self.trns_gray = Some(data[1]),
                _ => return Err(PngError::Corrupt),
            },
            Some(PngColorType::Rgb) => match data.len() {
                3 => self.trns_rgb = Some([data[0], data[1], data[2]]),
                6 => self.trns_rgb = Some([data[1], data[3], data[5]]),
                _ => return Err(PngError::Corrupt),
            },
            // tRNS is not meaningful for alpha-carrying color types; ignore.
            _ => {}
        }
        Ok(())
    }

    /// Pushes IDAT payload bytes through the zlib inflate, draining the
    /// output window into the row assembler as it fills.
    fn idat_feed<S: LumaSink>(
        &mut self,
        state: &mut InflateState,
        mut input: &[u8],
        sink: &mut S,
    ) -> Result<(), PngError> {
        if !self.state_reset {
            state.reset(DataFormat::Zlib);
            self.state_reset = true;
        }
        while !input.is_empty() {
            self.drain_rows(sink)?;
            let res = inflate(
                state,
                input,
                &mut self.window[self.win_pos..],
                MZFlush::None,
            );
            input = &input[res.bytes_consumed..];
            self.win_pos += res.bytes_written;
            self.drain_rows(sink)?;
            match res.status {
                Ok(MZStatus::StreamEnd) => {
                    self.idat_done = true;
                    return Ok(());
                }
                // `Buf` is miniz for "no room": output full or input empty.
                // Output space exists after the drain, so `Buf` with input
                // left and no progress means the stream is corrupt.
                Ok(MZStatus::Ok) | Err(MZError::Buf) => {
                    if res.bytes_consumed == 0 && res.bytes_written == 0 && !input.is_empty() {
                        return Err(PngError::Decompress(MZError::Buf));
                    }
                }
                Ok(MZStatus::NeedDict) => return Err(PngError::Corrupt),
                Err(e) => return Err(PngError::Decompress(e)),
            }
        }
        Ok(())
    }

    /// Moves window bytes into the current scanline, completing and
    /// processing rows as they fill. Always drains the window fully.
    fn drain_rows<S: LumaSink>(&mut self, sink: &mut S) -> Result<(), PngError> {
        let row_len = 1 + self.bpl;
        while self.win_pos > 0 {
            let room = row_len - self.row_fill;
            let take = self.win_pos.min(room);
            self.cur[self.row_fill..self.row_fill + take].copy_from_slice(&self.window[..take]);
            self.row_fill += take;
            self.window.copy_within(take..self.win_pos, 0);
            self.win_pos -= take;
            if self.row_fill == row_len {
                self.process_row(sink)?;
                self.row_fill = 0;
            }
        }
        Ok(())
    }

    /// Unfilters the completed scanline, converts it to luma, downsamples
    /// horizontally, accumulates the vertical box, and emits rows as output
    /// spans close.
    fn process_row<S: LumaSink>(&mut self, sink: &mut S) -> Result<(), PngError> {
        let filter = self.cur[0];
        if filter > 4 {
            return Err(PngError::Corrupt);
        }
        let bpl = self.bpl;
        let bpp = self.channels.max(1);
        for x in 0..bpl {
            let raw = self.cur[1 + x];
            let left = if x >= bpp { self.cur[1 + x - bpp] } else { 0 };
            let above = self.last[x];
            let upleft = if x >= bpp { self.last[x - bpp] } else { 0 };
            let value = match filter {
                0 => raw,
                1 => raw.wrapping_add(left),
                2 => raw.wrapping_add(above),
                3 => raw.wrapping_add(((left as u16 + above as u16) / 2) as u8),
                _ => {
                    let p = left as i16 + above as i16 - upleft as i16;
                    let pa = (p - left as i16).abs();
                    let pb = (p - above as i16).abs();
                    let pc = (p - upleft as i16).abs();
                    let pred = if pa <= pb && pa <= pc {
                        left
                    } else if pb <= pc {
                        above
                    } else {
                        upleft
                    };
                    raw.wrapping_add(pred)
                }
            };
            self.cur[1 + x] = value;
        }
        self.last[..bpl].copy_from_slice(&self.cur[1..1 + bpl]);

        let width = self.header.map(|h| h.width).unwrap_or(0) as usize;
        if width == 0 {
            return Err(PngError::Corrupt);
        }
        if self.header.unwrap().color_type == PngColorType::Palette {
            let entries = self.palette_entries;
            if self.cur[1..1 + width]
                .iter()
                .any(|&idx| idx as u16 >= entries)
            {
                return Err(PngError::Corrupt);
            }
        }
        for x in 0..width {
            let value = self.luma_at(x);
            self.luma_row[x] = value;
        }

        let (src_w, out_w, out_h) = (width as u32, self.out_w, self.out_h);
        if out_w == 0 || out_h == 0 {
            return Err(PngError::Corrupt);
        }
        for k in 0..out_w as usize {
            let lo = (k as u64 * src_w as u64 / out_w as u64) as usize;
            let hi = (((k + 1) as u64 * src_w as u64 / out_w as u64) as usize).max(lo + 1);
            let mut sum = 0u32;
            for v in &self.luma_row[lo..hi.min(width)] {
                sum += *v as u32;
            }
            let count = (hi.min(width) - lo).max(1) as u32;
            self.hbox_row[k] = ((sum + count / 2) / count) as u8;
        }

        let src_y = self.src_y;
        self.src_y += 1;
        let j = ((src_y as u64 * out_h as u64) / (self.header.unwrap().height as u64)) as u32;
        if self.cur_out_j == u32::MAX {
            self.cur_out_j = j;
            self.acc_count = 0;
        } else if j != self.cur_out_j {
            self.emit_span(sink)?;
            self.cur_out_j = j;
        }
        for k in 0..out_w as usize {
            let sum = u16::from(self.acc_lo[k]) | (u16::from(self.acc_hi[k]) << 8);
            let sum = sum + u16::from(self.hbox_row[k]);
            self.acc_lo[k] = (sum & 0xFF) as u8;
            self.acc_hi[k] = (sum >> 8) as u8;
        }
        self.acc_count += 1;
        if self.src_y == self.header.unwrap().height {
            self.emit_span(sink)?;
        }
        Ok(())
    }

    fn emit_span<S: LumaSink>(&mut self, sink: &mut S) -> Result<(), PngError> {
        let out_w = self.out_w as usize;
        if self.acc_count == 0 {
            return Err(PngError::Corrupt);
        }
        for k in 0..out_w {
            let sum = u16::from(self.acc_lo[k]) | (u16::from(self.acc_hi[k]) << 8);
            let sum = u32::from(sum) + self.acc_count / 2;
            self.out_row[k] = (sum / self.acc_count) as u8;
            self.acc_lo[k] = 0;
            self.acc_hi[k] = 0;
        }
        let y = self.out_y;
        self.out_y += 1;
        self.acc_count = 0;
        sink.row(y, &self.out_row[..out_w])
    }

    /// Luma of one source pixel, with alpha composited onto the white page.
    fn luma_at(&self, x: usize) -> u8 {
        let data = &self.cur[1..];
        match self.header.unwrap().color_type {
            PngColorType::Gray => data[x],
            PngColorType::Rgb => luma3([data[3 * x], data[3 * x + 1], data[3 * x + 2]]),
            PngColorType::Palette => {
                let idx = data[x] as usize;
                if idx as u16 >= self.palette_entries {
                    // Out-of-range index is corrupt data; callers fall back
                    // to the placeholder rather than render noise.
                    return 0;
                }
                let rgb = [
                    self.palette[3 * idx],
                    self.palette[3 * idx + 1],
                    self.palette[3 * idx + 2],
                ];
                let alpha = self.trns.get(idx).copied().unwrap_or(255);
                composite(luma3(rgb), alpha)
            }
        }
    }
}

fn crc32_finish_val(crc: u32) -> u32 {
    !crc
}

/// Parses and validates an IHDR payload. Bit depth is fixed at 8 and
/// interlacing is unsupported for v1; both reject to the placeholder path.
fn parse_ihdr(buf: &[u8; 13]) -> Result<PngHeader, PngError> {
    let width = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let height = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    let bit_depth = buf[8];
    let color_type = PngColorType::from_code(buf[9]).ok_or(PngError::Unsupported)?;
    let compression = buf[10];
    let filter = buf[11];
    let interlace = buf[12];
    if width == 0
        || height == 0
        || bit_depth != 8
        || compression != 0
        || filter != 0
        || interlace != 0
        || width > PNG_MAX_SOURCE_WIDTH
        || height > PNG_MAX_SOURCE_HEIGHT
    {
        return Err(PngError::Unsupported);
    }
    Ok(PngHeader {
        width,
        height,
        color_type,
    })
}

fn luma3(rgb: [u8; 3]) -> u8 {
    ((77 * rgb[0] as u32 + 151 * rgb[1] as u32 + 28 * rgb[2] as u32 + 128) >> 8) as u8
}

/// Alpha-composite one channel value onto the white page (255).
fn composite(v: u8, a: u8) -> u8 {
    let v = v as u32;
    let a = a as u32;
    ((v * a + 255 * (255 - a) + 127) / 255) as u8
}

/// Reads a [`PngHeader`] from a buffer holding at least the 8-byte signature
/// plus the 25-byte IHDR chunk. Cheap enough to run while reserving layout
/// space, paired with a bounded prefix read of the entry.
pub fn probe_png_header(prefix: &[u8]) -> Result<PngHeader, PngError> {
    if prefix.len() < 33 {
        return Err(PngError::MissingBytes);
    }
    if prefix[..8] != SIG {
        return Err(PngError::BadMagic);
    }
    let len = u32::from_be_bytes(prefix[8..12].try_into().unwrap());
    if len != 13 || prefix[12..16] != *b"IHDR" {
        return Err(PngError::BadChunk);
    }
    let mut ihdr = [0u8; 13];
    ihdr.copy_from_slice(&prefix[16..29]);
    parse_ihdr(&ihdr)
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn crc32(bytes: &[u8]) -> u32 {
        crc32_finish_val(crc32_update(0xFFFF_FFFF, bytes))
    }

    fn chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend((data.len() as u32).to_be_bytes());
        out.extend_from_slice(ty);
        out.extend_from_slice(data);
        out.extend(crc32(&out[4..]).to_be_bytes());
        out
    }

    fn ihdr(w: u32, h: u32, depth: u8, color: u8, interlace: u8) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend(w.to_be_bytes());
        data.extend(h.to_be_bytes());
        data.push(depth);
        data.push(color);
        data.push(0);
        data.push(0);
        data.push(interlace);
        data
    }

    fn png_file(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut out = SIG.to_vec();
        for part in parts {
            out.extend_from_slice(part);
        }
        out
    }

    /// Deflates raw scanline bytes into zlib IDAT payloads.
    fn idat(raw: &[u8]) -> Vec<u8> {
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(raw, 6);
        chunk(b"IDAT", &compressed)
    }

    #[derive(Default)]
    struct RowSink {
        rows: Vec<Vec<u8>>,
    }

    impl LumaSink for RowSink {
        fn row(&mut self, _y: u32, row: &[u8]) -> Result<(), PngError> {
            self.rows.push(row.to_vec());
            Ok(())
        }
    }

    struct FailingSink;

    impl LumaSink for FailingSink {
        fn row(&mut self, _y: u32, _row: &[u8]) -> Result<(), PngError> {
            Err(PngError::Sink)
        }
    }

    /// Drives a full decode with 1-byte feeds to stress the framing state
    /// machine, plus a second pass with irregular chunk sizes.
    fn decode_all(png: &[u8], tw: u32, th: u32) -> Result<(PngHeader, Vec<Vec<u8>>), PngError> {
        let mut state = InflateState::new(DataFormat::Zlib);
        let mut scratch = (0..PNG_SCRATCH_BYTES).map(|_| 0u8).collect::<Vec<_>>();
        let mut dec = PngDecoder::new(&mut scratch, tw, th).unwrap();
        let mut sink = RowSink::default();
        for &byte in png {
            dec.feed(&mut state, &[byte], &mut sink)?;
        }
        dec.finish(&mut state, &mut sink)?;
        let header = dec.header().unwrap();
        let rows = std::mem::take(&mut sink.rows);
        Ok((header, rows))
    }

    fn decode_chunked(png: &[u8], tw: u32, th: u32) -> Result<Vec<Vec<u8>>, PngError> {
        let mut state = InflateState::new(DataFormat::Zlib);
        let mut scratch = (0..PNG_SCRATCH_BYTES).map(|_| 0u8).collect::<Vec<_>>();
        let mut dec = PngDecoder::new(&mut scratch, tw, th).unwrap();
        let mut sink = RowSink::default();
        let mut pos = 0;
        let mut n = 1;
        while pos < png.len() {
            let take = n.min(png.len() - pos);
            dec.feed(&mut state, &png[pos..pos + take], &mut sink)?;
            pos += take;
            n = n * 3 + 1;
        }
        dec.finish(&mut state, &mut sink)?;
        Ok(std::mem::take(&mut sink.rows))
    }

    #[test]
    fn gray8_rows_pass_through() {
        let values: Vec<u8> = (0..12u16).map(|v| (v * 20) as u8).collect();
        let mut raw = Vec::new();
        for row in values.chunks(4) {
            raw.push(0); // filter: None
            raw.extend_from_slice(row);
        }
        let png = png_file(&[
            chunk(b"IHDR", &ihdr(4, 3, 8, 0, 0)),
            idat(&raw),
            chunk(b"IEND", &[]),
        ]);
        let (header, rows) = decode_all(&png, 4, 3).unwrap();
        assert_eq!(header.color_type, PngColorType::Gray);
        assert_eq!(rows.len(), 3);
        for (row, expect) in rows.iter().zip(values.chunks(4)) {
            assert_eq!(row, expect);
        }
        // Irregular feed sizes must produce identical output.
        assert_eq!(decode_chunked(&png, 4, 3).unwrap(), rows);
    }

    #[test]
    fn rgb_luma_matches_formula() {
        let pixels: [[u8; 3]; 6] = [
            [0, 0, 0],
            [255, 255, 255],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [9, 9, 9],
        ];
        let mut raw = std::iter::repeat(0u8).take(1).collect::<Vec<_>>();
        for px in &pixels {
            raw.extend_from_slice(px);
        }
        let png = png_file(&[
            chunk(b"IHDR", &ihdr(6, 1, 8, 2, 0)),
            idat(&raw),
            chunk(b"IEND", &[]),
        ]);
        let (_, rows) = decode_all(&png, 6, 1).unwrap();
        for (i, px) in pixels.iter().enumerate() {
            assert_eq!(
                rows[0][i],
                ((77 * px[0] as u32 + 151 * px[1] as u32 + 28 * px[2] as u32 + 128) >> 8) as u8
            );
        }
    }

    #[test]
    fn alpha_carrying_color_types_reject() {
        let gray_alpha = png_file(&[
            chunk(b"IHDR", &ihdr(3, 1, 8, 4, 0)),
            idat(&[0u8; 7]),
            chunk(b"IEND", &[]),
        ]);
        assert_eq!(
            decode_all(&gray_alpha, 3, 1).err(),
            Some(PngError::Unsupported)
        );
        let rgba = png_file(&[
            chunk(b"IHDR", &ihdr(2, 1, 8, 6, 0)),
            idat(&[0u8; 9]),
            chunk(b"IEND", &[]),
        ]);
        assert_eq!(decode_all(&rgba, 2, 1).err(), Some(PngError::Unsupported));
    }

    #[test]
    fn all_filter_types_roundtrip() {
        let width = 7usize;
        let raw: Vec<u8> = [10u8, 200, 3, 90, 45, 250, 128]
            .iter()
            .cycle()
            .take(width * 5)
            .copied()
            .collect();
        // Apply each filter type to one row, per the PNG spec.
        let mut filtered = Vec::new();
        for (y, filter) in (0..5).enumerate() {
            filtered.push(filter as u8);
            for x in 0..width {
                let raw_v = raw[y * width + x];
                let left = if x >= 1 { raw[y * width + x - 1] } else { 0 };
                let above = if y >= 1 { raw[(y - 1) * width + x] } else { 0 };
                let upleft = if y >= 1 && x >= 1 {
                    raw[(y - 1) * width + x - 1]
                } else {
                    0
                };
                let pred = match filter {
                    0 => 0,
                    1 => left,
                    2 => above,
                    3 => ((left as u16 + above as u16) / 2) as u8,
                    _ => {
                        let p = left as i16 + above as i16 - upleft as i16;
                        let pa = (p - left as i16).abs();
                        let pb = (p - above as i16).abs();
                        let pc = (p - upleft as i16).abs();
                        if pa <= pb && pa <= pc {
                            left
                        } else if pb <= pc {
                            above
                        } else {
                            upleft
                        }
                    }
                };
                filtered.push(raw_v.wrapping_sub(pred));
            }
        }
        let png = png_file(&[
            chunk(b"IHDR", &ihdr(7, 5, 8, 0, 0)),
            idat(&filtered),
            chunk(b"IEND", &[]),
        ]);
        let (_, rows) = decode_all(&png, 7, 5).unwrap();
        for (y, row) in rows.iter().enumerate() {
            assert_eq!(row, &raw[y * width..(y + 1) * width]);
        }
    }

    #[test]
    fn box_downscale_averages_spans() {
        // 4×4 gray where every pixel is distinct.
        let mut raw: Vec<u8> = Vec::new();
        for row in (1..=16u8).collect::<Vec<_>>().chunks(4) {
            raw.push(0);
            raw.extend_from_slice(row);
        }
        let png = png_file(&[
            chunk(b"IHDR", &ihdr(4, 4, 8, 0, 0)),
            idat(&raw),
            chunk(b"IEND", &[]),
        ]);
        let (_, rows) = decode_all(&png, 2, 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 2);
        // Top span averages rows 0-1, columns 0-1 and 2-3.
        assert_eq!(rows[0][0], ((1 + 2 + 5 + 6) + 2) / 4);
        assert_eq!(rows[0][1], ((3 + 4 + 7 + 8) + 2) / 4);
        assert_eq!(rows[1][0], ((9 + 10 + 13 + 14) + 2) / 4);
        assert_eq!(rows[1][1], ((11 + 12 + 15 + 16) + 2) / 4);
    }

    #[test]
    fn downscale_handles_uneven_spans() {
        // 5×5 → 2×2: spans are 3 rows/3 cols then 2; rounding is symmetric.
        let mut raw: Vec<u8> = Vec::new();
        for _ in 0..5 {
            raw.push(0);
            raw.extend(std::iter::repeat(255u8).take(5));
        }
        let png = png_file(&[
            chunk(b"IHDR", &ihdr(5, 5, 8, 0, 0)),
            idat(&raw),
            chunk(b"IEND", &[]),
        ]);
        let (_, rows) = decode_all(&png, 2, 2).unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(row.iter().all(|&v| v == 255));
        }
    }

    #[test]
    fn split_idat_chunks_and_garbage_tail() {
        let mut raw: Vec<u8> = Vec::new();
        for _ in 0..3 {
            raw.push(0);
            raw.extend(std::iter::repeat(7u8).take(3));
        }
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
        let (a, b) = compressed.split_at(compressed.len() / 2);
        let mut png = SIG.to_vec();
        png.extend_from_slice(&chunk(b"IHDR", &ihdr(3, 3, 8, 0, 0)));
        png.extend_from_slice(&chunk(b"IDAT", a));
        png.extend_from_slice(&chunk(b"IDAT", b));
        png.extend_from_slice(&chunk(b"tEXt", b"comment\0here"));
        png.extend_from_slice(&chunk(b"IEND", &[]));
        png.extend_from_slice(&[0xEE; 5]); // trailing garbage after IEND
        let (header, rows) = decode_all(&png, 3, 3).unwrap();
        assert_eq!(header.width, 3);
        for row in &rows {
            assert!(row.iter().all(|&v| v == 7));
        }
    }

    #[test]
    fn truncated_stream_fails() {
        let png = png_file(&[chunk(b"IHDR", &ihdr(2, 2, 8, 0, 0)), idat(&[0, 1, 2])]);
        assert_eq!(decode_all(&png, 2, 2), Err(PngError::Corrupt));
    }

    #[test]
    fn bad_crc_fails() {
        let mut ihdr_chunk = chunk(b"IHDR", &ihdr(2, 2, 8, 0, 0));
        let last = ihdr_chunk.len() - 1;
        ihdr_chunk[last] ^= 0xFF;
        let png = png_file(&[ihdr_chunk, idat(&[0, 1, 2]), chunk(b"IEND", &[])]);
        assert_eq!(decode_all(&png, 2, 2).err(), Some(PngError::BadCrc));
    }

    #[test]
    fn unsupported_inputs_reject() {
        let interlaced = png_file(&[
            chunk(b"IHDR", &ihdr(2, 2, 8, 0, 1)),
            idat(&[0; 6]),
            chunk(b"IEND", &[]),
        ]);
        assert_eq!(
            decode_all(&interlaced, 2, 2).err(),
            Some(PngError::Unsupported)
        );

        let depth1 = png_file(&[
            chunk(b"IHDR", &ihdr(2, 2, 1, 0, 0)),
            idat(&[0; 6]),
            chunk(b"IEND", &[]),
        ]);
        assert_eq!(decode_all(&depth1, 2, 2).err(), Some(PngError::Unsupported));

        let oversized = png_file(&[
            chunk(b"IHDR", &ihdr(2000, 2, 8, 0, 0)),
            idat(&[0; 6]),
            chunk(b"IEND", &[]),
        ]);
        assert_eq!(
            decode_all(&oversized, 100, 2).err(),
            Some(PngError::Unsupported)
        );

        let mut bad_magic = SIG;
        bad_magic[0] = 0;
        let mut state = InflateState::new(DataFormat::Zlib);
        let mut scratch = (0..PNG_SCRATCH_BYTES).map(|_| 0u8).collect::<Vec<_>>();
        let mut dec = PngDecoder::new(&mut scratch, 2, 2).unwrap();
        let mut sink = RowSink::default();
        assert_eq!(
            dec.feed(&mut state, &bad_magic, &mut sink),
            Err(PngError::BadMagic)
        );
    }

    #[test]
    fn palette_without_plte_fails() {
        let png = png_file(&[
            chunk(b"IHDR", &ihdr(2, 1, 8, 3, 0)),
            idat(&[0, 0, 1]),
            chunk(b"IEND", &[]),
        ]);
        assert_eq!(decode_all(&png, 2, 1).err(), Some(PngError::Corrupt));
    }

    #[test]
    fn sink_error_propagates() {
        let png = png_file(&[
            chunk(b"IHDR", &ihdr(2, 1, 8, 0, 0)),
            idat(&[0, 1, 2]),
            chunk(b"IEND", &[]),
        ]);
        let mut state = InflateState::new(DataFormat::Zlib);
        let mut scratch = (0..PNG_SCRATCH_BYTES).map(|_| 0u8).collect::<Vec<_>>();
        let mut dec = PngDecoder::new(&mut scratch, 2, 1).unwrap();
        let mut sink = FailingSink;
        let result = dec.feed(&mut state, &png, &mut sink);
        assert!(matches!(result, Err(PngError::Sink)));
    }

    #[test]
    fn probe_header_reads_ihdr() {
        let png = png_file(&[chunk(b"IHDR", &ihdr(30, 20, 8, 2, 0))]);
        assert_eq!(
            probe_png_header(&png),
            Ok(PngHeader {
                width: 30,
                height: 20,
                color_type: PngColorType::Rgb
            })
        );
        assert_eq!(probe_png_header(&png[..20]), Err(PngError::MissingBytes));
    }

    #[test]
    fn fit_inside_never_upscales() {
        assert_eq!(fit_inside(800, 600, 400, 400), (400, 300));
        assert_eq!(fit_inside(400, 1200, 400, 300), (100, 300));
        assert_eq!(fit_inside(100, 50, 400, 400), (100, 50));
        assert_eq!(fit_inside(1600, 100, 800, 400), (800, 50));
        assert_eq!(fit_inside(0, 0, 400, 400), (1, 1));
    }
}
