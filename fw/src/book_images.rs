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
    find_image, is_cover_name, is_image_path, open_image_index, read_image, scan_images,
    write_image_state, BuildStep, DecodeState, DecodeStatus, ImageEntry, ImageIndexBuilder,
    ImageIndexHeader, ImageIndexKey, ImagePlace, ImageProbe, ImageProber, IndexError, IndexIo,
    ProbeStatus, IMAGE_INDEX_FILE,
};
use proto::image::pack::{pack_file, PackRecord, SPOOL_FILE};
use proto::image::path::{path_hash, resolve_image_path};
use ui::reading::{fit_image, fit_inline_image, fits_inline, PageBox};

/// Bytes a probe reads at a time: a stored image's, or a deflated one's
/// compressed input.
const PROBE_INPUT_BYTES: usize = 512;
/// Most bytes a probe reads, or inflates, before it calls the image
/// damaged. A PNG's header is its first 33 bytes; a JPEG's frame header
/// can follow large APPn segments (an EXIF thumbnail can be 64 KB), which
/// a stored image seeks past but a deflated one inflates and drops. Either
/// way a request's probes must not read on until the session's budget runs
/// out, nor inflate a few KB into hundreds of MB between checks for a
/// press.
pub(crate) const PROBE_INPUT_CAP: u32 = 256 * 1024;
/// The part of the XHTML window a deflated probe inflates into; index
/// entries are read into the rest.
const PROBE_OUTPUT_BYTES: usize = 512;

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
    /// An image the decoder takes, naming its slot in the index: `block` is
    /// its size alone in a paragraph (fitted to the page box), `inline` its
    /// size inside a line, or `None` for an illustration too tall to go
    /// inside one (`ui::reading::fits_inline`). `inline_marked` says its
    /// entry already carries the inline mark, so showing it inside a line
    /// writes nothing.
    Shown {
        slot: u16,
        block: (u16, u16),
        inline: Option<(u16, u16)>,
        inline_marked: bool,
    },
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

/// Gives an entry the inline mark (`ImageEntry::inline`), and forgets its
/// decode in each orientation whose size that changes: one whose inline and
/// block sizes are the same keeps it. True when one was forgotten.
pub(crate) fn mark_entry_inline(entry: &mut ImageEntry) -> bool {
    let mut forgot = false;
    for portrait in [false, true] {
        let block = decoded_size(entry, portrait);
        entry.inline = true;
        let changes = decoded_size(entry, portrait) != block;
        entry.inline = false;
        if changes && entry.decode(portrait).status == DecodeStatus::Decoded {
            entry.set_decode(portrait, DecodeState::default());
            entry.wanted = true;
            forgot = true;
        }
    }
    entry.inline = true;
    forgot
}

/// The size an image decodes at in an orientation: its inline size once
/// layout has marked it inline (`ImageEntry::inline`), else its block size.
/// Layout fitted it from the same source size and page box.
pub(crate) fn decoded_size(entry: &ImageEntry, portrait: bool) -> (u16, u16) {
    let page_box = PageBox::for_portrait(portrait);
    let (width, height) = (entry.width.into(), entry.height.into());
    if entry.inline {
        fit_inline_image(page_box, width, height)
    } else {
        fit_image(page_box, width, height)
    }
}

/// Whether the book has images the index holds, from its ZIP's central
/// directory. A directory that cannot be read through (a damaged entry, a
/// failed read) counts as none: lookups by name still find the entries
/// before the damage, so the book builds as it always did, with its images
/// as alt text.
pub(crate) fn book_has_images<Z: EpubZipOps>(zip: &mut Z, buf: &mut [u8]) -> bool {
    zip.has_image_entries(buf).unwrap_or(false)
}

/// Whether a book laid out before sections held images (cache v27) must
/// be laid out again to show them: it has image entries beyond a lone
/// cover. Walks every entry of the central directory, through `buf`.
///
/// A book whose only image's file name says `cover` (`is_cover_name`)
/// answers no, and its index is stamped current. Nearly every book has a
/// JPEG cover, and rebuilding each once after the update would cost every
/// library a rebuild of every book for one page; such a book's cover page
/// keeps its v27 layout (alt text, or nothing for an svg-wrapped cover)
/// until the book is laid out again for another reason, when a fresh
/// build shows it (`book_has_images` still counts it). A directory that
/// cannot be read through counts as no images, as for a build.
pub(crate) fn book_has_images_beyond_a_cover<Z: EpubZipOps>(zip: &mut Z, buf: &mut [u8]) -> bool {
    let (mut images, mut cover) = (0u32, false);
    let walked = zip.for_each_entry(buf, &mut |entry| {
        if is_image_path(entry.name) {
            images += 1;
            cover = is_cover_name(entry.name);
        }
        images < 2
    });
    walked.is_ok() && (images > 1 || (images == 1 && !cover))
}

impl<'v, D, T, const ND: usize, const NF: usize, const NV: usize> BookImages<'v, D, T, ND, NF, NV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Finds the image `src` names in the chapter at ZIP path `chapter`,
    /// resolving it into `path` (the ZIP name scratch, idle while a chapter
    /// streams). A probed image the decoder takes comes back with its sizes
    /// alone in a paragraph and inside a line, fitted to the orientation's
    /// page box (which one the chapter shows is the caller's to decide), and
    /// is marked wanted unless it is already decoded for that orientation. On a spine's first pass an unprobed
    /// image is
    /// marked wanted for the probe that follows; on its second it is
    /// missing, so a spine is laid out at most twice. Either way the entry
    /// keeps `at` when it comes before where layout met the image so far,
    /// so the decode pass knows its place in reading order. Each change to
    /// the entry is one write, and a relayout, which meets every image
    /// where it did before, writes nothing new. Err only when the card
    /// fails.
    pub(crate) fn place(
        &self,
        chapter: &str,
        src: &str,
        path: &mut [u8],
        portrait: bool,
        first_pass: bool,
        at: ImagePlace,
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
        let placement = match entry.probe {
            ProbeStatus::Supported if entry.width > 0 && entry.height > 0 => {
                let page_box = PageBox::for_portrait(portrait);
                let (width, height) = (entry.width.into(), entry.height.into());
                ImagePlacement::Shown {
                    slot,
                    block: fit_image(page_box, width, height),
                    inline: fits_inline(width, height)
                        .then(|| fit_inline_image(page_box, width, height)),
                    inline_marked: entry.inline,
                }
            }
            ProbeStatus::Unprobed if first_pass => ImagePlacement::Unprobed { slot },
            _ => return Ok(ImagePlacement::Missing),
        };
        // Wanted until it is decoded for this orientation (or probed, for
        // an unprobed one).
        let wanted = entry.wanted
            || entry.probe == ProbeStatus::Unprobed
            || entry.decode(portrait).status == DecodeStatus::NotDecoded;
        let seen_at = entry.seen_at.map_or(at, |known| known.min(at));
        if wanted != entry.wanted || entry.seen_at != Some(seen_at) {
            entry.wanted = wanted;
            entry.seen_at = Some(seen_at);
            write_image_state(io, &self.header, slot, &entry).map_err(|_| ())?;
        }
        Ok(placement)
    }

    /// Marks the image in `slot` as shown inside a line at a size other
    /// than its block size (`ImageEntry::inline`), in one write, so it
    /// decodes at its inline size from now on. A decode it already has at
    /// its block size is forgotten in the same write, in each orientation
    /// where the sizes differ (its record becomes an orphan in the pack),
    /// and done again; true when one was. Err only when the card fails. Out
    /// of line: its
    /// entry buffers stay off the line builder's frames.
    #[inline(never)]
    pub(crate) fn mark_inline(&self, slot: u16) -> Result<bool, ()> {
        let io = &mut CardIndex(&self.file);
        let mut entry = match read_image(io, &self.header, slot) {
            Ok(Some(entry)) if !entry.inline => entry,
            Err(IndexError::Io) => return Err(()),
            _ => return Ok(false),
        };
        let forgot = mark_entry_inline(&mut entry);
        write_image_state(io, &self.header, slot, &entry).map_err(|_| ())?;
        Ok(forgot)
    }

    /// Probes the images in `slots` that a chapter's first pass marked
    /// (wanted and not yet probed), and stores each result in its entry as
    /// one write. A probe reads the image's local header by its stored
    /// offset, checks that it names the image, then reads on until it can
    /// judge it (`ImageProber`): a PNG's first 33 bytes, or a JPEG's
    /// segments up to its frame header. A stored image is read a block at
    /// a time and seeks past segments the probe does not read; a deflated
    /// one is inflated that far and the rest dropped. No more than
    /// `PROBE_INPUT_CAP` bytes are read, or inflated, either way. An image
    /// whose archive entry or header is damaged, or that has not said what
    /// it is by the cap, is marked failed and keeps its alt text. A failed
    /// read records nothing and stops the probing, so the image is probed
    /// again later.
    ///
    /// `window` (the XHTML window) holds a deflated probe's output and runs
    /// of index entries, `compressed` the bytes read, and `inflate` the
    /// stream: all idle between a spine's passes. Asks `gives_way` after
    /// each image. Returns how many it probed.
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
        let Some((output, entries)) = window.split_at_mut_checked(PROBE_OUTPUT_BYTES) else {
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
                let path_hash = entry.path_hash;
                let mut prober = ImageProber::new();
                let read = zip.visit_entry_start(
                    source,
                    &mut |name| {
                        core::str::from_utf8(name)
                            .is_ok_and(|name| proto::image::path::path_hash(name) == path_hash)
                    },
                    input,
                    output,
                    inflate,
                    PROBE_INPUT_CAP,
                    &mut prober,
                );
                entry.set_probe(match read {
                    Ok(()) => prober.finish(),
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

    /// The entry in `slot`, as an image token names it; `None` when it
    /// fails its checks, and the image is then treated as missing. Err only
    /// when the card fails.
    pub(crate) fn read(&self, slot: u16) -> Result<Option<ImageEntry>, ()> {
        match read_image(&mut CardIndex(&self.file), &self.header, slot) {
            Ok(found) => Ok(found),
            Err(IndexError::Io) => Err(()),
            Err(_) => Ok(None),
        }
    }

    /// Stores an entry's state in one write.
    pub(crate) fn write_state(&self, slot: u16, entry: &ImageEntry) -> Result<(), ()> {
        write_image_state(&mut CardIndex(&self.file), &self.header, slot, entry).map_err(|_| ())
    }

    /// Bytes of the orientation's pack that decoded images' records take:
    /// what a pack holding only them would be. Reads the index a run of
    /// entries at a time through `buf`.
    pub(crate) fn live_pack_bytes(&self, portrait: bool, buf: &mut [u8]) -> Result<u32, ()> {
        let mut live = 0u32;
        self.scan(buf, |_, entry| {
            if entry.decode(portrait).status == DecodeStatus::Decoded {
                let (width, height) = decoded_size(&entry, portrait);
                let record = PackRecord::new(entry.path_hash, width, height);
                live = live.saturating_add(record.map_or(0, |record| record.record_len()));
            }
            true
        })?;
        Ok(live)
    }

    /// Marks every image decoded in the orientation as not decoded (and
    /// wanted) again, one write each: before its pack is deleted, so nothing
    /// is left pointing into it. Images that failed stay failed.
    pub(crate) fn forget_decodes(&self, portrait: bool, buf: &mut [u8]) -> Result<(), ()> {
        scan_images(
            &mut CardIndex(&self.file),
            &self.header,
            0..self.header.entry_count,
            buf,
            |io, slot, mut entry| {
                if entry.decode(portrait).status == DecodeStatus::Decoded {
                    entry.set_decode(portrait, DecodeState::default());
                    entry.wanted = true;
                    write_image_state(io, &self.header, slot, &entry)?;
                }
                Ok(true)
            },
        )
        .map_err(|_| ())
    }

    /// Calls `visit` with every entry that passes its checks, reading a run
    /// of entries at a time through `buf`, until it returns false. Err only
    /// when the card fails.
    pub(crate) fn scan(
        &self,
        buf: &mut [u8],
        mut visit: impl FnMut(u16, ImageEntry) -> bool,
    ) -> Result<(), ()> {
        scan_images(
            &mut CardIndex(&self.file),
            &self.header,
            0..self.header.entry_count,
            buf,
            |_, slot, entry| Ok(visit(slot, entry)),
        )
        .map_err(|_| ())
    }
}

/// Opens the book's image index in its cache directory `dir` to decode
/// images, never building it: an index no build has written yet holds no
/// image a decode could take, since only layout probes them.
pub(crate) fn open_existing_book_images<
    'd,
    D,
    T,
    const ND: usize,
    const NF: usize,
    const NV: usize,
>(
    dir: &'d Directory<'_, D, T, ND, NF, NV>,
    source: (u32, u32),
) -> Option<BookImages<'d, D, T, ND, NF, NV>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = dir
        .open_file_in_dir(IMAGE_INDEX_FILE, Mode::ReadWriteAppend)
        .ok()?;
    let header = open_image_index(&mut CardIndex(&file), &image_index_key(source)).ok()?;
    Some(BookImages { file, header })
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
/// every image as before, but every image is undecoded again: `rebuilt`
/// says so.
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
    rebuilt: &mut bool,
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
    // Nothing in a new index points into the old packs, so every record in
    // them would be an orphan: delete them first, and any spool a power cut
    // left, so a cut part way leaves an index that is still rebuilt.
    for name in [pack_file(false), pack_file(true), SPOOL_FILE] {
        crate::sd_session::remove_file_reclaiming_clusters(dir, name);
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
    *rebuilt = true;
    Some(BookImages { file, header })
}

/// `IMAGES.BIN` as the index reads and writes it: exact reads and writes
/// at byte offsets in the open file. A page's draw reads through it too,
/// from a file it opened read-only.
pub(crate) struct CardIndex<'f, 'v, D, T, const ND: usize, const NF: usize, const NV: usize>(
    pub(crate) &'f File<'v, D, T, ND, NF, NV>,
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

    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), ()> {
        self.0.seek_from_start(offset).map_err(|_| ())?;
        crate::reader_cache_files::read_exact_file(self.0, buf)
    }

    fn write_at(&mut self, offset: u32, buf: &[u8]) -> Result<(), ()> {
        self.0.seek_from_start(offset).map_err(|_| ())?;
        self.0.write(buf).map_err(|_| ())
    }
}
