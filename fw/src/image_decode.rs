//! Decoding a book's images into its packs (`proto::image::pack`): each
//! image once per orientation, at the size layout fitted it to, nearest the
//! reader first.
//!
//! A decode reads the image from the book's ZIP at the local-header offset
//! its index entry keeps, runs it through the PNG decoder and the ditherer,
//! and appends the 1 bpp rows to the orientation's pack behind a
//! `PackRecord` header, through a write buffer. A stored image streams
//! straight from the ZIP. A deflated one is first inflated to a spool file
//! in the book's cache directory and decoded from that: the decoder's zlib
//! stream needs the one inflater the ZIP read would need too. Only once the
//! decoder's `finish` succeeds and the pack's new length is on the card is
//! the index entry updated, in one write. A decode that stops part way
//! leaves an orphan tail in the pack and nothing pointing at it.
//!
//! What a decode records, by how it ends (the same classes as a probe's):
//!
//! - It succeeds: the entry gets the record's offset, decoded.
//! - A content fault (a corrupt PNG, one the decoder does not take after
//!   all, a damaged ZIP entry, a deflated image past [`SPOOL_MAX_BYTES`])
//!   marks that orientation failed for good: the image keeps its
//!   placeholder. So does a pack that would pass `MAX_PACK_BYTES` with the
//!   records images still use; one filled mostly by orphan tails is started
//!   afresh instead (see `ImageSession::write_record`).
//! - A card I/O error, or a read budget run out, records nothing, so the
//!   image is tried again later.
//! - A press cancels it, also recording nothing. The sink asks between
//!   output rows, the feed loop between pieces of [`FEED_BYTES`], which
//!   bounds the decoder's work in between, and a spool between pieces of
//!   compressed input.
//!
//! Every buffer comes from the EPUB scratch, idle whenever no spine is
//! being laid out (see [`ImageScratch`]), so decoding adds no static RAM.

use crate::book_images::BookImages;
use crate::reader_cache_files::read_exact_file;
use crate::reader_store::ReaderStore;
use core::cell::Cell;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use proto::cache::BookV2SectionRecord;
use proto::epub::{
    entry_payload_at, inflate_to_sink, read_payload_at, OwnedZipEntry, ReadAt, ZipError,
    ZipInflateScratch,
};
use proto::image::dither::Ditherer;
use proto::image::index::{DecodeState, DecodeStatus, ImageEntry, ImagePlace, ProbeStatus};
pub(crate) use proto::image::pack::SPOOL_FILE;
use proto::image::pack::{pack_file, PackRecord, MAX_PACK_BYTES};
use proto::image::png::{PngDecoder, PngError};
use proto::image::{LumaSink, SinkError};
use ui::reading::{fit_image, PageBox};

/// Largest deflated image spooled, by its declared size: about 14 s of
/// card writes and reads at the X4's measured rates. Larger ones are
/// refused like an image past the decoder's pixel cap.
pub(crate) const SPOOL_MAX_BYTES: u32 = 4 * 1024 * 1024;
/// Bytes the decoder is fed between checks for a press. Deflate expands at
/// most about 1,032 times, so one piece inflates at most about 1 MB: a few
/// hundred milliseconds even for an image scaled down so far that output
/// rows, where the sink checks too, come rarely.
pub(crate) const FEED_BYTES: usize = 1024;
/// Pages after the one on screen whose images an idle session decodes
/// first.
pub(crate) const WINDOW_AHEAD: u32 = 3;

/// What a decode borrows, all from the EPUB scratch and all idle between
/// spines and between build sessions. A decode never overlaps a spine's
/// layout: image passes run in sessions of their own.
pub(crate) struct ImageScratch<'a> {
    /// ZIP reads (a stored image's bytes, a deflated one's compressed
    /// input) and the spool read back: half the compressed scratch.
    pub(crate) input: &'a mut [u8],
    /// The pack write buffer: the other half.
    pub(crate) stage: &'a mut [u8],
    /// The XHTML window: the ditherer's error rows, the packed row, and the
    /// PNG decoder's scratch, which takes the rest; before that, a deflated
    /// image's inflate output on its way to the spool, or the index read
    /// back a run of entries at a time.
    pub(crate) window: &'a mut [u8],
    /// The local header's name, checked against the index entry: the ZIP
    /// name scratch.
    pub(crate) name: &'a mut [u8],
    /// The ZIP inflater: the spool's outer stream, then the PNG's zlib
    /// stream.
    pub(crate) inflate: &'a mut ZipInflateScratch,
}

/// How one image's decode ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum DecodeOutcome {
    /// In the pack, and the index says where.
    Decoded,
    /// A press stopped it; nothing recorded.
    Cancelled,
    /// A content fault: marked failed for this orientation.
    Failed,
    /// A card or read-budget fault: nothing recorded, tried again later.
    Retry,
}

impl DecodeOutcome {
    /// Its name in bench lines. Cut out of one string by arithmetic rather
    /// than matched: the ESP32-C3 build keeps a `match`'s lookup table in
    /// RAM.
    pub(crate) fn label(self) -> &'static str {
        const NAMES: &str = "decoded   cancelled failed    retry     ";
        let at = self as usize * 10;
        NAMES.get(at..at + 10).map_or("", str::trim_end)
    }
}

/// What a session did, for its `bench: image_session` line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionStats {
    /// Index entries looked at for a page, or picked by the rest pass.
    pub(crate) checked: u16,
    pub(crate) decoded: u16,
    pub(crate) failed: u16,
    pub(crate) cancelled: u16,
    pub(crate) retry: u16,
    /// The pack was started afresh, so decodes recorded before are gone.
    pub(crate) pack_restarted: bool,
}

impl SessionStats {
    fn note(&mut self, outcome: DecodeOutcome) {
        let count = match outcome {
            DecodeOutcome::Decoded => &mut self.decoded,
            DecodeOutcome::Failed => &mut self.failed,
            DecodeOutcome::Cancelled => &mut self.cancelled,
            DecodeOutcome::Retry => &mut self.retry,
        };
        *count = count.saturating_add(1);
    }
}

/// Why a decode stopped short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    Cancelled,
    Content,
    Io,
}

fn zip_fault(error: ZipError) -> Fault {
    match error {
        ZipError::Io => Fault::Io,
        _ => Fault::Content,
    }
}

fn png_fault(error: PngError) -> Fault {
    match error {
        PngError::Sink(SinkError::Cancelled) => Fault::Cancelled,
        PngError::Sink(SinkError::Io) => Fault::Io,
        // Including a scratch too small for the image's rows: a PNG the
        // decoder does not take after all.
        _ => Fault::Content,
    }
}

/// The size an image is decoded at in this orientation, when it still
/// needs decoding there: an image the decoder takes, neither decoded nor
/// failed in this orientation yet. Layout fitted it the same way, from the
/// same source size and page box, so this is the size its blocks draw.
pub(crate) fn pending_size(entry: &ImageEntry, portrait: bool) -> Option<(u16, u16)> {
    (entry.probe == ProbeStatus::Supported
        && entry.width > 0
        && entry.height > 0
        && entry.decode(portrait).status == DecodeStatus::NotDecoded)
        .then(|| {
            fit_image(
                PageBox::for_portrait(portrait),
                entry.width.into(),
                entry.height.into(),
            )
        })
}

/// The pages whose images an idle session decodes before anything else,
/// nearest first: the [`WINDOW_AHEAD`] after the one on screen, then the
/// one before it, then the one on screen (whose request decoded its images
/// unless a card fault stopped it).
pub(crate) fn window_pages(page: u32) -> impl Iterator<Item = u32> {
    (1..=WINDOW_AHEAD)
        .map(move |ahead| page.checked_add(ahead))
        .chain([page.checked_sub(1), Some(page)])
        .flatten()
}

/// Whether the store's note says every image on the pages of `page`'s
/// window that exist needs no decode, so a window pass would find nothing.
pub(crate) fn window_settled(store: &ReaderStore, page: u32) -> bool {
    window_pages(page)
        .filter(|&page| store.page_exists(page))
        .all(|page| store.images_settled(page))
}

/// What an idle step does about the open book's images.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdleImageStep {
    /// The window around the page on screen.
    Window,
    /// The rest of the book, once its text is laid out.
    Rest,
    /// Nothing.
    Nothing,
}

/// The idle image work done since the reader last did anything, kept in the
/// display task's idle state and cleared by any command: whether the
/// window pass has run for the page on screen, and whether the rest pass
/// last stopped on a card fault. What is settled for good (pages, or the
/// whole book) is the store's note, which lasts across commands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct IdleImageWork {
    pub(crate) window_tried: bool,
    pub(crate) rest_stuck: bool,
}

impl IdleImageWork {
    /// What an idle step on `page` does about images first. A book without
    /// images, or whose images are all settled, does nothing, so it opens no
    /// image session after a page turn; nor does a window the note already
    /// covers. The rest pass waits for the text layout.
    pub(crate) fn next(
        self,
        store: &ReaderStore,
        page: u32,
        layout_pending: bool,
    ) -> IdleImageStep {
        if !store.book_has_images() || store.book_images_settled() {
            IdleImageStep::Nothing
        } else if !self.window_tried && !window_settled(store, page) {
            IdleImageStep::Window
        } else if !layout_pending && !self.rest_stuck {
            IdleImageStep::Rest
        } else {
            IdleImageStep::Nothing
        }
    }
}

/// How far an image placed at `seen_at` lies from the reader at `reader`,
/// in reading order: forward to the book's end, then on from its start.
/// An image no layout has placed yet comes last.
pub(crate) fn reading_distance(seen_at: Option<ImagePlace>, reader: ImagePlace) -> u32 {
    let position = |place: ImagePlace| (u32::from(place.spine) << 16) | u32::from(place.order);
    seen_at.map_or(u32::MAX, |place| {
        position(place).wrapping_sub(position(reader))
    })
}

const NEAREST_RECORD_BYTES: usize = 8;

/// The images the rest pass decodes next: the nearest to the reader, as
/// many as its scratch holds (8 bytes each), kept as entries stream past
/// and then put in order. One read of the index picks a whole batch.
pub(crate) struct Nearest<'a> {
    records: &'a mut [[u8; NEAREST_RECORD_BYTES]],
    len: usize,
}

impl<'a> Nearest<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self {
            records: buf.as_chunks_mut::<NEAREST_RECORD_BYTES>().0,
            len: 0,
        }
    }

    fn distance(record: &[u8; NEAREST_RECORD_BYTES]) -> u32 {
        u32::from_le_bytes([record[0], record[1], record[2], record[3]])
    }

    /// Keeps `slot` if it is among the nearest so far.
    pub(crate) fn offer(&mut self, slot: u16, distance: u32) {
        let mut record = [0u8; NEAREST_RECORD_BYTES];
        record[..4].copy_from_slice(&distance.to_le_bytes());
        record[4..6].copy_from_slice(&slot.to_le_bytes());
        if self.len < self.records.len() {
            self.records[self.len] = record;
            self.len += 1;
            return;
        }
        // Full: replace the farthest, when this one is nearer.
        let farthest = self.records[..self.len]
            .iter()
            .enumerate()
            .max_by_key(|(_, kept)| Self::distance(kept))
            .map(|(index, _)| index);
        if let Some(index) = farthest {
            if distance < Self::distance(&self.records[index]) {
                self.records[index] = record;
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The kept slots, nearest first.
    pub(crate) fn into_slots(self) -> impl Iterator<Item = u16> + 'a {
        let Self { records, len } = self;
        let kept = &mut records[..len];
        kept.sort_unstable_by_key(Self::distance);
        let kept: &'a [[u8; NEAREST_RECORD_BYTES]] = kept;
        kept.iter()
            .map(|record| u16::from_le_bytes([record[4], record[5]]))
    }
}

const PAGE_IMAGE_BYTES: usize = 6;

/// An image a page shows: its slot, and the size the page draws it at when
/// that is known (for a page the store holds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PageImage {
    pub(crate) slot: u16,
    pub(crate) drawn: Option<(u16, u16)>,
}

/// Images a pass is to look at, in order, kept in borrowed scratch (6
/// bytes each); past its room, the rest are left for later.
pub(crate) struct PageImages<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> PageImages<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    pub(crate) fn push(&mut self, image: PageImage) -> bool {
        let Some(record) = self
            .buf
            .get_mut(self.len * PAGE_IMAGE_BYTES..(self.len + 1) * PAGE_IMAGE_BYTES)
        else {
            return false;
        };
        // No fitted image is 0 wide, so a zero width says the size is not
        // known.
        let (width, height) = image.drawn.unwrap_or((0, 0));
        record[0..2].copy_from_slice(&image.slot.to_le_bytes());
        record[2..4].copy_from_slice(&width.to_le_bytes());
        record[4..6].copy_from_slice(&height.to_le_bytes());
        self.len += 1;
        true
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn get(&self, index: usize) -> Option<PageImage> {
        if index >= self.len {
            return None;
        }
        let record = self
            .buf
            .get(index * PAGE_IMAGE_BYTES..(index + 1) * PAGE_IMAGE_BYTES)?;
        let at = |offset: usize| u16::from_le_bytes([record[offset], record[offset + 1]]);
        let (width, height) = (at(2), at(4));
        Some(PageImage {
            slot: at(0),
            drawn: (width > 0).then_some((width, height)),
        })
    }
}

/// The images page `page` shows, when the store holds it (the caller
/// checks), at the size and in the order the page draws them. Reads
/// nothing from the card, so a page without images costs a book nothing.
/// False when the page is not held, or `out` filled before its last image.
pub(crate) fn resident_page_images(
    store: &ReaderStore,
    page: u32,
    out: &mut PageImages<'_>,
) -> bool {
    let local = store.local_page_for_global(page);
    let Some(record) = store.pages.get(local).filter(|_| local < store.page_count) else {
        return false;
    };
    let mut kept = true;
    ui::reading::for_each_drawable_block(store, *record, |block| {
        if let Some(image) = block.image.and_then(|image| {
            image.slot.map(|slot| PageImage {
                slot,
                drawn: Some((image.width, image.height)),
            })
        }) {
            kept = out.push(image);
        }
        kept
    });
    kept
}

/// The images on the pages an idle session looks at first
/// ([`window_pages`]), in that order: from the store for the pages it
/// holds, and from their section files for the rest (a few small reads a
/// page). Reads no image. Returns the run of pages listed, the window's
/// pages that exist, or `None` when one of them could not be listed whole
/// (its section unreadable or laid out for other settings, or `out` full),
/// so a pass cannot vouch for it.
pub(crate) fn list_window_images<D, T, const ND: usize, const NF: usize, const NV: usize>(
    store: &ReaderStore,
    index: usize,
    page: u32,
    book_dir: &Directory<'_, D, T, ND, NF, NV>,
    source: (u32, u32),
    out: &mut PageImages<'_>,
) -> Option<(u32, u32)>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut whole = true;
    let (mut from, mut to, mut pages) = (u32::MAX, 0, 0);
    for page in window_pages(page).filter(|&page| store.page_exists(page)) {
        let listed = if store.covers_global_page(index, page) {
            resident_page_images(store, page, out)
        } else {
            store.section_for_global_page(page).is_some_and(|section| {
                let mut kept = true;
                crate::reader_cache_files::read_section_page_images(
                    book_dir,
                    source,
                    store,
                    section,
                    page,
                    |slot| {
                        kept = out.push(PageImage { slot, drawn: None });
                        kept
                    },
                ) && kept
            })
        };
        whole &= listed;
        from = from.min(page);
        to = to.max(page + 1);
        pages += 1;
    }
    // The pages that exist run on from the book's first, so the window's
    // are one run; anything else vouches for nothing.
    (whole && pages > 0 && to - from == pages).then_some((from, to))
}

/// Where the reader is in the book's image order: the spine of the page on
/// screen, and the order of the first image at or after it in the sections
/// the store holds (one index read), or just past the last one before it.
/// The rest pass decodes forward from there.
pub(crate) fn reader_place<D, T, const ND: usize, const NF: usize, const NV: usize>(
    store: &ReaderStore,
    page: u32,
    images: &BookImages<'_, D, T, ND, NF, NV>,
) -> ImagePlace
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let spine = store
        .section_for_global_page(page)
        .map_or(0, |section: BookV2SectionRecord| section.spine);
    let mut place = ImagePlace { spine, order: 0 };
    let local = store.local_page_for_global(page);
    let Some(first) = store.pages.get(local).filter(|_| local < store.page_count) else {
        return place;
    };
    let slot_of = |block: usize| {
        store
            .block_record(block)
            .filter(|record| record.is_image())
            .and_then(|_| proto::image::token::decode_image_token(store.block_text(block)))
            .map(|token| token.slot)
    };
    let first = usize::from(first.first_block);
    let ahead = (first..store.block_count)
        .find_map(slot_of)
        .map(|slot| (slot, 0));
    let behind = || (0..first).rev().find_map(slot_of).map(|slot| (slot, 1));
    if let Some((slot, past)) = ahead.or_else(behind) {
        if let Ok(Some(Some(seen))) = images.read(slot).map(|entry| entry.map(|e| e.seen_at)) {
            if seen.spine == spine {
                place.order = seen.order.saturating_add(past);
            }
        }
    }
    place
}

/// One session's image decoding for a book: its index, its archive (opened
/// the first time an image needs reading), and the orientation's pack
/// (opened the first time one is written).
pub(crate) struct ImageSession<'s, R, O, D, T, const ND: usize, const NF: usize, const NV: usize>
where
    R: ReadAt,
    O: FnMut() -> Option<R>,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    dir: &'s Directory<'s, D, T, ND, NF, NV>,
    images: BookImages<'s, D, T, ND, NF, NV>,
    open_archive: O,
    archive: Option<R>,
    pack: Option<File<'s, D, T, ND, NF, NV>>,
    portrait: bool,
    now_ms: fn() -> u64,
    pub(crate) stats: SessionStats,
    /// A small cap in place of `MAX_PACK_BYTES`, so tests can fill a pack.
    #[cfg(test)]
    pack_cap: u32,
}

impl<'s, R, O, D, T, const ND: usize, const NF: usize, const NV: usize>
    ImageSession<'s, R, O, D, T, ND, NF, NV>
where
    R: ReadAt,
    O: FnMut() -> Option<R>,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// `dir` is the book's cache directory, which holds the index, the
    /// packs and the spool; `open_archive` opens the book's ZIP for reading
    /// at offsets. `now_ms` times the decodes for their bench lines.
    pub(crate) fn new(
        dir: &'s Directory<'s, D, T, ND, NF, NV>,
        images: BookImages<'s, D, T, ND, NF, NV>,
        open_archive: O,
        portrait: bool,
        now_ms: fn() -> u64,
    ) -> Self {
        Self {
            dir,
            images,
            open_archive,
            archive: None,
            pack: None,
            portrait,
            now_ms,
            stats: SessionStats::default(),
            #[cfg(test)]
            pack_cap: MAX_PACK_BYTES,
        }
    }

    pub(crate) fn images(&self) -> &BookImages<'s, D, T, ND, NF, NV> {
        &self.images
    }

    /// Caps the pack at `cap` bytes instead of `MAX_PACK_BYTES`.
    #[cfg(test)]
    pub(crate) fn with_pack_cap(mut self, cap: u32) -> Self {
        self.pack_cap = cap;
        self
    }

    /// The most a pack may hold.
    fn pack_cap(&self) -> u32 {
        #[cfg(test)]
        return self.pack_cap;
        #[cfg(not(test))]
        MAX_PACK_BYTES
    }

    /// Decodes a page's image unless it needs nothing here (see
    /// [`pending_size`]), or the page draws it at another size than the
    /// index fits it to, which only a damaged section can say. `None` when
    /// nothing was tried.
    pub(crate) fn decode_page_image(
        &mut self,
        image: PageImage,
        scratch: &mut ImageScratch<'_>,
        gives_way: &dyn Fn() -> bool,
    ) -> Option<DecodeOutcome> {
        self.stats.checked = self.stats.checked.saturating_add(1);
        let entry = match self.images.read(image.slot) {
            Ok(Some(entry)) => entry,
            Ok(None) => return None,
            Err(()) => {
                self.stats.note(DecodeOutcome::Retry);
                return Some(DecodeOutcome::Retry);
            }
        };
        let size = pending_size(&entry, self.portrait)?;
        if image.drawn.is_some_and(|drawn| drawn != size) {
            return None;
        }
        Some(self.decode(image.slot, entry, size, scratch, gives_way))
    }

    /// The rest pass: decodes the images still to decode in this
    /// orientation, nearest the reader at `reader` first, until a press,
    /// `out_of_time`, or a card fault. It reads the index a batch of
    /// `list.len() / 8` at a time through the window, giving way to a press
    /// as it reads too. True only when a read of the whole index found
    /// nothing left to decode.
    pub(crate) fn decode_nearest(
        &mut self,
        reader: ImagePlace,
        list: &mut [u8],
        scratch: &mut ImageScratch<'_>,
        gives_way: &dyn Fn() -> bool,
        out_of_time: &dyn Fn() -> bool,
    ) -> bool {
        loop {
            let mut nearest = Nearest::new(&mut *list);
            let portrait = self.portrait;
            let mut pressed = false;
            let scanned = self.images.scan(scratch.window, |slot, entry| {
                pressed = gives_way();
                if !pressed && pending_size(&entry, portrait).is_some() {
                    nearest.offer(slot, reading_distance(entry.seen_at, reader));
                }
                !pressed
            });
            if scanned.is_err() {
                self.stats.note(DecodeOutcome::Retry);
                return false;
            }
            if pressed {
                return false;
            }
            if nearest.is_empty() {
                return true;
            }
            // Each batch settles what it decodes, so the next read of the
            // index finds the next nearest; a batch that settles nothing
            // (entries that no longer read as pending) ends the pass.
            let mut settled = false;
            for slot in nearest.into_slots() {
                if gives_way() || out_of_time() {
                    return false;
                }
                match self.decode_page_image(PageImage { slot, drawn: None }, scratch, gives_way) {
                    Some(DecodeOutcome::Cancelled | DecodeOutcome::Retry) => return false,
                    Some(DecodeOutcome::Decoded | DecodeOutcome::Failed) => settled = true,
                    None => {}
                }
            }
            if !settled {
                return false;
            }
        }
    }

    /// Decodes one image at `size` into the pack and records the outcome in
    /// its entry, logging a `bench: image_decode` line.
    fn decode(
        &mut self,
        slot: u16,
        mut entry: ImageEntry,
        size: (u16, u16),
        scratch: &mut ImageScratch<'_>,
        gives_way: &dyn Fn() -> bool,
    ) -> DecodeOutcome {
        let started = (self.now_ms)();
        let mut spool_ms = 0;
        let written = self.write_record(&entry, size, scratch, gives_way, &mut spool_ms);
        let state = match written {
            Ok(offset) => Some(DecodeState {
                status: DecodeStatus::Decoded,
                pack_offset: offset,
            }),
            Err(Fault::Content) => Some(DecodeState {
                status: DecodeStatus::Failed,
                pack_offset: 0,
            }),
            Err(Fault::Cancelled | Fault::Io) => None,
        };
        let outcome = match (written, state) {
            (Err(Fault::Cancelled), _) => DecodeOutcome::Cancelled,
            (_, None) => DecodeOutcome::Retry,
            (_, Some(state)) => {
                entry.set_decode(self.portrait, state);
                // Nothing left to want here; layout in the other
                // orientation marks it again.
                entry.wanted = false;
                match self.images.write_state(slot, &entry) {
                    Ok(()) if state.status == DecodeStatus::Decoded => DecodeOutcome::Decoded,
                    Ok(()) => DecodeOutcome::Failed,
                    Err(()) => DecodeOutcome::Retry,
                }
            }
        };
        if outcome == DecodeOutcome::Retry {
            // Reopen the pack at its end next time rather than trust where
            // a failed write left it.
            self.pack = None;
        }
        self.stats.note(outcome);
        esp_println::println!(
            "bench: image_decode slot={} src_bytes={} out={}x{} ms={} spool_ms={} outcome={}",
            slot,
            entry.compressed_size,
            size.0,
            size.1,
            (self.now_ms)().saturating_sub(started),
            spool_ms,
            outcome.label()
        );
        outcome
    }

    /// Appends the image's record to the pack and returns its offset, once
    /// every row and the pack's length are on the card.
    #[inline(never)]
    fn write_record(
        &mut self,
        entry: &ImageEntry,
        (width, height): (u16, u16),
        scratch: &mut ImageScratch<'_>,
        gives_way: &dyn Fn() -> bool,
        spool_ms: &mut u64,
    ) -> Result<u32, Fault> {
        let record = PackRecord::new(entry.path_hash, width, height).ok_or(Fault::Content)?;
        if self.pack.is_none() {
            self.pack = self
                .dir
                .open_file_in_dir(pack_file(self.portrait), Mode::ReadWriteCreateOrAppend)
                .ok();
        }
        let pack_len = self.pack.as_ref().ok_or(Fault::Io)?.length();
        let fits = pack_len
            .checked_add(record.record_len())
            .is_some_and(|end| end <= self.pack_cap());
        if !fits {
            self.start_pack_afresh(record.record_len(), scratch)?;
        }
        if self.archive.is_none() {
            self.archive = (self.open_archive)();
        }
        let (Some(archive), Some(pack)) = (self.archive.as_mut(), self.pack.as_ref()) else {
            return Err(Fault::Io);
        };
        let offset = pack.length();
        pack.seek_from_end(0).map_err(|_| Fault::Io)?;
        let zip = OwnedZipEntry {
            compression_method: entry.method,
            compressed_size: entry.compressed_size,
            uncompressed_size: entry.uncompressed_size,
            local_header_offset: entry.local_header_offset,
        };
        let path_hash = entry.path_hash;
        let payload = entry_payload_at(archive, zip, scratch.name, &mut |name| {
            core::str::from_utf8(name)
                .is_ok_and(|name| proto::image::path::path_hash(name) == path_hash)
        })
        .map_err(zip_fault)?;
        match entry.method {
            0 => decode_stream(
                record,
                entry.compressed_size,
                &mut |at, buf| read_payload_at(archive, payload + at, buf).map_err(zip_fault),
                pack,
                scratch,
                gives_way,
            )?,
            8 => {
                let started = (self.now_ms)();
                let spooled = spool(archive, zip, payload, self.dir, scratch, gives_way);
                *spool_ms = (self.now_ms)().saturating_sub(started);
                let decoded = spooled.and_then(|(spool, len)| {
                    decode_stream(
                        record,
                        len,
                        &mut |at, buf| read_spool(&spool, at, buf),
                        pack,
                        scratch,
                        gives_way,
                    )
                });
                // The spool's handle is closed by now: delete it.
                crate::sd_session::remove_file_reclaiming_clusters(self.dir, SPOOL_FILE);
                decoded?
            }
            _ => return Err(Fault::Content),
        }
        // The index may point at the record only once the pack's directory
        // entry covers it.
        pack.flush().map_err(|_| Fault::Io)?;
        Ok(offset)
    }

    /// The pack cannot take a record of `needed` bytes. Records are only
    /// ever appended, so a pack fills with the orphan tails cancelled
    /// decodes leave as well as with the records images use. When those
    /// records and this one would fill no more than half of it, the rest is
    /// orphans: every decode in this orientation is forgotten (one index
    /// write each), the pack is deleted, and a new one begun, so the images
    /// decode again as they are needed. Otherwise the book's images do not
    /// fit a pack, and this one fails. Uses the window to read the index.
    fn start_pack_afresh(
        &mut self,
        needed: u32,
        scratch: &mut ImageScratch<'_>,
    ) -> Result<(), Fault> {
        let live = self
            .images
            .live_pack_bytes(self.portrait, scratch.window)
            .map_err(|_| Fault::Io)?;
        if live.saturating_add(needed) > self.pack_cap() / 2 {
            return Err(Fault::Content);
        }
        // Nothing may point into the pack once it is gone, so the index
        // forgets first; a cut in between leaves orphans, as a cancel does.
        self.images
            .forget_decodes(self.portrait, scratch.window)
            .map_err(|_| Fault::Io)?;
        self.stats.pack_restarted = true;
        self.pack = None;
        let name = pack_file(self.portrait);
        crate::sd_session::remove_file_reclaiming_clusters(self.dir, name);
        self.pack = self
            .dir
            .open_file_in_dir(name, Mode::ReadWriteCreateOrTruncate)
            .ok();
        esp_println::println!("images: pack started afresh; {} bytes were in use", live);
        self.pack.as_ref().map(|_| ()).ok_or(Fault::Io)
    }
}

/// Inflates a deflated image to the spool, through `scratch.input` and the
/// window, capped at its declared size (and [`SPOOL_MAX_BYTES`]). Returns
/// the spool, rewound, and its length.
#[inline(never)]
fn spool<'d, R, D, T, const ND: usize, const NF: usize, const NV: usize>(
    archive: &mut R,
    zip: OwnedZipEntry,
    payload: u32,
    dir: &'d Directory<'d, D, T, ND, NF, NV>,
    scratch: &mut ImageScratch<'_>,
    gives_way: &dyn Fn() -> bool,
) -> Result<(File<'d, D, T, ND, NF, NV>, u32), Fault>
where
    R: ReadAt,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let declared = zip.uncompressed_size;
    if declared > SPOOL_MAX_BYTES {
        return Err(Fault::Content);
    }
    let file = dir
        .open_file_in_dir(SPOOL_FILE, Mode::ReadWriteCreateOrTruncate)
        .map_err(|_| Fault::Io)?;
    let mut written = 0u32;
    let stop = Cell::new(None);
    let inflated = inflate_to_sink(
        // Asked before every piece of input, so a stream that never fills
        // the output (empty stored blocks, say) still gives way.
        &mut |at, buf| {
            if gives_way() {
                stop.set(Some(Fault::Cancelled));
                return Err(ZipError::OutputTooSmall);
            }
            read_payload_at(archive, payload + at, buf)
        },
        zip.compressed_size,
        scratch.input,
        scratch.window,
        scratch.inflate,
        &mut |chunk| {
            let fault = if gives_way() {
                Some(Fault::Cancelled)
            } else if chunk.len() as u32 > declared - written {
                // More than the entry says it holds: damage.
                Some(Fault::Content)
            } else if file.write(chunk).is_err() {
                Some(Fault::Io)
            } else {
                None
            };
            if let Some(fault) = fault {
                stop.set(Some(fault));
                return Err(ZipError::OutputTooSmall);
            }
            written += chunk.len() as u32;
            Ok(())
        },
    );
    if let Some(fault) = stop.get() {
        return Err(fault);
    }
    inflated.map_err(zip_fault)?;
    file.seek_from_start(0).map_err(|_| Fault::Io)?;
    Ok((file, written))
}

fn read_spool<D, T, const ND: usize, const NF: usize, const NV: usize>(
    spool: &File<'_, D, T, ND, NF, NV>,
    at: u32,
    buf: &mut [u8],
) -> Result<(), Fault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if spool.offset() != at {
        spool.seek_from_start(at).map_err(|_| Fault::Io)?;
    }
    read_exact_file(spool, buf).map_err(|_| Fault::Io)
}

/// Fills a buffer with the image's bytes from an offset into it.
type ReadPiece<'a> = dyn FnMut(u32, &mut [u8]) -> Result<(), Fault> + 'a;

/// Decodes `total` bytes of PNG, read a piece at a time through `read`,
/// into `record`'s rows, appended to the pack after its header.
#[inline(never)]
fn decode_stream<D, T, const ND: usize, const NF: usize, const NV: usize>(
    record: PackRecord,
    total: u32,
    read: &mut ReadPiece<'_>,
    pack: &File<'_, D, T, ND, NF, NV>,
    scratch: &mut ImageScratch<'_>,
    gives_way: &dyn Fn() -> bool,
) -> Result<(), Fault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let width = u32::from(record.width);
    let (cells, rest) = scratch
        .window
        .split_at_mut_checked(Ditherer::scratch_bytes(width))
        .ok_or(Fault::Content)?;
    let (packed, decoder_scratch) = rest
        .split_at_mut_checked(usize::from(record.stride))
        .ok_or(Fault::Content)?;
    let mut sink = PackSink {
        ditherer: Ditherer::new(cells, width).map_err(|_| Fault::Content)?,
        packed,
        out: Staged {
            pack,
            buf: &mut *scratch.stage,
            len: 0,
        },
        gives_way,
        rows: 0,
    };
    let mut decoder =
        PngDecoder::new(decoder_scratch, width, u32::from(record.height)).map_err(png_fault)?;
    sink.out.push(&record.encode()).map_err(|_| Fault::Io)?;
    let inflate = scratch.inflate.inflater();
    let input = &mut *scratch.input;
    let mut fed = 0u32;
    while fed < total && !decoder.is_complete() {
        let len = (total - fed).min(input.len() as u32) as usize;
        let piece = &mut input[..len];
        read(fed, piece)?;
        for bytes in piece.chunks(FEED_BYTES) {
            if gives_way() {
                return Err(Fault::Cancelled);
            }
            decoder.feed(inflate, bytes, &mut sink).map_err(png_fault)?;
            if decoder.is_complete() {
                break;
            }
        }
        fed += len as u32;
    }
    decoder.finish(inflate, &mut sink).map_err(png_fault)?;
    if sink.rows != u32::from(record.height) {
        return Err(Fault::Content);
    }
    sink.out.flush().map_err(|_| Fault::Io)
}

/// Pack writes gathered into whole runs of the write buffer.
struct Staged<'a, 'f, D, T, const ND: usize, const NF: usize, const NV: usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    pack: &'f File<'f, D, T, ND, NF, NV>,
    buf: &'a mut [u8],
    len: usize,
}

impl<D, T, const ND: usize, const NF: usize, const NV: usize> Staged<'_, '_, D, T, ND, NF, NV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if bytes.len() > self.buf.len() - self.len {
            self.flush()?;
        }
        match self.buf.get_mut(self.len..self.len + bytes.len()) {
            Some(room) => {
                room.copy_from_slice(bytes);
                self.len += bytes.len();
                Ok(())
            }
            None => self.pack.write(bytes).map_err(|_| ()),
        }
    }

    fn flush(&mut self) -> Result<(), ()> {
        let len = core::mem::take(&mut self.len);
        if len == 0 {
            return Ok(());
        }
        self.pack.write(&self.buf[..len]).map_err(|_| ())
    }
}

/// Dithers each gray row the decoder hands over and stages it for the
/// pack, giving way to a press between rows.
struct PackSink<'a, 'f, D, T, const ND: usize, const NF: usize, const NV: usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    ditherer: Ditherer<'a>,
    packed: &'a mut [u8],
    out: Staged<'a, 'f, D, T, ND, NF, NV>,
    gives_way: &'a dyn Fn() -> bool,
    rows: u32,
}

impl<D, T, const ND: usize, const NF: usize, const NV: usize> LumaSink
    for PackSink<'_, '_, D, T, ND, NF, NV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn row(&mut self, _y: u32, luma: &[u8]) -> Result<(), SinkError> {
        if (self.gives_way)() {
            return Err(SinkError::Cancelled);
        }
        // The decoder hands over exactly the width it was built for, which
        // the ditherer was too, so this cannot fail; were it to, nothing is
        // recorded and the image is tried again.
        self.ditherer
            .row(luma, self.packed)
            .map_err(|_| SinkError::Io)?;
        self.out.push(self.packed).map_err(|_| SinkError::Io)?;
        self.rows += 1;
        Ok(())
    }
}
