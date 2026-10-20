//! Build-time half of the EPUB image pipeline, kept free of storage types
//! so host tests can drive it end to end.
//!
//! The book build owns one inflate state, and a spine's XHTML stream holds
//! it, so images are probed before the first spine streams and decoded only
//! after a spine's stream is done. Every image is keyed by its zip path,
//! folded with [`resolve_relative_href`]: the manifest href (relative to the
//! OPF) and the `<img src>` (relative to the chapter) must fold to the same
//! key, or the lookup misses and the image stays a placeholder.

use crate::epub::{
    resolve_relative_href, EpubPackage, EpubZipOps, OwnedZipEntry, ZipError, ZipInflateScratch,
    MAX_ENTRY_NAME_BYTES, MAX_IMAGE_ITEMS,
};
use crate::png::{probe_png_header, LumaSink, PngDecoder, PngError, PNG_SCRATCH_BYTES};

/// Images one spine can queue for decode; later ones keep their placeholder.
pub const MAX_PENDING_IMAGES: usize = 6;

/// Longest zip path the decode queue holds. Below `MAX_ENTRY_NAME_BYTES` on
/// purpose: the queue is static RAM, and the X3 stack floor has little room.
/// Longer paths keep their placeholder.
pub const MAX_IMAGE_PATH_BYTES: usize = 96;

/// Signature plus the full IHDR chunk.
const PNG_PREFIX_BYTES: usize = 33;

/// Output window for inflating a deflated entry into the spool. Carved from
/// the decoder scratch before the decoder claims it.
const SPOOL_WINDOW_BYTES: usize = 512;

const SOURCE_CHUNK_BYTES: usize = 512;

pub type ImagePath = heapless::String<MAX_ENTRY_NAME_BYTES>;

/// FNV-1a 64 over the folded zip path.
pub fn image_hash(path: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &byte in path.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Folds `href`, relative to the document at `base`, into a zip path. A
/// leading `/` names a path from the zip root.
pub fn resolve_image_href(base: &str, href: &str, out: &mut ImagePath) -> bool {
    match href.strip_prefix('/') {
        Some(rooted) => resolve_relative_href("", rooted, out),
        None => resolve_relative_href(base, href, out),
    }
}

/// Source sizes of every probed PNG, keyed by [`image_hash`].
pub struct ImageTable {
    hashes: [u64; MAX_IMAGE_ITEMS],
    widths: [u16; MAX_IMAGE_ITEMS],
    heights: [u16; MAX_IMAGE_ITEMS],
    count: usize,
}

impl ImageTable {
    pub const fn new() -> Self {
        Self {
            hashes: [0; MAX_IMAGE_ITEMS],
            widths: [0; MAX_IMAGE_ITEMS],
            heights: [0; MAX_IMAGE_ITEMS],
            count: 0,
        }
    }

    pub fn clear(&mut self) {
        self.count = 0;
    }

    pub fn insert(&mut self, hash: u64, width: u16, height: u16) {
        if self.lookup(hash).is_some() || self.count == MAX_IMAGE_ITEMS {
            return;
        }
        self.hashes[self.count] = hash;
        self.widths[self.count] = width;
        self.heights[self.count] = height;
        self.count += 1;
    }

    pub fn lookup(&self, hash: u64) -> Option<(u16, u16)> {
        let index = self.hashes[..self.count].iter().position(|&h| h == hash)?;
        Some((self.widths[index], self.heights[index]))
    }
}

impl Default for ImageTable {
    fn default() -> Self {
        Self::new()
    }
}

/// An image the current spine placed, waiting for its decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingImage {
    pub path: heapless::String<MAX_IMAGE_PATH_BYTES>,
    pub width: u16,
    pub height: u16,
}

pub struct PendingImages {
    images: heapless::Vec<PendingImage, MAX_PENDING_IMAGES>,
}

impl PendingImages {
    pub const fn new() -> Self {
        Self {
            images: heapless::Vec::new(),
        }
    }

    /// Whether `path` is queued already or still fits in the queue.
    pub fn has_room_for(&self, path: &str) -> bool {
        self.images.iter().any(|image| image.path == path)
            || (!self.images.is_full() && path.len() <= MAX_IMAGE_PATH_BYTES)
    }

    /// Queues `path` once. False when the queue is full.
    pub fn push(&mut self, path: &str, width: u16, height: u16) -> bool {
        if self.images.iter().any(|image| image.path == path) {
            return true;
        }
        let Ok(path) = heapless::String::try_from(path) else {
            return false;
        };
        self.images
            .push(PendingImage {
                path,
                width,
                height,
            })
            .is_ok()
    }

    pub fn as_slice(&self) -> &[PendingImage] {
        &self.images
    }

    pub fn clear(&mut self) {
        self.images.clear();
    }
}

impl Default for PendingImages {
    fn default() -> Self {
        Self::new()
    }
}

/// The image state of one book build.
pub struct BookImages {
    pub table: ImageTable,
    pub pending: PendingImages,
}

impl BookImages {
    pub const fn new() -> Self {
        Self {
            table: ImageTable::new(),
            pending: PendingImages::new(),
        }
    }
}

impl Default for BookImages {
    fn default() -> Self {
        Self::new()
    }
}

/// Probes every manifest PNG and records the decodable ones in `table`.
/// Must run while the zip inflate is idle. Failures are silent: those images
/// keep their placeholder.
pub fn probe_book_images<Z: EpubZipOps>(
    zip: &mut Z,
    package: &EpubPackage<'_>,
    header_scratch: &mut [u8; 46],
    name_scratch: &mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed_scratch: &mut [u8],
    inflate_scratch: &mut ZipInflateScratch,
    table: &mut ImageTable,
) {
    table.clear();
    for span in &package.image_hrefs[..package.image_count] {
        let mut path = ImagePath::new();
        if !resolve_image_href(package.opf_path, span.of(package.opf_text), &mut path) {
            continue;
        }
        let Ok(entry) = zip.find_entry(path.as_str(), header_scratch, name_scratch) else {
            continue;
        };
        // `read_entry_prefix_streamed` reports `complete` only when the whole
        // entry fit, which a real PNG never does: only the length matters.
        let mut prefix = [0u8; PNG_PREFIX_BYTES];
        let Ok((read, _)) =
            zip.read_entry_prefix_streamed(entry, compressed_scratch, &mut prefix, inflate_scratch)
        else {
            continue;
        };
        if read < PNG_PREFIX_BYTES {
            continue;
        }
        if let Ok(header) = probe_png_header(&prefix) {
            table.insert(
                image_hash(path.as_str()),
                header.width as u16,
                header.height as u16,
            );
        }
    }
}

/// Temporary storage for a deflated entry's inflated PNG bytes. The zip
/// layer and the PNG's IDAT stream both need the one inflate state, so the
/// PNG is inflated to the spool first and decoded from it afterwards.
pub trait ImageSpool {
    /// Appends bytes. The first write starts a fresh spool.
    fn write(&mut self, bytes: &[u8]) -> bool;
    /// Moves to the start for reading.
    fn rewind(&mut self) -> bool;
    /// Reads the next bytes; `Some(0)` at the end, `None` on failure.
    fn read(&mut self, out: &mut [u8]) -> Option<usize>;
}

/// Decodes the PNG in `entry` into exactly `width × height` luma rows.
///
/// `scratch` must hold [`PNG_SCRATCH_BYTES`]. A stored entry decodes
/// straight from raw reads bounded by the entry size; a deflated entry goes
/// through `spool`.
#[allow(clippy::too_many_arguments)]
pub fn decode_zip_png<Z, P, S>(
    zip: &mut Z,
    entry: OwnedZipEntry,
    spool: &mut P,
    header_scratch: &mut [u8; 46],
    name_scratch: &mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed_scratch: &mut [u8],
    scratch: &mut [u8],
    inflate_scratch: &mut ZipInflateScratch,
    width: u16,
    height: u16,
    sink: &mut S,
) -> Result<(), PngError>
where
    Z: EpubZipOps,
    P: ImageSpool,
    S: LumaSink,
{
    if scratch.len() < PNG_SCRATCH_BYTES {
        return Err(PngError::ScratchTooSmall);
    }
    match entry.compression_method {
        0 => {
            let mut offset = zip
                .entry_data_offset(entry, header_scratch, name_scratch)
                .map_err(|_| PngError::Source)?;
            let mut remaining = entry.compressed_size;
            let mut source = |out: &mut [u8]| -> Option<usize> {
                let want = (out.len() as u32).min(remaining) as usize;
                if want == 0 {
                    return Some(0);
                }
                let read = zip.read_raw_at(offset, &mut out[..want]).ok()?;
                if read == 0 {
                    return None;
                }
                offset = offset.checked_add(read as u32)?;
                remaining -= read as u32;
                Some(read)
            };
            decode_png_source(&mut source, scratch, inflate_scratch, width, height, sink)
        }
        8 => {
            let mut emit = |bytes: &[u8]| {
                if spool.write(bytes) {
                    Ok(())
                } else {
                    Err(ZipError::Io)
                }
            };
            zip.read_entry_to_sink(
                entry,
                compressed_scratch,
                &mut scratch[..SPOOL_WINDOW_BYTES],
                inflate_scratch,
                &mut emit,
            )
            .map_err(|_| PngError::Source)?;
            if !spool.rewind() {
                return Err(PngError::Source);
            }
            let mut source = |out: &mut [u8]| spool.read(out);
            decode_png_source(&mut source, scratch, inflate_scratch, width, height, sink)
        }
        _ => Err(PngError::Unsupported),
    }
}

fn decode_png_source<F, S>(
    source: &mut F,
    scratch: &mut [u8],
    inflate_scratch: &mut ZipInflateScratch,
    width: u16,
    height: u16,
    sink: &mut S,
) -> Result<(), PngError>
where
    F: FnMut(&mut [u8]) -> Option<usize>,
    S: LumaSink,
{
    let mut decoder = PngDecoder::with_output_size(scratch, u32::from(width), u32::from(height))?;
    let mut chunk = [0u8; SOURCE_CHUNK_BYTES];
    loop {
        let read = source(&mut chunk).ok_or(PngError::Source)?;
        if read == 0 {
            break;
        }
        decoder.feed(inflate_scratch.state_mut(), &chunk[..read], sink)?;
    }
    decoder.finish(inflate_scratch.state_mut(), sink)
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub::{ReadAt, ZipStream};
    use std::vec::Vec;

    struct SliceReader<'a> {
        bytes: &'a [u8],
    }

    impl ReadAt for SliceReader<'_> {
        type Error = ();

        fn len(&mut self) -> Result<u32, ()> {
            Ok(self.bytes.len() as u32)
        }

        fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, ()> {
            let start = (offset as usize).min(self.bytes.len());
            let count = out.len().min(self.bytes.len() - start);
            out[..count].copy_from_slice(&self.bytes[start..start + count]);
            Ok(count)
        }
    }

    #[derive(Default)]
    struct VecSpool {
        bytes: Vec<u8>,
        cursor: usize,
    }

    impl ImageSpool for VecSpool {
        fn write(&mut self, bytes: &[u8]) -> bool {
            self.bytes.extend_from_slice(bytes);
            true
        }

        fn rewind(&mut self) -> bool {
            self.cursor = 0;
            true
        }

        fn read(&mut self, out: &mut [u8]) -> Option<usize> {
            let count = out.len().min(self.bytes.len() - self.cursor);
            out[..count].copy_from_slice(&self.bytes[self.cursor..self.cursor + count]);
            self.cursor += count;
            Some(count)
        }
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

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in bytes {
            crc ^= byte as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    fn png_chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
        let start = out.len();
        out.extend((data.len() as u32).to_be_bytes());
        out.extend_from_slice(ty);
        out.extend_from_slice(data);
        let crc = crc32(&out[start + 4..]);
        out.extend(crc.to_be_bytes());
    }

    /// An 8-bit grayscale PNG whose pixel at (x, y) is `(x + y) * 16`.
    fn gray_png(width: u32, height: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        for y in 0..height {
            raw.push(0);
            for x in 0..width {
                raw.push(((x + y) * 16) as u8);
            }
        }
        let mut ihdr = Vec::new();
        ihdr.extend(width.to_be_bytes());
        ihdr.extend(height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        let mut png = std::vec![137, 80, 78, 71, 13, 10, 26, 10];
        png_chunk(&mut png, b"IHDR", &ihdr);
        png_chunk(
            &mut png,
            b"IDAT",
            &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6),
        );
        png_chunk(&mut png, b"IEND", &[]);
        png
    }

    /// A zip of `(name, method, data)` entries; method 8 deflates `data`.
    fn zip(entries: &[(&str, u16, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, method, data) in entries {
            let payload = match method {
                8 => miniz_oxide::deflate::compress_to_vec(data, 6),
                _ => data.to_vec(),
            };
            let offset = out.len() as u32;
            let crc = crc32(data);
            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend(20u16.to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(method.to_le_bytes());
            out.extend([0u8; 4]);
            out.extend(crc.to_le_bytes());
            out.extend((payload.len() as u32).to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&payload);

            central.extend(0x0201_4b50u32.to_le_bytes());
            central.extend(20u16.to_le_bytes());
            central.extend(20u16.to_le_bytes());
            central.extend(0u16.to_le_bytes());
            central.extend(method.to_le_bytes());
            central.extend([0u8; 4]);
            central.extend(crc.to_le_bytes());
            central.extend((payload.len() as u32).to_le_bytes());
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0u8; 12]);
            central.extend(offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((central.len() as u32).to_le_bytes());
        out.extend(central_offset.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    const OPF: &[u8] = br#"<package><manifest>
<item id="c1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
<item id="a" href="Images/a.png" media-type="image/png"/>
</manifest><spine><itemref idref="c1"/></spine></package>"#;

    fn reference_rows(png: &[u8], width: u16, height: u16) -> Vec<Vec<u8>> {
        let mut scratch = std::vec![0u8; PNG_SCRATCH_BYTES];
        let mut inflate = ZipInflateScratch::new();
        let mut sink = RowSink::default();
        let mut decoder =
            PngDecoder::with_output_size(&mut scratch, u32::from(width), u32::from(height))
                .unwrap_or_else(|_| panic!("decoder builds"));
        decoder
            .feed(inflate.state_mut(), png, &mut sink)
            .and_then(|()| decoder.finish(inflate.state_mut(), &mut sink))
            .unwrap_or_else(|error| panic!("reference decode: {error:?}"));
        sink.rows
    }

    #[test]
    fn resolve_image_href_folds_paths() {
        struct TestCase {
            base: &'static str,
            href: &'static str,
            expected: Option<&'static str>,
        }
        let cases = [
            TestCase {
                base: "OEBPS/content.opf",
                href: "Images/map.png",
                expected: Some("OEBPS/Images/map.png"),
            },
            TestCase {
                base: "OEBPS/Text/ch1.xhtml",
                href: "../Images/map.png",
                expected: Some("OEBPS/Images/map.png"),
            },
            TestCase {
                base: "OEBPS/Text/ch1.xhtml",
                href: "/OEBPS/Images/map.png",
                expected: Some("OEBPS/Images/map.png"),
            },
            TestCase {
                base: "content.opf",
                href: "./map.png#frag",
                expected: Some("map.png"),
            },
            TestCase {
                base: "ch1.xhtml",
                href: "../map.png",
                expected: None,
            },
        ];
        for case in cases {
            let mut out = ImagePath::new();
            let ok = resolve_image_href(case.base, case.href, &mut out);
            let result = ok.then(|| std::string::String::from(out.as_str()));
            assert_eq!(
                result.as_deref(),
                case.expected,
                "{} + {}",
                case.base,
                case.href
            );
        }
    }

    #[test]
    fn pending_images_queue_each_path_once_up_to_the_bound() {
        let mut pending = PendingImages::new();
        for index in 0..MAX_PENDING_IMAGES {
            let path = std::format!("OEBPS/{index}.png");
            assert!(pending.push(&path, 10, 20));
        }
        assert!(pending.push("OEBPS/0.png", 10, 20));
        assert!(pending.has_room_for("OEBPS/0.png"));
        assert!(!pending.has_room_for("OEBPS/new.png"));
        assert!(!pending.push("OEBPS/new.png", 10, 20));
        assert_eq!(pending.as_slice().len(), MAX_PENDING_IMAGES);

        let long = "x".repeat(MAX_IMAGE_PATH_BYTES + 1);
        let mut pending = PendingImages::new();
        assert!(!pending.has_room_for(&long));
        assert!(!pending.push(&long, 10, 20));
    }

    #[test]
    fn probe_and_decode_zip_pngs() {
        struct TestCase {
            name: &'static str,
            method: u16,
            source: (u32, u32),
            fitted: (u16, u16),
            expected_rows: usize,
        }
        let cases = [
            TestCase {
                name: "stored, unscaled",
                method: 0,
                source: (40, 30),
                fitted: (40, 30),
                expected_rows: 30,
            },
            TestCase {
                name: "deflated, unscaled",
                method: 8,
                source: (40, 30),
                fitted: (40, 30),
                expected_rows: 30,
            },
            TestCase {
                // fit_inside(8, 430, 720, 420) = (7, 420); fitting that
                // again would give (7, 376) and drop 44 rows.
                name: "deflated, height-limited fit",
                method: 8,
                source: (8, 430),
                fitted: (7, 420),
                expected_rows: 420,
            },
            TestCase {
                name: "stored, downscaled",
                method: 0,
                source: (100, 60),
                fitted: (50, 30),
                expected_rows: 30,
            },
        ];
        for case in cases {
            let png = gray_png(case.source.0, case.source.1);
            let bytes = zip(&[
                ("OEBPS/Images/a.png", case.method, &png),
                ("OEBPS/Text/ch1.xhtml", 8, b"<html/>"),
            ]);
            let mut tail = [0u8; 512];
            let mut zip = ZipStream::new(SliceReader { bytes: &bytes }, &mut tail)
                .unwrap_or_else(|_| panic!("{}: zip opens", case.name));
            let mut header = [0u8; 46];
            let mut name = [0u8; MAX_ENTRY_NAME_BYTES];
            let mut compressed = [0u8; 64];
            let mut inflate = ZipInflateScratch::new();
            let mut arena = [0u8; 512];
            let package = crate::epub::parse_opf_streamed(
                |emit| emit(OPF),
                &mut arena,
                crate::book::BookId(1),
                "book.epub",
                0,
                "OEBPS/content.opf",
            )
            .unwrap_or_else(|_| panic!("{}: OPF parses", case.name));

            let mut table = ImageTable::new();
            probe_book_images(
                &mut zip,
                &package,
                &mut header,
                &mut name,
                &mut compressed,
                &mut inflate,
                &mut table,
            );
            // The chapter's `../` src must fold to the key the manifest
            // href (relative to the OPF) was probed under.
            let mut path = ImagePath::new();
            assert!(resolve_image_href(
                "OEBPS/Text/ch1.xhtml",
                "../Images/a.png",
                &mut path
            ));
            assert_eq!(
                table.lookup(image_hash(path.as_str())),
                Some((case.source.0 as u16, case.source.1 as u16)),
                "{}",
                case.name
            );

            let entry = zip
                .find_entry(path.as_str(), &mut header, &mut name)
                .unwrap_or_else(|_| panic!("{}: entry exists", case.name));
            let mut spool = VecSpool::default();
            let mut scratch = std::vec![0u8; PNG_SCRATCH_BYTES];
            let mut sink = RowSink::default();
            let result = decode_zip_png(
                &mut zip,
                entry,
                &mut spool,
                &mut header,
                &mut name,
                &mut compressed,
                &mut scratch,
                &mut inflate,
                case.fitted.0,
                case.fitted.1,
                &mut sink,
            );
            assert_eq!(result, Ok(()), "{}", case.name);
            assert_eq!(sink.rows.len(), case.expected_rows, "{}", case.name);
            assert_eq!(
                sink.rows,
                reference_rows(&png, case.fitted.0, case.fitted.1),
                "{}",
                case.name
            );
        }
    }
}
