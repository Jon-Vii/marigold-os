//! A reading page's images, drawn from the book's packs on the card.
//!
//! Each decoded image is a record in its orientation's pack in the book's
//! cache directory (`proto::image::pack`), and the image index
//! (`IMAGES.BIN`) says whether it is decoded and where its record starts. A
//! page whose blocks include images is drawn inside an SD session, and
//! [`draw_page`] hands its draw a drawer for them. For each image block the
//! drawer reads the block's index entry and, when the image is decoded for
//! this orientation, the record's header, and checks that the record is the
//! image the block names at the size layout reserved and lies inside the
//! pack. Its rows then stream from the card a band at a time into a
//! borrowed buffer, and from there into the frame through
//! `Framebuffer::blit`, which turns them for the frame.
//!
//! Anything else draws the block's placeholder, an outline with the alt
//! text: no slot in the token, a damaged index or index entry, an image not
//! decoded for this orientation or whose decode failed, a header that does
//! not match or a record past the pack's end, or a failed read. Reads never
//! go past the record: its header, then exactly its rows. Only a card error
//! part way through the rows leaves some drawn; they are cleared for the
//! placeholder, and the page's later images draw theirs without reading.
//! A page without image blocks opens nothing.

use crate::book_images::CardIndex;
use crate::reader_cache_files::read_exact_file;
use crate::reader_store::ReaderStore;
use display::fb::{Framebuffer, ImageRows};
use display::render::fill_rect;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use proto::cache::PageRecord;
use proto::image::index::{
    open_image_index, read_image, DecodeStatus, ImageIndexHeader, IndexError, IMAGE_INDEX_FILE,
};
use proto::image::pack::{pack_file, PackRecord, PACK_RECORD_HEADER_BYTES};
use ui::reading::{draw_image_placeholder, ReaderImage};

/// What a page's draw hands each image block to.
pub(crate) type ImageDrawer<'a> = dyn FnMut(&mut Framebuffer, &ReaderImage<'_>) + 'a;

/// Runs `draw`, the page's draw, with the drawer for `page`'s image blocks,
/// then prints the page's `image_draw` bench line when it drew any. When
/// `page` has image blocks, the drawer reads the book's index and pack for
/// the layout's orientation (the book is `book_id`'s) and streams rows
/// through `band`, a buffer idle while a page draws. `clock` gives
/// microseconds, for the bench line.
#[inline(never)]
pub(crate) fn draw_page<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    source: &ReaderStore,
    book_id: u32,
    page: PageRecord,
    band: &mut [u8],
    clock: fn() -> u64,
    draw: impl FnOnce(&mut ImageDrawer<'_>),
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let started = clock();
    let book = ReaderStore::selected_book_index(book_id)
        .and_then(|index| source.catalog_entry(index))
        .filter(|_| ui::reading::page_has_images(source, page));
    let dir = book.and_then(|book| {
        let key = proto::cache::cache_key_for(book.display_name.as_str(), book.byte_size);
        crate::book_images::book_cache_dir(root, key.as_str())
    });
    let identity = book.map_or((0, 0), |book| (book.source_hash, book.byte_size));
    let mut images = PageImages::open(dir.as_ref(), identity, source.portrait(), band, clock);
    images.stats.open_us = elapsed(clock, started);
    draw(&mut |fb: &mut Framebuffer, image: &ReaderImage<'_>| images.draw(fb, image));
    images.report();
}

/// The book's index and pack, open for one page's draw.
struct PageImages<'f, 'b, D, T, const ND: usize, const NF: usize, const NV: usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    index: Option<(File<'f, D, T, ND, NF, NV>, ImageIndexHeader)>,
    /// The pack and its length.
    pack: Option<(File<'f, D, T, ND, NF, NV>, u32)>,
    portrait: bool,
    band: &'b mut [u8],
    clock: fn() -> u64,
    /// A card read failed: the page's later images draw their placeholders
    /// without reading.
    card_failed: bool,
    stats: DrawStats,
}

/// One page's image draws, for the bench line.
#[derive(Default)]
struct DrawStats {
    images: u16,
    placeholders: u16,
    /// Row bytes read from the pack.
    bytes: u32,
    /// Opening the directory, the index and the pack.
    open_us: u32,
    /// Reading index entries, headers and rows.
    read_us: u32,
    blit_us: u32,
}

/// How far an image got.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shown {
    Drawn,
    /// Nothing drawn: the placeholder takes the rect as it is.
    Missing,
    /// Some rows drawn before a read failed.
    Partly,
}

impl<'f, 'b, D, T, const ND: usize, const NF: usize, const NV: usize>
    PageImages<'f, 'b, D, T, ND, NF, NV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Opens the index and the orientation's pack in the book's cache
    /// directory `dir`, the index only if it checks out for the book whose
    /// source identity is `identity`. What does not open leaves its images
    /// to their placeholders. Out of line, so the index check's buffers are
    /// off the stack while the page draws.
    #[inline(never)]
    fn open(
        dir: Option<&'f Directory<'_, D, T, ND, NF, NV>>,
        identity: (u32, u32),
        portrait: bool,
        band: &'b mut [u8],
        clock: fn() -> u64,
    ) -> Self {
        let key = crate::book_images::image_index_key(identity);
        let index = dir
            .and_then(|dir| dir.open_file_in_dir(IMAGE_INDEX_FILE, Mode::ReadOnly).ok())
            .and_then(|file| {
                let header = open_image_index(&mut CardIndex(&file), &key).ok()?;
                Some((file, header))
            });
        let pack = index
            .as_ref()
            .and(dir)
            .and_then(|dir| {
                dir.open_file_in_dir(pack_file(portrait), Mode::ReadOnly)
                    .ok()
            })
            .map(|file| {
                let len = file.length();
                (file, len)
            });
        Self {
            index,
            pack,
            portrait,
            band,
            clock,
            card_failed: false,
            stats: DrawStats::default(),
        }
    }

    fn draw(&mut self, fb: &mut Framebuffer, image: &ReaderImage<'_>) {
        self.stats.images = self.stats.images.saturating_add(1);
        let shown = self.draw_rows(fb, image);
        if shown == Shown::Drawn {
            return;
        }
        if shown == Shown::Partly {
            if let (Ok(x), Ok(y)) = (u16::try_from(image.x), u16::try_from(image.y)) {
                fill_rect(
                    fb,
                    display::Rect::new(x, y, image.width, image.height),
                    true,
                );
            }
        }
        draw_image_placeholder(fb, image);
        self.stats.placeholders = self.stats.placeholders.saturating_add(1);
    }

    fn draw_rows(&mut self, fb: &mut Framebuffer, image: &ReaderImage<'_>) -> Shown {
        let Self {
            index,
            pack,
            portrait,
            band,
            clock,
            card_failed,
            stats,
        } = self;
        let (Some(slot), Some((index, header)), Some((pack, pack_len)), false) =
            (image.slot, index.as_ref(), pack.as_ref(), *card_failed)
        else {
            return Shown::Missing;
        };
        let clock = *clock;
        let started = clock();
        let found = find_record(index, header, pack, *pack_len, *portrait, slot, image);
        stats.read_us = stats.read_us.saturating_add(elapsed(clock, started));
        let record = match found {
            Ok(Some(record)) => record,
            Ok(None) => return Shown::Missing,
            Err(()) => {
                *card_failed = true;
                return Shown::Missing;
            }
        };
        // The rows follow the header, which the pack was just read past.
        let stride = usize::from(record.stride);
        let rows_per_band = band.len() / stride;
        if rows_per_band == 0 {
            return Shown::Missing;
        }
        let height = usize::from(record.height);
        let mut row = 0;
        while row < height {
            let rows = rows_per_band.min(height - row);
            let bits = &mut band[..rows * stride];
            let started = clock();
            let read = read_exact_file(pack, bits);
            stats.read_us = stats.read_us.saturating_add(elapsed(clock, started));
            if read.is_err() {
                *card_failed = true;
                return if row == 0 {
                    Shown::Missing
                } else {
                    Shown::Partly
                };
            }
            let started = clock();
            fb.blit(
                i32::from(image.x),
                i32::from(image.y),
                ImageRows {
                    width: usize::from(record.width),
                    stride,
                    first_row: row,
                    bits,
                },
            );
            stats.blit_us = stats.blit_us.saturating_add(elapsed(clock, started));
            row += rows;
        }
        stats.bytes = stats.bytes.saturating_add(record.rows_len());
        Shown::Drawn
    }

    fn report(&self) {
        let stats = &self.stats;
        if stats.images == 0 {
            return;
        }
        esp_println::println!(
            "bench: image_draw images={} placeholders={} bytes={} open_us={} read_us={} blit_us={} t_ms={}",
            stats.images,
            stats.placeholders,
            stats.bytes,
            stats.open_us,
            stats.read_us,
            stats.blit_us,
            (self.clock)() / 1000,
        );
    }
}

/// The pack record for image block `image`, whose token names `slot`,
/// with the pack read just past its header: `None` when the image is not
/// decoded for this orientation, its entry is damaged, or the record is
/// not the image at the size layout reserved or does not lie inside the
/// pack (`pack_len` bytes). Err when a read fails.
fn find_record<D, T, const ND: usize, const NF: usize, const NV: usize>(
    index: &File<'_, D, T, ND, NF, NV>,
    header: &ImageIndexHeader,
    pack: &File<'_, D, T, ND, NF, NV>,
    pack_len: u32,
    portrait: bool,
    slot: u16,
    image: &ReaderImage<'_>,
) -> Result<Option<PackRecord>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let entry = match read_image(&mut CardIndex(index), header, slot) {
        Ok(Some(entry)) => entry,
        Err(IndexError::Io) => return Err(()),
        Ok(None) | Err(_) => return Ok(None),
    };
    let decode = entry.decode(portrait);
    let fits = |len: u32| {
        decode
            .pack_offset
            .checked_add(len)
            .is_some_and(|end| end <= pack_len)
    };
    if decode.status != DecodeStatus::Decoded || !fits(PACK_RECORD_HEADER_BYTES as u32) {
        return Ok(None);
    }
    let mut head = [0u8; PACK_RECORD_HEADER_BYTES];
    read_at(pack, decode.pack_offset, &mut head)?;
    Ok(PackRecord::decode(&head).filter(|record| {
        record.matches(entry.path_hash, image.width, image.height) && fits(record.record_len())
    }))
}

fn elapsed(clock: fn() -> u64, since: u64) -> u32 {
    clock().saturating_sub(since).min(u64::from(u32::MAX)) as u32
}

fn read_at<D, T, const ND: usize, const NF: usize, const NV: usize>(
    file: &File<'_, D, T, ND, NF, NV>,
    offset: u32,
    buf: &mut [u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file.seek_from_start(offset).map_err(|_| ())?;
    read_exact_file(file, buf)
}
