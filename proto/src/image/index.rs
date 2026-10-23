//! `IMAGES.BIN`: where each image in a book lives, what it is, and how far
//! it has been decoded. One per book, in the book's cache directory.
//!
//! Built once from the ZIP central directory, which a book open reads
//! anyway, so building costs no extra card reads beyond walking that
//! directory again when the book has more images than one pass can hold.
//! It holds the images a decoder takes: PNGs and JPEGs (see
//! [`is_image_path`]).
//! Everything else in an entry starts empty and is filled in place later:
//! the image's format and size when it is first probed, a `wanted` mark
//! when a chapter's layout meets it before that, an `inline` mark once
//! layout shows it inside a line at a size of its own, and per orientation,
//! the decode status and where its rows sit in that orientation's pack. The
//! index does not depend on type settings, so it survives every relayout.
//!
//! This module owns the format and the logic; the caller does the card
//! I/O through [`IndexIo`]. Nothing here keeps static memory: building
//! borrows a scratch slice, and lookups use a 64-byte stack buffer.
//!
//! # Layout
//!
//! Little-endian throughout.
//!
//! | offset | bytes | holds |
//! |---|---|---|
//! | 0 | 64 | header: magic `X4IM`, version, entry size, the book's identity, the two page boxes images are fitted to, entry count, flags, checksum |
//! | 64 | 514 | fan-out: 257 `u16`; bucket `b` (a hash's top byte) holds entries `fan[b]..fan[b + 1]` |
//! | 1024 | 64 · count | entries, sorted by path hash |
//!
//! What is checked, and when:
//!
//! - [`open_image_index`] checks the header (magic, version, sizes, zeroed
//!   reserved bytes, the book and page boxes it was built for), a checksum
//!   (FNV-1a, 32-bit) over the header's first 60 bytes and the fan-out, that
//!   the fan-out never decreases and ends at the entry count, and that the
//!   file is long enough to hold every entry.
//! - Entries change in place after the build, so the checksum cannot cover
//!   them. Each carries its own 16-bit check over bytes 0 to 21 (its path
//!   hash and where it lives in the ZIP) in bytes 22 and 23, and every read
//!   verifies it, along with the entry's bucket: [`find_image`] that the
//!   hash's top byte is the bucket it searched, [`read_image`] that the slot
//!   lies in the fan-out range of its hash's bucket. An entry that fails
//!   either is never trusted: it reads as not found, and the image keeps its
//!   alt-text placeholder.
//! - An entry's state bytes are range-checked on every read. State that is
//!   out of range reads as unprobed and not decoded, so the image is probed
//!   and decoded again rather than drawn from a pack offset nothing vouches
//!   for.
//!
//! Entries start on a 512-byte boundary and are 64 bytes long, so no entry
//! straddles a card block, and updating one is a single write inside one
//! block.
//!
//! Entry:
//!
//! | offset | bytes | holds |
//! |---|---|---|
//! | 0 | 8 | path hash ([`super::path::path_hash`] of the ZIP name) |
//! | 8 | 4 | ZIP local-header offset |
//! | 12 | 4 | compressed size |
//! | 16 | 4 | uncompressed size |
//! | 20 | 2 | ZIP method (0 stored, 8 deflated) |
//! | 22 | 2 | check: FNV-1a (32-bit) of bytes 0 to 21, its halves XORed |
//! | 24 | 1 | [`ImageFormat`] |
//! | 25 | 1 | [`ProbeStatus`] |
//! | 26 | 1 | flags: bit 0 `wanted`, bit 1 `inline` |
//! | 27 | 1 | reserved, zero |
//! | 28 | 2, 2 | source width, height (0 until probed) |
//! | 32 | 1, 1 | landscape, portrait [`DecodeStatus`] |
//! | 34 | 2 | reserved, zero |
//! | 36 | 4, 4 | landscape, portrait pack offset |
//! | 44 | 2 | where layout first met it ([`ImagePlace`]): spine + 1, 0 until met |
//! | 46 | 2 | and how many images that spine met before it |
//! | 48 | 16 | reserved, zero |
//!
//! Bytes 24 onward are the entry's state, which [`write_image_state`]
//! rewrites in one write; bytes 0 to 23 never change after the build.
//!
//! # Lookup cost
//!
//! [`find_image`] reads the bucket's two fan-out values (4 bytes), then
//! binary-searches the bucket reading one 64-byte entry per step, and the
//! last entry read is the answer. [`read_image`] reads the entry and its
//! bucket's fan-out values: two reads. With hashes spread evenly, a book at the
//! [`MAX_IMAGE_ENTRIES`] cap has about 32 entries per bucket: 1 + 6 reads,
//! all within about 2 KB of the file. The worst case, every entry in one
//! bucket, is 1 + 14.
//!
//! # Building
//!
//! [`ImageIndexBuilder`] takes the central directory as a stream of entries
//! and holds 24 bytes per image in its scratch. When the book has more
//! images than that holds (about 900 in 24 KB), a pass keeps the ones in the
//! lowest hash buckets and the caller streams the directory again for the
//! rest; entries reach the card in hash order either way. Up to
//! [`MAX_IMAGE_ENTRIES`] images are indexed. Past that, or should a single
//! bucket ever outgrow the scratch, the images left out read as missing,
//! keep their alt-text placeholder, and the header's `truncated` flag says
//! so.
//!
//! The header is written last. A build that stops early leaves a zeroed
//! header, which [`open_image_index`] rejects, so the index is rebuilt.

use super::jpeg::{JpegError, JpegProbe};
use super::path::path_hash;
use super::png;
use crate::epub::{EntryVisitor, ZipEntry};

/// The file's name in a book's cache directory.
pub const IMAGE_INDEX_FILE: &str = "IMAGES.BIN";
pub const IMAGE_INDEX_MAGIC: [u8; 4] = *b"X4IM";
/// 1 held PNGs only. 2 holds JPEGs too, so an index built before them is
/// built again, which deletes its packs.
pub const IMAGE_INDEX_VERSION: u16 = 2;
pub const IMAGE_INDEX_HEADER_BYTES: usize = 64;
pub const IMAGE_FANOUT_AT: u32 = 64;
pub const IMAGE_FANOUT_BYTES: usize = 2 * (BUCKETS + 1);
pub const IMAGE_ENTRIES_AT: u32 = 1024;
pub const IMAGE_ENTRY_BYTES: usize = 64;
/// Most images an index holds.
pub const MAX_IMAGE_ENTRIES: usize = 8192;
/// Smallest scratch [`ImageIndexBuilder::new`] takes. It holds 16 images
/// per pass; 24 KB holds about 900.
pub const MIN_BUILD_SCRATCH: usize = FANOUT_SCRATCH + STAGING_BYTES + 16 * RECORD_BYTES;

const BUCKETS: usize = 256;
const CHECKSUM_AT: usize = 60;
const STATE_AT: usize = 24;
const STATE_BYTES: usize = IMAGE_ENTRY_BYTES - STATE_AT;
/// What a pass keeps of each image: hash, local-header offset, both sizes
/// and the method.
const RECORD_BYTES: usize = 24;
/// The fan-out, rounded up to keep what follows aligned.
const FANOUT_SCRATCH: usize = IMAGE_FANOUT_BYTES.next_multiple_of(8);
/// Entries go to the card in runs this long: whole card blocks.
const STAGING_BYTES: usize = 2048;
const FLAG_TRUNCATED: u16 = 1;
const FLAG_WANTED: u8 = 1;
const FLAG_INLINE: u8 = 2;

const _: () = assert!(IMAGE_ENTRIES_AT as usize >= IMAGE_FANOUT_AT as usize + IMAGE_FANOUT_BYTES);
const _: () =
    assert!(IMAGE_ENTRIES_AT.is_multiple_of(512) && 512usize.is_multiple_of(IMAGE_ENTRY_BYTES));
const _: () = assert!(STAGING_BYTES.is_multiple_of(IMAGE_ENTRY_BYTES) && STAGING_BYTES >= 1024);
const _: () = assert!(MAX_IMAGE_ENTRIES <= u16::MAX as usize);

/// Card access for the index: exact reads and writes at byte offsets in
/// `IMAGES.BIN`. The builder only writes at or before the end of what it
/// has written, so an append-only file with in-place rewrites suffices.
pub trait IndexIo {
    type Error;

    /// Fills `buf` from `offset`; anything short of that is an error.
    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), Self::Error>;
    fn write_at(&mut self, offset: u32, buf: &[u8]) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexError {
    /// A read or write failed.
    Io,
    /// Not an index: wrong magic, or a build that never finished.
    BadMagic,
    /// An index from another version of this format.
    BadVersion,
    /// Built for another copy of the book, or images fitted to other page
    /// boxes.
    Stale,
    /// The header's checksum or the fan-out does not hold together.
    Corrupt,
    /// The slot is past the last entry.
    NoSuchSlot,
    /// The build scratch is under [`MIN_BUILD_SCRATCH`].
    ScratchTooSmall,
}

/// What an index must match to be trusted: the book it was built from and
/// the page boxes images are fitted to (decode state is only good for
/// those).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageIndexKey {
    pub source_hash: u32,
    pub source_size: u32,
    /// Width and height images are fitted to, landscape then portrait.
    pub landscape_box: (u16, u16),
    pub portrait_box: (u16, u16),
}

/// A validated index: keep it to look images up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageIndexHeader {
    pub key: ImageIndexKey,
    pub entry_count: u16,
    /// The book has images the index does not hold; they read as missing.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageFormat {
    #[default]
    Unknown,
    Png,
    Jpeg,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProbeStatus {
    /// The header has not been read yet.
    #[default]
    Unprobed,
    /// A format and size the decoder takes.
    Supported,
    /// A real image the decoders do not take (an interlaced PNG, a
    /// progressive JPEG, one too large, neither): it shows as its alt text.
    Unsupported,
    /// The header could not be read or makes no sense.
    Failed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecodeStatus {
    #[default]
    NotDecoded,
    Decoded,
    Failed,
}

/// One orientation's decode of an image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeState {
    pub status: DecodeStatus,
    /// Where the rows start in that orientation's pack file.
    pub pack_offset: u32,
}

/// One image in the index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageEntry {
    pub path_hash: u64,
    pub local_header_offset: u32,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    /// The ZIP compression method: 0 stored, 8 deflated.
    pub method: u16,
    pub format: ImageFormat,
    pub probe: ProbeStatus,
    /// A chapter's layout met this image before it was probed.
    pub wanted: bool,
    /// Layout showed this image inside a line, at a size other than the
    /// one it has alone in a paragraph (it is taller than an inline image
    /// may be). From then on it decodes at its inline size, in every
    /// orientation; a block use draws that smaller decode in its box.
    /// Never cleared, so an image has one decoded size per orientation.
    pub inline: bool,
    /// Source size, once probed.
    pub width: u16,
    pub height: u16,
    pub landscape: DecodeState,
    pub portrait: DecodeState,
    /// Where layout first met the image, the earliest place in the book that
    /// shows it; `None` until a chapter's layout has. The decode pass
    /// orders by it, so images are decoded in reading order.
    pub seen_at: Option<ImagePlace>,
}

/// A place in the book's reading order: a spine, and how many images that
/// spine's layout met before this one there. It does not depend on type
/// settings or orientation, so a relayout finds the same place again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImagePlace {
    pub spine: u16,
    pub order: u16,
}

impl ImageEntry {
    /// Fills in what a probe found.
    pub fn set_probe(&mut self, probe: ImageProbe) {
        self.format = probe.format;
        self.probe = probe.status;
        self.width = probe.width;
        self.height = probe.height;
    }

    /// The decode state for an orientation.
    pub fn decode(&self, portrait: bool) -> DecodeState {
        if portrait {
            self.portrait
        } else {
            self.landscape
        }
    }

    pub fn set_decode(&mut self, portrait: bool, state: DecodeState) {
        if portrait {
            self.portrait = state;
        } else {
            self.landscape = state;
        }
    }
}

/// What the start of an image file says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageProbe {
    pub format: ImageFormat,
    pub status: ProbeStatus,
    pub width: u16,
    pub height: u16,
}

/// Bytes [`probe_image`] needs to judge a PNG: the signature and IHDR.
pub const IMAGE_PROBE_BYTES: usize = png::PROBE_BYTES;

/// Judges an image from a prefix of its file: a PNG's first
/// [`IMAGE_PROBE_BYTES`], or a JPEG's segments up to its frame header. A
/// prefix that ends before what it needs judges as failed.
pub fn probe_image(prefix: &[u8]) -> ImageProbe {
    let mut prober = ImageProber::new();
    prober.take(prefix);
    prober.finish()
}

/// Judges an image from its first bytes as they stream past: a PNG by its
/// signature and IHDR, a JPEG by its segments up to the frame header,
/// which may come after large APPn segments (an EXIF thumbnail can be 64
/// KB). A reader that can seek need not read what
/// [`EntryVisitor::skippable`] says the prober would pass over.
#[derive(Clone, Debug)]
pub struct ImageProber {
    state: Prober,
}

#[derive(Clone, Debug)]
enum Prober {
    /// The first bytes, until they say which format.
    Prefix([u8; IMAGE_PROBE_BYTES], u8),
    Jpeg(JpegProbe),
    Done(ImageProbe),
}

/// A JPEG starts with SOI and another marker.
const JPEG_START: [u8; 3] = [0xFF, 0xD8, 0xFF];

impl Default for ImageProber {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageProber {
    pub fn new() -> Self {
        Self {
            state: Prober::Prefix([0; IMAGE_PROBE_BYTES], 0),
        }
    }

    /// The answer, once [`EntryVisitor::take`] has said it needs no more;
    /// before that, what the bytes so far say about a file that ended
    /// there.
    pub fn finish(&self) -> ImageProbe {
        match &self.state {
            Prober::Done(probe) => *probe,
            Prober::Prefix(bytes, len) => {
                probe_png(bytes.get(..usize::from(*len)).unwrap_or_default())
            }
            Prober::Jpeg(_) => ImageProbe {
                format: ImageFormat::Jpeg,
                status: ProbeStatus::Failed,
                ..ImageProbe::default()
            },
        }
    }
}

impl EntryVisitor for ImageProber {
    fn take(&mut self, mut bytes: &[u8]) -> bool {
        if let Prober::Prefix(prefix, len) = &mut self.state {
            let have = usize::from(*len);
            let n = bytes.len().min(IMAGE_PROBE_BYTES - have);
            if let (Some(room), Some(part)) = (prefix.get_mut(have..have + n), bytes.get(..n)) {
                room.copy_from_slice(part);
            }
            *len += n as u8;
            bytes = bytes.get(n..).unwrap_or_default();
            let held = prefix.get(..usize::from(*len)).unwrap_or_default();
            if held.starts_with(&JPEG_START) {
                // A JPEG: its probe reads from the start.
                let mut jpeg = JpegProbe::new();
                let answer = jpeg.feed(held);
                self.state = Prober::Jpeg(jpeg);
                if let Some(answer) = answer {
                    self.state = Prober::Done(jpeg_probe(answer));
                    return false;
                }
            } else if held.len() >= JPEG_START.len() && !held.starts_with(&[0x89]) {
                // Neither: every PNG starts with 0x89.
                self.state = Prober::Done(ImageProbe {
                    format: ImageFormat::Unknown,
                    status: ProbeStatus::Unsupported,
                    ..ImageProbe::default()
                });
                return false;
            } else if held.len() == IMAGE_PROBE_BYTES {
                self.state = Prober::Done(probe_png(held));
                return false;
            } else {
                return true;
            }
        }
        if let Prober::Jpeg(jpeg) = &mut self.state {
            if let Some(answer) = jpeg.feed(bytes) {
                self.state = Prober::Done(jpeg_probe(answer));
                return false;
            }
            return true;
        }
        false
    }

    fn skippable(&self) -> u32 {
        match &self.state {
            Prober::Jpeg(jpeg) => jpeg.skippable(),
            _ => 0,
        }
    }

    fn skip(&mut self, n: u32) {
        if let Prober::Jpeg(jpeg) = &mut self.state {
            jpeg.skip(n);
        }
    }
}

fn jpeg_probe(answer: Result<super::jpeg::JpegInfo, JpegError>) -> ImageProbe {
    let size = |side: u32| u16::try_from(side).unwrap_or(u16::MAX);
    match answer {
        Ok(info) => ImageProbe {
            format: ImageFormat::Jpeg,
            status: if info.is_supported() {
                ProbeStatus::Supported
            } else {
                ProbeStatus::Unsupported
            },
            width: size(info.width),
            height: size(info.height),
        },
        // Hierarchical: a real JPEG, with no size to say.
        Err(JpegError::Unsupported) => ImageProbe {
            format: ImageFormat::Jpeg,
            status: ProbeStatus::Unsupported,
            ..ImageProbe::default()
        },
        Err(_) => ImageProbe {
            format: ImageFormat::Jpeg,
            status: ProbeStatus::Failed,
            ..ImageProbe::default()
        },
    }
}

/// Judges a PNG, or anything that is neither format, from its first bytes
/// (up to [`IMAGE_PROBE_BYTES`]; fewer when the file is shorter).
fn probe_png(prefix: &[u8]) -> ImageProbe {
    match png::probe(prefix) {
        Ok(info) => {
            let size = |side: u32| u16::try_from(side).unwrap_or(u16::MAX);
            ImageProbe {
                format: ImageFormat::Png,
                status: if info.is_supported() {
                    ProbeStatus::Supported
                } else {
                    ProbeStatus::Unsupported
                },
                width: size(info.width),
                height: size(info.height),
            }
        }
        Err(png::PngError::NotPng) => ImageProbe {
            format: ImageFormat::Unknown,
            status: ProbeStatus::Unsupported,
            width: 0,
            height: 0,
        },
        // Cut short or a broken header. Past eight bytes the signature
        // matched, so it was meant to be a PNG.
        Err(_) => ImageProbe {
            format: if prefix.len() >= 8 {
                ImageFormat::Png
            } else {
                ImageFormat::Unknown
            },
            status: ProbeStatus::Failed,
            width: 0,
            height: 0,
        },
    }
}

/// Whether a ZIP entry name looks like an image the index should hold: a
/// `.png`, `.jpg` or `.jpeg`, in any case. A probe confirms it by the
/// file's magic bytes.
///
/// Taking in a new format bumps [`IMAGE_INDEX_VERSION`], so every index is
/// built again with it, and the cache version, so books holding it are
/// laid out again to show it (see `proto::cache`).
pub fn is_image_path(name: &str) -> bool {
    is_image_name(name.as_bytes())
}

/// Whether an image's file name (its last path component) says it is a
/// cover: it contains `cover`, in any case.
pub fn is_cover_name(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name);
    file.as_bytes()
        .windows(5)
        .any(|window| window.eq_ignore_ascii_case(b"cover"))
}

/// [`is_image_path`] on raw name bytes, which may be only a name's tail:
/// the check reads nothing before the byte ahead of the last dot.
pub fn is_image_name(name: &[u8]) -> bool {
    let Some(dot) = name.iter().rposition(|&byte| byte == b'.') else {
        return false;
    };
    let (stem, extension) = (&name[..dot], &name[dot + 1..]);
    stem.last().is_some_and(|&byte| byte != b'/')
        && (extension.eq_ignore_ascii_case(b"png")
            || extension.eq_ignore_ascii_case(b"jpg")
            || extension.eq_ignore_ascii_case(b"jpeg"))
}

/// Reads and checks an index's header and fan-out. Any error means the
/// file must be rebuilt, not used.
pub fn open_image_index(
    io: &mut impl IndexIo,
    key: &ImageIndexKey,
) -> Result<ImageIndexHeader, IndexError> {
    let mut chunk = [0u8; IMAGE_INDEX_HEADER_BYTES];
    read(io, 0, &mut chunk)?;
    let header = chunk;
    if header[..4] != IMAGE_INDEX_MAGIC {
        return Err(IndexError::BadMagic);
    }
    if u16_at(&header, 4) != IMAGE_INDEX_VERSION {
        return Err(IndexError::BadVersion);
    }
    if usize::from(u16_at(&header, 6)) != IMAGE_ENTRY_BYTES
        || u32_at(&header, 28) != IMAGE_ENTRIES_AT
        || header[32..CHECKSUM_AT].iter().any(|&byte| byte != 0)
    {
        return Err(IndexError::Corrupt);
    }
    let count = u16_at(&header, 24);
    let flags = u16_at(&header, 26);
    if usize::from(count) > MAX_IMAGE_ENTRIES || flags & !FLAG_TRUNCATED != 0 {
        return Err(IndexError::Corrupt);
    }
    // The fan-out, a chunk at a time: 64 is even, so no value straddles two.
    let mut checksum = fnv1a32(FNV32_OFFSET, &header[..CHECKSUM_AT]);
    let mut previous = 0u16;
    let mut index = 0usize;
    let mut at = 0usize;
    while at < IMAGE_FANOUT_BYTES {
        let len = (IMAGE_FANOUT_BYTES - at).min(chunk.len());
        let part = &mut chunk[..len];
        read(io, IMAGE_FANOUT_AT + at as u32, part)?;
        checksum = fnv1a32(checksum, part);
        for value in part.chunks_exact(2) {
            let value = u16::from_le_bytes([value[0], value[1]]);
            let first_is_zero = index > 0 || value == 0;
            if value < previous || value > count || !first_is_zero {
                return Err(IndexError::Corrupt);
            }
            previous = value;
            index += 1;
        }
        at += len;
    }
    if previous != count || checksum != u32_at(&header, CHECKSUM_AT) {
        return Err(IndexError::Corrupt);
    }
    let found = ImageIndexKey {
        source_hash: u32_at(&header, 8),
        source_size: u32_at(&header, 12),
        landscape_box: (u16_at(&header, 16), u16_at(&header, 18)),
        portrait_box: (u16_at(&header, 20), u16_at(&header, 22)),
    };
    if found != *key {
        return Err(IndexError::Stale);
    }
    // A file cut short reads as corrupt here rather than on first use.
    if count > 0 {
        let mut last = [0u8; 1];
        let end = entry_at(count) - 1;
        io.read_at(end, &mut last)
            .map_err(|_| IndexError::Corrupt)?;
    }
    Ok(ImageIndexHeader {
        key: found,
        entry_count: count,
        truncated: flags & FLAG_TRUNCATED != 0,
    })
}

/// Finds the image filed under `path_hash`: its slot (what an image token
/// names) and its entry, or `None` when the book has no such image, or when
/// an entry the search reads fails its check, since the search cannot be
/// trusted past it.
pub fn find_image(
    io: &mut impl IndexIo,
    header: &ImageIndexHeader,
    path_hash: u64,
) -> Result<Option<(u16, ImageEntry)>, IndexError> {
    if header.entry_count == 0 {
        return Ok(None);
    }
    let bucket = (path_hash >> 56) as u32;
    let mut entry = [0u8; IMAGE_ENTRY_BYTES];
    read(io, IMAGE_FANOUT_AT + 2 * bucket, &mut entry[..4])?;
    let mut low = u16_at(&entry, 0);
    let mut high = u16_at(&entry, 2);
    if low > high || high > header.entry_count {
        return Err(IndexError::Corrupt);
    }
    while low < high {
        let middle = low + (high - low) / 2;
        read(io, entry_at(middle), &mut entry)?;
        let Some(found) = decode_entry(&entry) else {
            return Ok(None);
        };
        if found.path_hash >> 56 != u64::from(bucket) {
            return Ok(None);
        }
        match found.path_hash.cmp(&path_hash) {
            core::cmp::Ordering::Less => low = middle + 1,
            core::cmp::Ordering::Greater => high = middle,
            core::cmp::Ordering::Equal => return Ok(Some((middle, found))),
        }
    }
    Ok(None)
}

/// The entry in `slot`, as an image token names it, or `None` when the
/// entry fails its check or sits outside its hash's bucket: the image is
/// then treated as missing.
pub fn read_image(
    io: &mut impl IndexIo,
    header: &ImageIndexHeader,
    slot: u16,
) -> Result<Option<ImageEntry>, IndexError> {
    if slot >= header.entry_count {
        return Err(IndexError::NoSuchSlot);
    }
    let mut entry = [0u8; IMAGE_ENTRY_BYTES];
    read(io, entry_at(slot), &mut entry)?;
    let Some(found) = decode_entry(&entry) else {
        return Ok(None);
    };
    let bucket = (found.path_hash >> 56) as u32;
    let mut range = [0u8; 4];
    read(io, IMAGE_FANOUT_AT + 2 * bucket, &mut range)?;
    let in_bucket = (u16_at(&range, 0)..u16_at(&range, 2)).contains(&slot);
    Ok(in_bucket.then_some(found))
}

/// Smallest buffer [`scan_images`] takes: the fan-out and one entry.
pub const MIN_SCAN_BUFFER: usize = IMAGE_FANOUT_BYTES + IMAGE_ENTRY_BYTES;

/// Calls `visit` with each entry in `slots`, reading them a run at a time
/// through `buf`, and with `io` back so it can update the entry. An entry
/// that fails its check or sits outside its hash's bucket is passed over,
/// as [`read_image`] would refuse it. Stops when `visit` returns false.
///
/// A chapter's second layout pass uses this to find the images its first
/// pass marked: one read per run of entries rather than two per entry.
pub fn scan_images<I: IndexIo>(
    io: &mut I,
    header: &ImageIndexHeader,
    slots: core::ops::Range<u16>,
    buf: &mut [u8],
    mut visit: impl FnMut(&mut I, u16, ImageEntry) -> Result<bool, IndexError>,
) -> Result<(), IndexError> {
    if buf.len() < MIN_SCAN_BUFFER {
        return Err(IndexError::ScratchTooSmall);
    }
    let end = slots.end.min(header.entry_count);
    let (fanout, entries) = buf.split_at_mut(IMAGE_FANOUT_BYTES);
    read(io, IMAGE_FANOUT_AT, fanout)?;
    let per_read = (entries.len() / IMAGE_ENTRY_BYTES).min(usize::from(u16::MAX)) as u16;
    let mut slot = slots.start;
    while slot < end {
        let count = (end - slot).min(per_read);
        let run = &mut entries[..usize::from(count) * IMAGE_ENTRY_BYTES];
        read(io, entry_at(slot), run)?;
        for (offset, bytes) in run.chunks_exact(IMAGE_ENTRY_BYTES).enumerate() {
            let at = slot + offset as u16;
            let Ok(bytes) = <&[u8; IMAGE_ENTRY_BYTES]>::try_from(bytes) else {
                continue;
            };
            let Some(entry) = decode_entry(bytes) else {
                continue;
            };
            let bucket = bucket_of(entry.path_hash);
            let first = u16_at(fanout, 2 * bucket);
            let next = u16_at(fanout, 2 * bucket + 2);
            if !(first..next).contains(&at) {
                continue;
            }
            if !visit(io, at, entry)? {
                return Ok(());
            }
        }
        slot += count;
    }
    Ok(())
}

/// Stores an entry's state (probe result, `wanted`, both decodes) in one
/// write of [`IMAGE_ENTRY_BYTES`]` - 24` bytes. Where the image lives in the
/// ZIP never changes, so that part is left alone.
pub fn write_image_state(
    io: &mut impl IndexIo,
    header: &ImageIndexHeader,
    slot: u16,
    entry: &ImageEntry,
) -> Result<(), IndexError> {
    if slot >= header.entry_count {
        return Err(IndexError::NoSuchSlot);
    }
    let mut bytes = [0u8; IMAGE_ENTRY_BYTES];
    encode_entry(entry, &mut bytes);
    io.write_at(
        entry_at(slot) + STATE_AT as u32,
        &bytes[STATE_AT..STATE_AT + STATE_BYTES],
    )
    .map_err(|_| IndexError::Io)
}

/// What [`ImageIndexBuilder::end_pass`] wants next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildStep {
    /// Offer the whole central directory again, then end the pass.
    AnotherPass,
    /// The index is on the card and valid.
    Done(ImageIndexHeader),
}

/// Builds an index from the ZIP central directory in bounded memory.
///
/// ```text
/// let mut builder = ImageIndexBuilder::new(scratch, key)?;
/// loop {
///     for entry in central_directory { builder.offer(&entry); }
///     if let BuildStep::Done(header) = builder.end_pass(&mut io)? { break header; }
/// }
/// ```
///
/// The caller writes nothing else to the file while building, and starts
/// from an empty file. Every pass must offer the same entries.
pub struct ImageIndexBuilder<'a> {
    /// The fan-out being built, kept across passes.
    fanout: &'a mut [u8],
    /// Encoded entries on their way to the card.
    staging: &'a mut [u8],
    /// This pass's images, [`RECORD_BYTES`] each.
    records: &'a mut [[u8; RECORD_BYTES]],
    key: ImageIndexKey,
    held: usize,
    /// This pass takes buckets `start..end`.
    start: usize,
    end: usize,
    /// Entries written by earlier passes.
    written: usize,
    truncated: bool,
    started: bool,
}

impl<'a> ImageIndexBuilder<'a> {
    pub fn new(scratch: &'a mut [u8], key: ImageIndexKey) -> Result<Self, IndexError> {
        if scratch.len() < MIN_BUILD_SCRATCH {
            return Err(IndexError::ScratchTooSmall);
        }
        let (fanout, rest) = scratch.split_at_mut(FANOUT_SCRATCH);
        let (staging, rest) = rest.split_at_mut(STAGING_BYTES);
        let usable = rest.len() / RECORD_BYTES * RECORD_BYTES;
        let records: &mut [[u8; RECORD_BYTES]] = bytemuck::cast_slice_mut(&mut rest[..usable]);
        fanout.fill(0);
        Ok(Self {
            fanout: &mut fanout[..IMAGE_FANOUT_BYTES],
            staging,
            records,
            key,
            held: 0,
            start: 0,
            end: BUCKETS,
            written: 0,
            truncated: false,
            started: false,
        })
    }

    /// Takes one central-directory entry. Anything that is not an image,
    /// or not in this pass's share, is passed over.
    pub fn offer(&mut self, entry: &ZipEntry<'_>) {
        if !is_image_path(entry.name) {
            return;
        }
        let hash = path_hash(entry.name);
        let bucket = bucket_of(hash);
        if bucket < self.start || bucket >= self.end {
            return;
        }
        if self.held == self.records.len() {
            // Full: hand the highest bucket on hand to a later pass. When
            // everything is in this pass's first bucket, no later pass can
            // take the newcomer, and it is left out.
            let top = self.records[..self.held]
                .iter()
                .map(|record| bucket_of(u64_at(record, 0)))
                .fold(bucket, usize::max);
            if top == self.start {
                self.truncated = true;
                return;
            }
            self.end = top;
            let mut kept = 0;
            for index in 0..self.held {
                if bucket_of(u64_at(&self.records[index], 0)) < top {
                    self.records[kept] = self.records[index];
                    kept += 1;
                }
            }
            self.held = kept;
            if bucket >= self.end {
                return;
            }
        }
        let record = &mut self.records[self.held];
        record[0..8].copy_from_slice(&hash.to_le_bytes());
        record[8..12].copy_from_slice(&entry.local_header_offset.to_le_bytes());
        record[12..16].copy_from_slice(&entry.compressed_size.to_le_bytes());
        record[16..20].copy_from_slice(&entry.uncompressed_size.to_le_bytes());
        record[20..22].copy_from_slice(&entry.compression_method.to_le_bytes());
        record[22..24].fill(0);
        self.held += 1;
    }

    /// Writes this pass's entries and says whether another pass is needed.
    /// On [`BuildStep::Done`] the header has been written last.
    pub fn end_pass(&mut self, io: &mut impl IndexIo) -> Result<BuildStep, IndexError> {
        if !self.started {
            // A zeroed header until the build commits.
            self.staging.fill(0);
            write(io, 0, &self.staging[..IMAGE_ENTRIES_AT as usize])?;
            self.started = true;
        }
        let records = &mut self.records[..self.held];
        records.sort_unstable_by_key(|record| (u64_at(record, 0), u32_at(record, 8)));
        // Two entries with one hash (a ZIP listing a name twice): keep the
        // first in the file.
        let mut kept = 0;
        for index in 0..records.len() {
            if kept == 0 || u64_at(&records[kept - 1], 0) != u64_at(&records[index], 0) {
                records[kept] = records[index];
                kept += 1;
            }
        }
        let room = MAX_IMAGE_ENTRIES - self.written;
        if kept > room {
            kept = room;
            self.truncated = true;
            // No later pass may add more.
            self.end = BUCKETS;
        }
        let records = &records[..kept];
        let mut next = 0;
        for bucket in self.start..self.end {
            while next < records.len() && bucket_of(u64_at(&records[next], 0)) == bucket {
                next += 1;
            }
            let end = (self.written + next) as u16;
            self.fanout[2 * (bucket + 1)..2 * (bucket + 2)].copy_from_slice(&end.to_le_bytes());
        }
        let per_write = STAGING_BYTES / IMAGE_ENTRY_BYTES;
        for (run, chunk) in records.chunks(per_write).enumerate() {
            for (index, record) in chunk.iter().enumerate() {
                let entry = ImageEntry {
                    path_hash: u64_at(record, 0),
                    local_header_offset: u32_at(record, 8),
                    compressed_size: u32_at(record, 12),
                    uncompressed_size: u32_at(record, 16),
                    method: u16_at(record, 20),
                    ..ImageEntry::default()
                };
                let at = index * IMAGE_ENTRY_BYTES;
                encode_entry(&entry, &mut self.staging[at..at + IMAGE_ENTRY_BYTES]);
            }
            let first = self.written + run * per_write;
            write(
                io,
                entry_at(first as u16),
                &self.staging[..chunk.len() * IMAGE_ENTRY_BYTES],
            )?;
        }
        self.written += kept;
        self.held = 0;
        self.start = self.end;
        self.end = BUCKETS;
        if self.start < BUCKETS {
            return Ok(BuildStep::AnotherPass);
        }
        let header = ImageIndexHeader {
            key: self.key,
            entry_count: self.written as u16,
            truncated: self.truncated,
        };
        let bytes = &mut self.staging[..IMAGE_INDEX_HEADER_BYTES];
        bytes.fill(0);
        bytes[..4].copy_from_slice(&IMAGE_INDEX_MAGIC);
        put_u16(bytes, 4, IMAGE_INDEX_VERSION);
        put_u16(bytes, 6, IMAGE_ENTRY_BYTES as u16);
        put_u32(bytes, 8, header.key.source_hash);
        put_u32(bytes, 12, header.key.source_size);
        put_u16(bytes, 16, header.key.landscape_box.0);
        put_u16(bytes, 18, header.key.landscape_box.1);
        put_u16(bytes, 20, header.key.portrait_box.0);
        put_u16(bytes, 22, header.key.portrait_box.1);
        put_u16(bytes, 24, header.entry_count);
        put_u16(bytes, 26, if header.truncated { FLAG_TRUNCATED } else { 0 });
        put_u32(bytes, 28, IMAGE_ENTRIES_AT);
        let checksum = fnv1a32(fnv1a32(FNV32_OFFSET, &bytes[..CHECKSUM_AT]), self.fanout);
        put_u32(bytes, CHECKSUM_AT, checksum);
        write(io, IMAGE_FANOUT_AT, self.fanout)?;
        write(io, 0, &self.staging[..IMAGE_INDEX_HEADER_BYTES])?;
        Ok(BuildStep::Done(header))
    }
}

fn encode_entry(entry: &ImageEntry, out: &mut [u8]) {
    out[..IMAGE_ENTRY_BYTES].fill(0);
    out[0..8].copy_from_slice(&entry.path_hash.to_le_bytes());
    put_u32(out, 8, entry.local_header_offset);
    put_u32(out, 12, entry.compressed_size);
    put_u32(out, 16, entry.uncompressed_size);
    put_u16(out, 20, entry.method);
    let check = zip_check(out);
    put_u16(out, 22, check);
    out[24] = entry.format as u8;
    out[25] = entry.probe as u8;
    out[26] =
        if entry.wanted { FLAG_WANTED } else { 0 } | if entry.inline { FLAG_INLINE } else { 0 };
    put_u16(out, 28, entry.width);
    put_u16(out, 30, entry.height);
    out[32] = entry.landscape.status as u8;
    out[33] = entry.portrait.status as u8;
    put_u32(out, 36, entry.landscape.pack_offset);
    put_u32(out, 40, entry.portrait.pack_offset);
    // A spine past the field's range keeps no place; nothing in a book
    // comes near it (the spine cap is 1,024).
    if let Some(place) = entry.seen_at.filter(|place| place.spine < u16::MAX) {
        put_u16(out, 44, place.spine + 1);
        put_u16(out, 46, place.order);
    }
}

/// The check an entry keeps over its first 22 bytes: its path hash and
/// where it lives in the ZIP.
fn zip_check(bytes: &[u8]) -> u16 {
    let hash = fnv1a32(FNV32_OFFSET, &bytes[..22]);
    (hash >> 16) as u16 ^ hash as u16
}

/// An entry as stored, or `None` when its path hash or ZIP facts fail their
/// check. Out-of-range state reads as the state of a fresh entry.
fn decode_entry(bytes: &[u8; IMAGE_ENTRY_BYTES]) -> Option<ImageEntry> {
    if zip_check(bytes) != u16_at(bytes, 22) {
        return None;
    }
    let mut entry = ImageEntry {
        path_hash: u64_at(bytes, 0),
        local_header_offset: u32_at(bytes, 8),
        compressed_size: u32_at(bytes, 12),
        uncompressed_size: u32_at(bytes, 16),
        method: u16_at(bytes, 20),
        ..ImageEntry::default()
    };
    if let Some(state) = decode_state(bytes) {
        entry.format = state.format;
        entry.probe = state.probe;
        entry.wanted = state.wanted;
        entry.inline = state.inline;
        entry.width = state.width;
        entry.height = state.height;
        entry.landscape = state.landscape;
        entry.portrait = state.portrait;
        entry.seen_at = state.seen_at;
    }
    Some(entry)
}

/// The state half of an entry (bytes 24 onward), or `None` when a field is
/// out of range. The ZIP facts in the result are left empty.
fn decode_state(bytes: &[u8; IMAGE_ENTRY_BYTES]) -> Option<ImageEntry> {
    let format = match bytes[24] {
        0 => ImageFormat::Unknown,
        1 => ImageFormat::Png,
        2 => ImageFormat::Jpeg,
        _ => return None,
    };
    let probe = match bytes[25] {
        0 => ProbeStatus::Unprobed,
        1 => ProbeStatus::Supported,
        2 => ProbeStatus::Unsupported,
        3 => ProbeStatus::Failed,
        _ => return None,
    };
    let decode = |status: u8, offset: usize| -> Option<DecodeState> {
        Some(DecodeState {
            status: match status {
                0 => DecodeStatus::NotDecoded,
                1 => DecodeStatus::Decoded,
                2 => DecodeStatus::Failed,
                _ => return None,
            },
            pack_offset: u32_at(bytes, offset),
        })
    };
    if bytes[26] & !(FLAG_WANTED | FLAG_INLINE) != 0 {
        return None;
    }
    Some(ImageEntry {
        format,
        probe,
        wanted: bytes[26] & FLAG_WANTED != 0,
        inline: bytes[26] & FLAG_INLINE != 0,
        width: u16_at(bytes, 28),
        height: u16_at(bytes, 30),
        landscape: decode(bytes[32], 36)?,
        portrait: decode(bytes[33], 40)?,
        seen_at: u16_at(bytes, 44).checked_sub(1).map(|spine| ImagePlace {
            spine,
            order: u16_at(bytes, 46),
        }),
        ..ImageEntry::default()
    })
}

fn entry_at(slot: u16) -> u32 {
    IMAGE_ENTRIES_AT + u32::from(slot) * IMAGE_ENTRY_BYTES as u32
}

fn bucket_of(hash: u64) -> usize {
    (hash >> 56) as usize
}

fn read(io: &mut impl IndexIo, offset: u32, buf: &mut [u8]) -> Result<(), IndexError> {
    io.read_at(offset, buf).map_err(|_| IndexError::Io)
}

fn write(io: &mut impl IndexIo, offset: u32, buf: &[u8]) -> Result<(), IndexError> {
    io.write_at(offset, buf).map_err(|_| IndexError::Io)
}

const FNV32_OFFSET: u32 = 0x811c_9dc5;

fn fnv1a32(hash: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(hash, |hash, &byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut value = [0u8; 8];
    value.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(value)
}

fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::format;
    use std::string::String;
    use std::vec::Vec;

    /// A file in RAM that counts its reads, and refuses to write past its
    /// end (the card's FAT file cannot have holes).
    #[derive(Default)]
    struct File {
        bytes: Vec<u8>,
        reads: usize,
        writes: Vec<(u32, usize)>,
    }

    impl IndexIo for File {
        type Error = ();

        fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), ()> {
            self.reads += 1;
            let start = offset as usize;
            let source = self.bytes.get(start..start + buf.len()).ok_or(())?;
            buf.copy_from_slice(source);
            Ok(())
        }

        fn write_at(&mut self, offset: u32, buf: &[u8]) -> Result<(), ()> {
            let start = offset as usize;
            if start > self.bytes.len() {
                return Err(());
            }
            let end = start + buf.len();
            if end > self.bytes.len() {
                self.bytes.resize(end, 0);
            }
            self.bytes[start..end].copy_from_slice(buf);
            self.writes.push((offset, buf.len()));
            Ok(())
        }
    }

    const KEY: ImageIndexKey = ImageIndexKey {
        source_hash: 0xdead_beef,
        source_size: 123_456,
        landscape_box: (784, 451),
        portrait_box: (464, 763),
    };

    fn names(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("OEBPS/images/fig{index:05}.png"))
            .collect()
    }

    fn zip_entry(name: &str, index: usize) -> ZipEntry<'_> {
        ZipEntry {
            name,
            compression_method: if index % 3 == 0 { 8 } else { 0 },
            compressed_size: 1000 + index as u32,
            uncompressed_size: 2000 + index as u32,
            local_header_offset: 10 * index as u32,
        }
    }

    /// Builds from `entries` (a central directory) with `scratch_len` bytes
    /// of scratch, returning the header and the number of passes.
    fn build(
        file: &mut File,
        entries: &[ZipEntry<'_>],
        scratch_len: usize,
    ) -> (ImageIndexHeader, usize) {
        let mut scratch = std::vec![0xA5u8; scratch_len];
        let mut builder = ImageIndexBuilder::new(&mut scratch, KEY).expect("scratch is enough");
        let mut passes = 0;
        loop {
            passes += 1;
            for entry in entries {
                builder.offer(entry);
            }
            if let BuildStep::Done(header) = builder.end_pass(file).expect("pass writes") {
                return (header, passes);
            }
            assert!(passes < 1000, "the build must finish");
        }
    }

    #[test]
    fn a_built_index_finds_every_image_in_a_few_reads() {
        let names = names(4096);
        let mut entries: Vec<ZipEntry<'_>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| zip_entry(name, index))
            .collect();
        // Not images: passed over.
        entries.push(ZipEntry {
            name: "OEBPS/text/ch1.xhtml",
            ..zip_entry("", 0)
        });
        entries.push(ZipEntry {
            name: "OEBPS/images/",
            ..zip_entry("", 0)
        });
        entries.push(ZipEntry {
            name: "OEBPS/style.css",
            ..zip_entry("", 0)
        });

        let mut file = File::default();
        let (header, passes) = build(&mut file, &entries, 24 * 1024);
        assert_eq!(header.entry_count, 4096);
        assert!(!header.truncated);
        assert!(passes > 1 && passes <= 6, "{passes} passes");
        assert_eq!(
            file.bytes.len(),
            IMAGE_ENTRIES_AT as usize + 4096 * IMAGE_ENTRY_BYTES
        );
        // The build appends, then rewrites only the header area.
        let mut end = 0usize;
        for &(offset, len) in &file.writes {
            assert!(offset as usize <= end, "a write skipped ahead");
            end = end.max(offset as usize + len);
            if offset as usize + len <= IMAGE_ENTRIES_AT as usize {
                continue;
            }
            assert_eq!(offset as usize, end - len, "entries are appended");
        }

        let opened = open_image_index(&mut file, &KEY).expect("index opens");
        assert_eq!(opened, header);

        let mut worst = 0;
        let mut slots = std::collections::BTreeSet::new();
        for (index, name) in names.iter().enumerate() {
            file.reads = 0;
            let (slot, entry) = find_image(&mut file, &header, path_hash(name))
                .expect("lookup reads")
                .expect("image found");
            worst = worst.max(file.reads);
            assert!(slots.insert(slot));
            let source = zip_entry(name, index);
            assert_eq!(entry.path_hash, path_hash(name));
            assert_eq!(entry.local_header_offset, source.local_header_offset);
            assert_eq!(entry.compressed_size, source.compressed_size);
            assert_eq!(entry.uncompressed_size, source.uncompressed_size);
            assert_eq!(entry.method, source.compression_method);
            assert_eq!(entry.probe, ProbeStatus::Unprobed);
            assert_eq!(read_image(&mut file, &header, slot), Ok(Some(entry)));
        }
        assert!(worst <= 8, "a lookup took {worst} reads");

        file.reads = 0;
        assert_eq!(
            find_image(&mut file, &header, path_hash("OEBPS/missing.png")),
            Ok(None)
        );
        assert!(file.reads <= 8);
        assert_eq!(
            find_image(&mut file, &header, path_hash("OEBPS/text/ch1.xhtml")),
            Ok(None)
        );
    }

    #[test]
    fn small_books_build_in_one_pass_and_slots_follow_hash_order() {
        let names = names(639);
        let entries: Vec<ZipEntry<'_>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| zip_entry(name, index))
            .collect();
        let mut file = File::default();
        let (header, passes) = build(&mut file, &entries, 24 * 1024);
        assert_eq!((header.entry_count, passes), (639, 1));
        let mut previous = None;
        for slot in 0..header.entry_count {
            let hash = read_image(&mut file, &header, slot)
                .unwrap()
                .expect("entry is sound")
                .path_hash;
            assert!(previous < Some(hash));
            previous = Some(hash);
        }
        assert_eq!(
            read_image(&mut file, &header, 639),
            Err(IndexError::NoSuchSlot)
        );
    }

    #[test]
    fn tiny_scratch_takes_many_passes_and_gives_the_same_file() {
        let names = names(700);
        let entries: Vec<ZipEntry<'_>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| zip_entry(name, index))
            .collect();
        let mut roomy = File::default();
        build(&mut roomy, &entries, 24 * 1024);
        let mut tight = File::default();
        let (header, passes) = build(&mut tight, &entries, MIN_BUILD_SCRATCH + 40 * 24);
        assert!(passes > 10, "{passes} passes");
        assert_eq!(header.entry_count, 700);
        assert!(!header.truncated);
        assert_eq!(roomy.bytes, tight.bytes);
    }

    #[test]
    fn past_the_cap_images_are_left_out_and_flagged() {
        let names = names(MAX_IMAGE_ENTRIES + 50);
        let entries: Vec<ZipEntry<'_>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| zip_entry(name, index))
            .collect();
        let mut file = File::default();
        let (header, _) = build(&mut file, &entries, 24 * 1024);
        assert_eq!(usize::from(header.entry_count), MAX_IMAGE_ENTRIES);
        assert!(header.truncated);
        assert_eq!(open_image_index(&mut file, &KEY), Ok(header));
        let found = names
            .iter()
            .filter(|name| {
                find_image(&mut file, &header, path_hash(name))
                    .unwrap()
                    .is_some()
            })
            .count();
        assert_eq!(found, MAX_IMAGE_ENTRIES);
    }

    #[test]
    fn duplicate_names_keep_one_entry_and_books_without_images_get_an_empty_index() {
        let entries = [
            ZipEntry {
                name: "a.png",
                ..zip_entry("", 5)
            },
            ZipEntry {
                name: "b.PNG",
                ..zip_entry("", 2)
            },
            ZipEntry {
                name: "a.png",
                ..zip_entry("", 1)
            },
        ];
        let mut file = File::default();
        let (header, _) = build(&mut file, &entries, MIN_BUILD_SCRATCH);
        assert_eq!(header.entry_count, 2);
        let (_, entry) = find_image(&mut file, &header, path_hash("a.png"))
            .unwrap()
            .unwrap();
        assert_eq!(entry.local_header_offset, 10);

        let mut empty = File::default();
        let (header, passes) = build(&mut empty, &[], MIN_BUILD_SCRATCH);
        assert_eq!((header.entry_count, passes), (0, 1));
        assert_eq!(empty.bytes.len(), IMAGE_ENTRIES_AT as usize);
        assert_eq!(open_image_index(&mut empty, &KEY), Ok(header));
        assert_eq!(
            find_image(&mut empty, &header, path_hash("a.png")),
            Ok(None)
        );
    }

    #[test]
    fn updating_an_entry_is_one_small_write() {
        let names = names(50);
        let entries: Vec<ZipEntry<'_>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| zip_entry(name, index))
            .collect();
        let mut file = File::default();
        let (header, _) = build(&mut file, &entries, MIN_BUILD_SCRATCH);
        let (slot, mut entry) = find_image(&mut file, &header, path_hash(&names[7]))
            .unwrap()
            .unwrap();
        let before = file.bytes.clone();

        entry.wanted = true;
        entry.set_probe(ImageProbe {
            format: ImageFormat::Png,
            status: ProbeStatus::Supported,
            width: 640,
            height: 900,
        });
        entry.set_decode(
            true,
            DecodeState {
                status: DecodeStatus::Decoded,
                pack_offset: 0x0012_3456,
            },
        );
        entry.seen_at = Some(ImagePlace {
            spine: 0,
            order: 517,
        });
        // A careless caller cannot move the image in the ZIP.
        let mut careless = entry;
        careless.local_header_offset = 0;
        file.writes.clear();
        write_image_state(&mut file, &header, slot, &careless).unwrap();
        assert_eq!(file.writes.len(), 1);
        let (offset, len) = file.writes[0];
        assert_eq!(len, STATE_BYTES);
        assert_eq!(
            offset / 512,
            (offset + len as u32 - 1) / 512,
            "one card block"
        );
        assert_eq!(read_image(&mut file, &header, slot), Ok(Some(entry)));
        assert!(entry.decode(true).status == DecodeStatus::Decoded);
        assert!(entry.decode(false).status == DecodeStatus::NotDecoded);
        // The inline mark is a flag beside `wanted`, kept with it.
        for (wanted, inline) in [(false, true), (true, true), (false, false)] {
            let marked = ImageEntry {
                wanted,
                inline,
                ..entry
            };
            write_image_state(&mut file, &header, slot, &marked).unwrap();
            assert_eq!(read_image(&mut file, &header, slot), Ok(Some(marked)));
        }
        write_image_state(&mut file, &header, slot, &entry).unwrap();
        // Nothing else moved.
        let changed: Vec<usize> = (0..before.len())
            .filter(|&at| before[at] != file.bytes[at])
            .collect();
        assert!(changed
            .iter()
            .all(|&at| at >= offset as usize && at < offset as usize + len));
        assert_eq!(open_image_index(&mut file, &KEY), Ok(header));
        assert_eq!(
            write_image_state(&mut file, &header, 50, &entry),
            Err(IndexError::NoSuchSlot)
        );
    }

    #[test]
    fn damaged_old_or_foreign_files_are_refused() {
        let names = names(40);
        let entries: Vec<ZipEntry<'_>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| zip_entry(name, index))
            .collect();
        let mut good = File::default();
        build(&mut good, &entries, MIN_BUILD_SCRATCH);
        let open = |bytes: &[u8], key: &ImageIndexKey| {
            let mut file = File {
                bytes: bytes.to_vec(),
                ..File::default()
            };
            open_image_index(&mut file, key)
        };
        assert!(open(&good.bytes, &KEY).is_ok());

        // An interrupted build: header still zero.
        let mut unfinished = good.bytes.clone();
        unfinished[..IMAGE_INDEX_HEADER_BYTES].fill(0);
        assert_eq!(open(&unfinished, &KEY), Err(IndexError::BadMagic));
        for version in [0, 1] {
            // Version 1 held PNGs only: built again, now with JPEGs.
            let mut old = good.bytes.clone();
            old[4] = version;
            assert_eq!(open(&old, &KEY), Err(IndexError::BadVersion));
        }
        // Any flipped header or fan-out byte fails the checksum or a check.
        for at in (6..IMAGE_FANOUT_AT as usize + IMAGE_FANOUT_BYTES).step_by(7) {
            let mut damaged = good.bytes.clone();
            damaged[at] ^= 0x10;
            assert!(open(&damaged, &KEY).is_err(), "byte {at}");
        }
        // Cut short.
        assert_eq!(
            open(&good.bytes[..good.bytes.len() - 1], &KEY),
            Err(IndexError::Corrupt)
        );
        // Another copy of the book, or another panel's page boxes.
        let other_book = ImageIndexKey {
            source_size: 1,
            ..KEY
        };
        assert_eq!(open(&good.bytes, &other_book), Err(IndexError::Stale));
        let other_panel = ImageIndexKey {
            landscape_box: (776, 499),
            ..KEY
        };
        assert_eq!(open(&good.bytes, &other_panel), Err(IndexError::Stale));
    }

    #[test]
    fn damaged_entries_are_never_trusted() {
        let names = names(300);
        let entries: Vec<ZipEntry<'_>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| zip_entry(name, index))
            .collect();
        let mut good = File::default();
        let (header, _) = build(&mut good, &entries, 24 * 1024);
        let slot_of = |file: &mut File, name: &str| {
            find_image(file, &header, path_hash(name))
                .unwrap()
                .map(|(slot, _)| slot)
        };
        let target = &names[123];
        let slot = slot_of(&mut good, target).expect("built");
        let at = IMAGE_ENTRIES_AT as usize + usize::from(slot) * IMAGE_ENTRY_BYTES;

        // Any change to the hash, the ZIP facts or their check: the image
        // reads as missing both ways.
        for byte in 0..STATE_AT {
            for flip in [0x01u8, 0x80] {
                let mut file = File {
                    bytes: good.bytes.clone(),
                    ..File::default()
                };
                file.bytes[at + byte] ^= flip;
                assert_eq!(slot_of(&mut file, target), None, "byte {byte}");
                assert_eq!(
                    read_image(&mut file, &header, slot),
                    Ok(None),
                    "byte {byte}"
                );
            }
        }
        // A zeroed entry fails its check too.
        let mut zeroed = File {
            bytes: good.bytes.clone(),
            ..File::default()
        };
        zeroed.bytes[at..at + IMAGE_ENTRY_BYTES].fill(0);
        assert_eq!(read_image(&mut zeroed, &header, slot), Ok(None));

        // A sound entry in the wrong place (here, another bucket's) is not
        // trusted either.
        let bucket_at = |slot: u16| {
            let at = IMAGE_ENTRIES_AT as usize + usize::from(slot) * IMAGE_ENTRY_BYTES;
            u64_at(&good.bytes, at) >> 56
        };
        let other = (0..header.entry_count)
            .find(|&other| bucket_at(other) != bucket_at(slot))
            .expect("another bucket");
        let other_at = IMAGE_ENTRIES_AT as usize + usize::from(other) * IMAGE_ENTRY_BYTES;
        let mut moved = File {
            bytes: good.bytes.clone(),
            ..File::default()
        };
        moved
            .bytes
            .copy_within(other_at..other_at + IMAGE_ENTRY_BYTES, at);
        assert_eq!(read_image(&mut moved, &header, slot), Ok(None));
        assert_eq!(slot_of(&mut moved, target), None);

        // Out-of-range state: the entry is found, unprobed and undecoded.
        let mut entry = read_image(&mut good, &header, slot).unwrap().unwrap();
        entry.set_probe(ImageProbe {
            format: ImageFormat::Png,
            status: ProbeStatus::Supported,
            width: 10,
            height: 20,
        });
        entry.wanted = true;
        entry.set_decode(
            false,
            DecodeState {
                status: DecodeStatus::Decoded,
                pack_offset: 4096,
            },
        );
        write_image_state(&mut good, &header, slot, &entry).unwrap();
        for (byte, value) in [(24, 3u8), (25, 4), (26, 4), (32, 3), (33, 9)] {
            let mut file = File {
                bytes: good.bytes.clone(),
                ..File::default()
            };
            file.bytes[at + byte] = value;
            let (_, found) = find_image(&mut file, &header, path_hash(target))
                .unwrap()
                .expect("still found");
            assert_eq!(found.probe, ProbeStatus::Unprobed, "byte {byte}");
            assert!(!found.wanted && !found.inline && found.width == 0);
            assert_eq!(found.landscape, DecodeState::default());
            assert_eq!(found.local_header_offset, entry.local_header_offset);
            assert_eq!(read_image(&mut file, &header, slot), Ok(Some(found)));
        }
    }

    #[test]
    fn probes_sort_images_into_what_the_decoder_takes() {
        let mut png = std::vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&900u32.to_be_bytes());
        png.extend_from_slice(&[8, 0, 0, 0, 0]);
        png.extend_from_slice(&[0; 4]);
        assert_eq!(png.len(), IMAGE_PROBE_BYTES);
        assert_eq!(
            probe_image(&png),
            ImageProbe {
                format: ImageFormat::Png,
                status: ProbeStatus::Supported,
                width: 640,
                height: 900,
            }
        );
        let mut interlaced = png.clone();
        interlaced[28] = 1;
        assert_eq!(probe_image(&interlaced).status, ProbeStatus::Unsupported);
        let mut huge = png.clone();
        huge[16..20].copy_from_slice(&70_000u32.to_be_bytes());
        assert_eq!(
            (probe_image(&huge).status, probe_image(&huge).width),
            (ProbeStatus::Unsupported, u16::MAX)
        );
        assert_eq!(probe_image(&png[..20]).status, ProbeStatus::Failed);
        // A JPEG's frame header, after an APP0 it passes over.
        let mut jpeg = std::vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 16];
        jpeg.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0, 17, 8, 0x02, 0x58, 0x03, 0x20, 3]);
        jpeg.extend_from_slice(&[1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        assert_eq!(
            probe_image(&jpeg),
            ImageProbe {
                format: ImageFormat::Jpeg,
                status: ProbeStatus::Supported,
                width: 800,
                height: 600,
            }
        );
        let mut progressive = jpeg.clone();
        progressive[21] = 0xC2;
        assert_eq!(
            probe_image(&progressive),
            ImageProbe {
                status: ProbeStatus::Unsupported,
                ..probe_image(&jpeg)
            }
        );
        // Cut before the frame header, or broken: failed.
        assert_eq!(
            (
                probe_image(&jpeg[..20]).format,
                probe_image(&jpeg[..20]).status
            ),
            (ImageFormat::Jpeg, ProbeStatus::Failed)
        );
        let mut broken = jpeg.clone();
        // A frame header too short for its components.
        broken[23] = 9;
        assert_eq!(probe_image(&broken).status, ProbeStatus::Failed);
        let gif = probe_image(b"GIF89a....");
        assert_eq!(
            (gif.format, gif.status),
            (ImageFormat::Unknown, ProbeStatus::Unsupported)
        );
        // Three bytes say a file is neither.
        let mut prober = ImageProber::new();
        assert!(!prober.take(b"GIF"));
        assert_eq!(prober.finish().status, ProbeStatus::Unsupported);
        // Fed a byte at a time, the answers are the same.
        for file in [&png[..], &jpeg, &progressive, &png[..20], &jpeg[..20]] {
            let mut prober = ImageProber::new();
            for &byte in file {
                if !prober.take(&[byte]) {
                    break;
                }
            }
            assert_eq!(prober.finish(), probe_image(file));
        }
    }

    #[test]
    fn covers_are_named_so_in_their_file_names() {
        for name in [
            "OEBPS/images/cover.jpg",
            "cover.jpeg",
            "OEBPS/Images/9780141439518_Cover.JPG",
            "img/front-COVER-image.png",
        ] {
            assert!(is_cover_name(name), "{name}");
        }
        for name in [
            "OEBPS/cover/front.jpg",
            "OEBPS/images/plate01.jpg",
            "cove.jpg",
            "",
        ] {
            assert!(!is_cover_name(name), "{name}");
        }
    }

    #[test]
    fn image_names_are_picked_by_extension() {
        for name in [
            "a.png",
            "OEBPS/Images/Cover.PNG",
            "x/y.Png",
            "cover.jpg",
            "x/y.jpeg",
            "OEBPS/Images/Plate.JPG",
            "a.JpEg",
        ] {
            assert!(is_image_path(name), "{name}");
        }
        for name in [
            "a.gif",
            "a.svg",
            "png",
            ".png",
            ".jpg",
            "dir/.jpeg",
            "dir/.png",
            "a.png/",
            "a.jpg/",
            "a.xhtml",
            "a.webp",
            "p.Jpe",
            "a.jfif",
        ] {
            assert!(!is_image_path(name), "{name}");
        }
        // The ZIP index pass sees a long name's last 64 bytes, which may
        // start inside a character: the answer is the whole name's.
        let long = format!("OEBPS/Bilder/é{}_1.png", "x".repeat(57));
        let tail = &long.as_bytes()[long.len() - 64..];
        assert!(
            core::str::from_utf8(tail).is_err(),
            "the tail starts inside é"
        );
        assert!(is_image_name(tail) && is_image_path(&long));
        assert!(!is_image_name(&long.as_bytes()[..long.len() - 1]));
    }

    #[test]
    fn a_scan_reads_entries_in_runs_and_passes_damaged_ones_over() {
        let names = names(300);
        let entries: Vec<ZipEntry<'_>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| zip_entry(name, index))
            .collect();
        let mut file = File::default();
        let (header, _) = build(&mut file, &entries, 24 * 1024);
        // Mark every seventh image wanted, then find exactly those.
        let mut marked = std::collections::BTreeSet::new();
        for slot in (0..300u16).step_by(7) {
            let mut entry = read_image(&mut file, &header, slot).unwrap().unwrap();
            entry.wanted = true;
            write_image_state(&mut file, &header, slot, &entry).unwrap();
            marked.insert(slot);
        }
        let mut buf = std::vec![0xA5u8; MIN_SCAN_BUFFER + 9 * IMAGE_ENTRY_BYTES];
        file.reads = 0;
        let mut found = std::collections::BTreeSet::new();
        scan_images(
            &mut file,
            &header,
            0..u16::MAX,
            &mut buf,
            |io, slot, mut entry| {
                if entry.wanted {
                    found.insert(slot);
                    // The scan hands the index back: update what it found.
                    entry.wanted = false;
                    write_image_state(io, &header, slot, &entry)?;
                }
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(found, marked);
        // The fan-out once, then ten entries a read.
        assert_eq!(file.reads, 1 + 30);
        assert!((0..300).all(|slot| !read_image(&mut file, &header, slot)
            .unwrap()
            .unwrap()
            .wanted));

        // A range, an early stop, and a damaged entry passed over.
        let damaged = 40u16;
        let at = IMAGE_ENTRIES_AT as usize + usize::from(damaged) * IMAGE_ENTRY_BYTES;
        file.bytes[at + 3] ^= 0x10;
        let mut seen = Vec::new();
        scan_images(&mut file, &header, 35..60, &mut buf, |_, slot, _| {
            seen.push(slot);
            Ok(slot < 50)
        })
        .unwrap();
        let expected: Vec<u16> = (35..=50).filter(|&slot| slot != damaged).collect();
        assert_eq!(seen, expected);
        assert_eq!(
            scan_images(
                &mut file,
                &header,
                0..1,
                &mut buf[..MIN_SCAN_BUFFER - 1],
                |_, _, _| Ok(true)
            ),
            Err(IndexError::ScratchTooSmall)
        );
    }
}
