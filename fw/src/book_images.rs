//! A book's images during a build: its `IMAGES.BIN` (see
//! `proto::image::index`), the lookup each image in a chapter makes, and
//! the probes a chapter waits for between its two layout passes.
//!
//! A build session that has the book's ZIP open, and finds image entries in
//! its central directory, opens the index before its first spine, building
//! it first when there is no valid one ([`open_book_images`]). That reads
//! the central directory and no image: an image is probed only once a
//! chapter's layout meets it. A chapter that meets an image no probe has
//! read yet marks it wanted in the index, finishes that pass, probes what
//! it marked ([`BookImages::probe_marked`]) while the inflater and the XHTML
//! window are idle, and is laid out again with the real sizes. Only
//! chapters with newly met images pay for the second pass.
//!
//! The index sits beside the book index in the book's cache directory, so
//! it goes wherever that directory goes, and it does not depend on type
//! settings or orientation, so it survives every relayout.

use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use proto::cache::{CACHE_ROOT_DIR, CACHE_V2_DIR};
use proto::epub::{EpubZipOps, OwnedZipEntry, ZipError, ZipInflateScratch};
use proto::image::index::{
    find_image, open_image_index, probe_image, scan_images, write_image_state, BuildStep,
    DecodeStatus, ImageIndexBuilder, ImageIndexHeader, ImageIndexKey, ImageProbe, IndexError,
    IndexIo, ProbeStatus, IMAGE_INDEX_FILE, IMAGE_PROBE_BYTES,
};
use proto::image::path::{path_hash, resolve_image_path};
use ui::reading::{fit_image, PageBox};

/// Compressed bytes a deflated image's probe inflates from per read: its
/// first bytes come out of the stream's first few dozen.
const PROBE_INPUT_BYTES: usize = 512;
/// Most compressed bytes a probe reads before it calls the image damaged:
/// a stream that has not given up its first bytes by then never will, and
/// a request's probes must not read on until the session's budget runs
/// out.
const PROBE_INPUT_CAP: u32 = 4096;
/// The part of the XHTML window a probe's bytes take; index entries are
/// read into the rest.
const PROBE_OUTPUT_BYTES: usize = 64;

const _: () = assert!(IMAGE_PROBE_BYTES <= PROBE_OUTPUT_BYTES);

/// What the index must match: the book, and the page boxes images are
/// fitted to, so decodes for other boxes are never trusted.
pub(crate) fn image_index_key(source: (u32, u32)) -> ImageIndexKey {
    let size = |page_box: PageBox| {
        (
            (page_box.right - page_box.left).max(0) as u16,
            (page_box.bottom - page_box.top).max(0) as u16,
        )
    };
    ImageIndexKey {
        source_hash: source.0,
        source_size: source.1,
        landscape_box: size(PageBox::LANDSCAPE),
        portrait_box: size(PageBox::PORTRAIT),
    }
}

/// The book's open image index, for one build session.
pub(crate) struct BookImages<'v, D, T, const ND: usize, const NF: usize, const NV: usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: File<'v, D, T, ND, NF, NV>,
    header: ImageIndexHeader,
}

/// What layout does with an image a chapter names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImagePlacement {
    /// An image block, `width × height` (fitted to the page box), naming
    /// the image's slot in the index.
    Block { slot: u16, width: u16, height: u16 },
    /// Nothing to show: not in the book, or not an image the decoder
    /// takes. An `<img>` keeps its alt-text placeholder.
    Missing,
    /// Not probed yet, and now marked wanted in the index: the chapter is
    /// laid out again once it has been probed.
    Unprobed { slot: u16 },
}

/// Why probing stopped short. The probes before it are kept, and the
/// images after it are probed when the spine is next laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeStop {
    /// A button press.
    Interrupted,
    /// A read of the book failed, or the session's read budget ran out:
    /// nothing is recorded for the image, so it is tried again.
    Archive,
    /// The index could not be read or written.
    Index,
}

/// Whether the book has images the index holds, from its ZIP's central
/// directory. A directory that cannot be read through (a damaged entry, a
/// failed read) counts as none: lookups by name still find the entries
/// before the damage, so the book builds as it always did, with its images
/// as alt text.
pub(crate) fn book_has_images<Z: EpubZipOps>(zip: &mut Z, buf: &mut [u8]) -> bool {
    zip.has_image_entries(buf).unwrap_or(false)
}

impl<'v, D, T, const ND: usize, const NF: usize, const NV: usize> BookImages<'v, D, T, ND, NF, NV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Finds the image `src` names in the chapter at ZIP path `chapter`,
    /// resolving it into `path` (the ZIP name scratch, idle while a chapter
    /// streams). A probed image the decoder takes becomes a block fitted to
    /// `page_box`, and is marked wanted unless it is already decoded for
    /// this orientation. On a spine's first pass an unprobed image is
    /// marked wanted for the probe that follows; on its second it is
    /// missing, so a spine is laid out at most twice. Each change to the
    /// entry is one write. Err only when the card fails.
    pub(crate) fn place(
        &self,
        chapter: &str,
        src: &str,
        path: &mut [u8],
        page_box: PageBox,
        portrait: bool,
        first_pass: bool,
    ) -> Result<ImagePlacement, ()> {
        let Ok(name) = resolve_image_path(chapter, src, path) else {
            return Ok(ImagePlacement::Missing);
        };
        let io = &mut CardIndex(&self.file);
        let (slot, mut entry) = match find_image(io, &self.header, path_hash(name)) {
            Ok(Some(found)) => found,
            Ok(None) => return Ok(ImagePlacement::Missing),
            Err(IndexError::Io) => return Err(()),
            Err(_) => return Ok(ImagePlacement::Missing),
        };
        match entry.probe {
            ProbeStatus::Supported if entry.width > 0 && entry.height > 0 => {
                let (width, height) = fit_image(page_box, entry.width.into(), entry.height.into());
                // Wanted until it is decoded for this orientation.
                if !entry.wanted && entry.decode(portrait).status == DecodeStatus::NotDecoded {
                    entry.wanted = true;
                    write_image_state(io, &self.header, slot, &entry).map_err(|_| ())?;
                }
                Ok(ImagePlacement::Block {
                    slot,
                    width,
                    height,
                })
            }
            ProbeStatus::Unprobed if first_pass => {
                if !entry.wanted {
                    entry.wanted = true;
                    write_image_state(io, &self.header, slot, &entry).map_err(|_| ())?;
                }
                Ok(ImagePlacement::Unprobed { slot })
            }
            _ => Ok(ImagePlacement::Missing),
        }
    }

    /// Probes the images in `slots` that a chapter's first pass marked
    /// (wanted and not yet probed), and stores each result in its entry as
    /// one write. A probe reads the image's local header by its stored
    /// offset, checks that it names the image, then reads its first
    /// `IMAGE_PROBE_BYTES`; a deflated image is inflated only that far,
    /// from at most `PROBE_INPUT_CAP` compressed bytes. An image whose
    /// archive entry or header is damaged is marked failed and keeps its
    /// alt text. A failed read records nothing and stops the probing, so
    /// the image is probed again later.
    ///
    /// `window` (the XHTML window) holds the probe's bytes and runs of
    /// index entries, `compressed` a deflated image's input, and `inflate`
    /// its stream: all idle between a spine's passes. Asks `gives_way`
    /// after each image. Returns how many it probed.
    #[inline(never)]
    pub(crate) fn probe_marked<Z: EpubZipOps>(
        &self,
        zip: &mut Z,
        slots: core::ops::RangeInclusive<u16>,
        compressed: &mut [u8],
        window: &mut [u8],
        inflate: &mut ZipInflateScratch,
        gives_way: impl Fn() -> bool,
    ) -> Result<usize, ProbeStop> {
        let input_len = compressed.len().min(PROBE_INPUT_BYTES);
        let input = &mut compressed[..input_len];
        let Some((bytes, entries)) = window.split_at_mut_checked(PROBE_OUTPUT_BYTES) else {
            return Err(ProbeStop::Index);
        };
        let mut probed = 0;
        let mut stop = None;
        let range = *slots.start()..slots.end().saturating_add(1);
        let mut io = CardIndex(&self.file);
        scan_images(
            &mut io,
            &self.header,
            range,
            entries,
            |io, slot, mut entry| {
                if !entry.wanted || entry.probe != ProbeStatus::Unprobed {
                    return Ok(true);
                }
                let source = OwnedZipEntry {
                    compression_method: entry.method,
                    compressed_size: entry.compressed_size,
                    uncompressed_size: entry.uncompressed_size,
                    local_header_offset: entry.local_header_offset,
                };
                let prefix = &mut bytes[..IMAGE_PROBE_BYTES];
                let path_hash = entry.path_hash;
                let read = zip.read_entry_start(
                    source,
                    &mut |name| {
                        core::str::from_utf8(name)
                            .is_ok_and(|name| proto::image::path::path_hash(name) == path_hash)
                    },
                    input,
                    prefix,
                    inflate,
                    PROBE_INPUT_CAP,
                );
                entry.set_probe(match read {
                    Ok(len) => probe_image(&prefix[..len]),
                    Err(ZipError::Io) => {
                        stop = Some(ProbeStop::Archive);
                        return Ok(false);
                    }
                    // Damage the archive holds: the same next time, so it
                    // must not hold its chapter back.
                    Err(_) => ImageProbe {
                        status: ProbeStatus::Failed,
                        ..ImageProbe::default()
                    },
                });
                write_image_state(io, &self.header, slot, &entry)?;
                probed += 1;
                if gives_way() {
                    stop = Some(ProbeStop::Interrupted);
                }
                Ok(stop.is_none())
            },
        )
        .map_err(|_| ProbeStop::Index)?;
        match stop {
            Some(stop) => Err(stop),
            None => Ok(probed),
        }
    }
}

/// The book's cache directory, which holds its image index. A session
/// keeps it open while the index is: one directory handle, walked down the
/// chain as the SECTIONS directory is.
pub(crate) fn book_cache_dir<'a, D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &'a Directory<'_, D, T, ND, NF, NV>,
    key: &str,
) -> Option<Directory<'a, D, T, ND, NF, NV>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut dir = root.open_dir(CACHE_ROOT_DIR).ok()?;
    dir.change_dir(CACHE_V2_DIR).ok()?;
    dir.change_dir(key).ok()?;
    Some(dir)
}

/// Opens the book's image index in its cache directory `dir` for a build
/// session, first building it from the ZIP's central directory when there
/// is no valid one (none yet, an interrupted build, or one for another
/// copy of the book). The build streams the directory through `walk`, a
/// run of entries per read, and keeps its records in `scratch`: the
/// compressed scratch and the XHTML window, idle before a session's first
/// spine. Slots follow from the directory alone, so a rebuilt index names
/// every image as before.
///
/// None when the ZIP cannot be read or the card cannot hold the index: the
/// session then lays images out as alt text, as it always did.
#[inline(never)]
pub(crate) fn open_book_images<'d, Z, D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &'d Directory<'_, D, T, ND, NF, NV>,
    source: (u32, u32),
    zip: &mut Z,
    walk: &mut [u8],
    scratch: &mut [u8],
) -> Option<BookImages<'d, D, T, ND, NF, NV>>
where
    Z: EpubZipOps,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let index_key = image_index_key(source);
    if let Ok(file) = dir.open_file_in_dir(IMAGE_INDEX_FILE, Mode::ReadWriteAppend) {
        if let Ok(header) = open_image_index(&mut CardIndex(&file), &index_key) {
            esp_println::println!("images: index holds {} image(s)", header.entry_count);
            return Some(BookImages { file, header });
        }
        // Not formatted with the error: its Debug table would sit in RAM.
        esp_println::println!("images: index unusable; rebuilding");
    }
    let file = dir
        .open_file_in_dir(IMAGE_INDEX_FILE, Mode::ReadWriteCreateOrTruncate)
        .ok()?;
    let mut builder = ImageIndexBuilder::new(scratch, index_key).ok()?;
    // Each pass takes the buckets the last one left, so at most one pass
    // per bucket.
    let header = loop {
        zip.for_each_entry(walk, &mut |entry| {
            builder.offer(entry);
            true
        })
        .ok()?;
        if let BuildStep::Done(header) = builder.end_pass(&mut CardIndex(&file)).ok()? {
            break header;
        }
    };
    esp_println::println!(
        "images: index built with {} image(s){}",
        header.entry_count,
        if header.truncated { ", truncated" } else { "" }
    );
    Some(BookImages { file, header })
}

/// `IMAGES.BIN` as the index reads and writes it: exact reads and writes
/// at byte offsets in the open file.
struct CardIndex<'f, 'v, D, T, const ND: usize, const NF: usize, const NV: usize>(
    &'f File<'v, D, T, ND, NF, NV>,
)
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource;

impl<D, T, const ND: usize, const NF: usize, const NV: usize> IndexIo
    for CardIndex<'_, '_, D, T, ND, NF, NV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    type Error = ();

    fn read_at(&mut self, offset: u32, mut buf: &mut [u8]) -> Result<(), ()> {
        self.0.seek_from_start(offset).map_err(|_| ())?;
        while !buf.is_empty() {
            let read = self.0.read(buf).map_err(|_| ())?;
            if read == 0 {
                return Err(());
            }
            buf = &mut buf[read..];
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u32, buf: &[u8]) -> Result<(), ()> {
        self.0.seek_from_start(offset).map_err(|_| ())?;
        self.0.write(buf).map_err(|_| ())
    }
}
