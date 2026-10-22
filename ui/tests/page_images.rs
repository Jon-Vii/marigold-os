//! A reading page's images drawn from the book's packs
//! (`fw::page_images`), run against the pinned FAT code on an in-memory
//! card: what draws, what draws its placeholder instead, and that a page
//! without images reads nothing.
#![allow(dead_code, unused_imports, unexpected_cfgs)]
extern crate self as esp_println;
extern crate self as hal_ext;
#[path = "../../hal-ext/src/nvm.rs"]
pub mod nvm;
pub use std::println;
#[path = "../../fw/src/book_images.rs"]
mod book_images;
#[path = "support/fat.rs"]
mod fat;
#[path = "../../fw/src/page_images.rs"]
mod page_images;
#[path = "../../fw/src/reader_cache_files.rs"]
mod reader_cache_files;
#[path = "../../fw/src/reader_layout.rs"]
mod reader_layout;
#[path = "../../fw/src/reader_store.rs"]
mod reader_store;
mod sd_session {
    pub fn remove_file_reclaiming_clusters<
        D,
        T,
        const ND: usize,
        const NF: usize,
        const NV: usize,
    >(
        d: &embedded_sdmmc::Directory<'_, D, T, ND, NF, NV>,
        name: &str,
    ) -> bool
    where
        D: embedded_sdmmc::BlockDevice,
        T: embedded_sdmmc::TimeSource,
    {
        d.delete_file_in_dir(name).is_ok()
    }
}

use display::fb::{FbFrame, Framebuffer};
use display::font::{FontStyle, TypeSettings};
use embedded_sdmmc::{Directory, Mode, VolumeIdx, VolumeManager};
use proto::epub::ZipEntry;
use proto::image::index::{
    find_image, write_image_state, BuildStep, DecodeState, DecodeStatus, ImageIndexBuilder,
    IndexIo, IMAGE_INDEX_FILE,
};
use proto::image::pack::{PackRecord, LANDSCAPE_PACK_FILE, PORTRAIT_PACK_FILE};
use proto::image::path::path_hash;
use proto::text::{TextAlign, TextRole};
use reader_store::ReaderStore;
use ui::reading::{draw_image_placeholder, ReaderImage};

const NAME: &str = "Plates.epub";
const SIZE: u32 = 4321;
const SOURCE_HASH: u32 = 0x1234_5678;
const IMAGES: [&str; 4] = ["OEBPS/a.png", "OEBPS/b.png", "OEBPS/c.png", "OEBPS/d.png"];

/// What the pack holds for an image, or what is wrong with it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pack {
    /// Decoded, with these rows.
    Good,
    NotDecoded,
    Failed,
    /// A record for another image.
    OtherImage,
    /// A record of another size.
    OtherSize,
    /// A header that is not one.
    BadMagic,
    /// The record runs past the pack's end.
    CutShort,
}

struct Plate {
    width: u16,
    height: u16,
    pack: Pack,
    /// Rows in the pack layout; padding bits are random, and must not draw.
    rows: Vec<u8>,
}

fn rows(seed: &mut u64, width: u16, height: u16) -> Vec<u8> {
    let len = usize::from(width.div_ceil(8)) * usize::from(height);
    (0..len)
        .map(|_| {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed as u8
        })
        .collect()
}

/// `IMAGES.BIN` through an open file, for setting up decode state.
struct TestIndex<'f, 'v, D, T>(&'f embedded_sdmmc::File<'v, D, T, 8, 8, 1>)
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource;

impl<D, T> IndexIo for TestIndex<'_, '_, D, T>
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
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

/// Writes the book's cache directory with its index and both packs, each
/// image in `plates` stored as it says, and returns each image's slot.
fn write_book<D: embedded_sdmmc::BlockDevice>(
    root: &Directory<'_, D, fat::Clock, 8, 8, 1>,
    plates: &[Plate],
    portrait: bool,
) -> Vec<u16> {
    let key = proto::cache::cache_key_for(NAME, SIZE);
    root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
    let xteink = root.open_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
    xteink.make_dir_in_dir(proto::cache::CACHE_V2_DIR).unwrap();
    let cache = xteink.open_dir(proto::cache::CACHE_V2_DIR).unwrap();
    cache.make_dir_in_dir(key.as_str()).unwrap();
    let dir = cache.open_dir(key.as_str()).unwrap();

    let index = dir
        .open_file_in_dir(IMAGE_INDEX_FILE, Mode::ReadWriteCreateOrTruncate)
        .unwrap();
    let mut scratch = vec![0xa5u8; 8192];
    let mut builder = ImageIndexBuilder::new(
        &mut scratch,
        book_images::image_index_key((SOURCE_HASH, SIZE)),
    )
    .unwrap();
    for (at, name) in IMAGES.iter().enumerate() {
        builder.offer(&ZipEntry {
            name,
            compression_method: 0,
            compressed_size: 100,
            uncompressed_size: 100,
            local_header_offset: 1000 * at as u32,
        });
    }
    let BuildStep::Done(header) = builder.end_pass(&mut TestIndex(&index)).unwrap() else {
        panic!("one pass holds four images");
    };

    // Both packs get every record, so a draw that read the other
    // orientation's pack would still find its image and fail the test.
    let mut slots = Vec::new();
    for pack_name in [LANDSCAPE_PACK_FILE, PORTRAIT_PACK_FILE] {
        let pack = dir
            .open_file_in_dir(pack_name, Mode::ReadWriteCreateOrTruncate)
            .unwrap();
        // Something ahead of the first record, as an orphan tail would be.
        pack.write(&[0x5a; 37]).unwrap();
        let in_this_pack = (pack_name == PORTRAIT_PACK_FILE) == portrait;
        slots.clear();
        for (plate, name) in plates.iter().zip(IMAGES) {
            let hash = path_hash(name);
            let (slot, mut entry) = find_image(&mut TestIndex(&index), &header, hash)
                .unwrap()
                .unwrap();
            slots.push(slot);
            let offset = pack.length();
            let (record_hash, width) = match plate.pack {
                Pack::OtherImage => (path_hash(IMAGES[3]), plate.width),
                Pack::OtherSize => (hash, plate.width + 1),
                _ => (hash, plate.width),
            };
            let record = PackRecord::new(record_hash, width, plate.height).unwrap();
            let mut head = record.encode();
            if plate.pack == Pack::BadMagic {
                head[0] = b'Y';
            }
            pack.write(&head).unwrap();
            let mut body = plate.rows.clone();
            body.resize(record.rows_len() as usize, 0);
            if plate.pack == Pack::CutShort {
                body.truncate(body.len() - 1);
            }
            // Rows the wrong way up in the other orientation's pack.
            if !in_this_pack {
                body.iter_mut().for_each(|byte| *byte = !*byte);
            }
            pack.write(&body).unwrap();
            let status = match plate.pack {
                Pack::NotDecoded => DecodeStatus::NotDecoded,
                Pack::Failed => DecodeStatus::Failed,
                _ => DecodeStatus::Decoded,
            };
            entry.set_decode(
                pack_name == PORTRAIT_PACK_FILE,
                DecodeState {
                    status,
                    pack_offset: offset,
                },
            );
            write_image_state(&mut TestIndex(&index), &header, slot, &entry).unwrap();
        }
    }
    slots
}

/// A store holding one page: a line, then each plate as an image block
/// naming `slots`, each under a line.
fn store(plates: &[Plate], slots: &[u16], portrait: bool) -> ReaderStore {
    let mut store = ReaderStore::new();
    store.set_active_entry(0, NAME, "PLATES.EPU", true, SIZE, SOURCE_HASH, None);
    store.set_layout(TypeSettings::DEFAULT, portrait);
    store.set_book_has_images(true);
    let (regular, body, left) = (FontStyle::Regular, TextRole::Body, TextAlign::Left);
    assert!(store.push_line_block("Plates gjpqy", regular, body, left, true, 0));
    for (plate, &slot) in plates.iter().zip(slots) {
        assert!(store.push_image_block(plate.height, 0, |out| {
            proto::image::token::encode_image_token(slot, plate.width, Some("A plate"), out)
                .map(str::len)
        }));
        assert!(store.push_line_block("Under it", regular, body, left, true, 0));
    }
    reader_layout::rebuild_page_index(&mut store);
    assert_eq!(store.page_count, 1, "one page");
    store
}

fn book_id() -> u32 {
    app_core::ReaderSource::sd(0).book_id()
}

/// Draws the store's page through `page_images::draw_page` in `frame`.
fn draw<D: embedded_sdmmc::BlockDevice>(
    root: &Directory<'_, D, fat::Clock, 8, 8, 1>,
    store: &ReaderStore,
    frame: FbFrame,
) -> Framebuffer {
    let mut fb = Framebuffer::new();
    fb.set_frame(frame);
    // Dirty, as the borrowed band is on the device.
    let mut band = vec![0xa5u8; display::BAND_BYTES];
    let page = store.pages[0];
    page_images::draw_page(
        root,
        store,
        book_id(),
        page,
        &mut band,
        || 0,
        |images| ui::reading::draw_reading_page_body(&mut fb, store, page, images),
    );
    fb
}

/// What the page must look like: each `Good` plate's rows set pixel by
/// pixel, every other plate its placeholder.
fn expected(store: &ReaderStore, plates: &[Plate], frame: FbFrame) -> Framebuffer {
    let mut fb = Framebuffer::new();
    fb.set_frame(frame);
    let mut next = 0;
    ui::reading::draw_reading_page_body(&mut fb, store, store.pages[0], |fb, image| {
        let plate = &plates[next];
        next += 1;
        if plate.pack != Pack::Good {
            draw_image_placeholder(fb, image);
            return;
        }
        let stride = usize::from(plate.width.div_ceil(8));
        for row in 0..usize::from(plate.height) {
            for column in 0..usize::from(plate.width) {
                let white = plate.rows[row * stride + column / 8] & (0x80 >> (column % 8)) != 0;
                fb.set_pixel(image.x as usize + column, image.y as usize + row, white);
            }
        }
    });
    assert_eq!(next, plates.len(), "every plate is on the page");
    fb
}

fn plates(kinds: &[(u16, u16, Pack)]) -> Vec<Plate> {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    kinds
        .iter()
        .map(|&(width, height, pack)| Plate {
            width,
            height,
            pack,
            rows: rows(&mut seed, width, height),
        })
        .collect()
}

#[test]
fn decoded_images_draw_their_rows_in_every_frame() {
    for frame in [
        FbFrame::Landscape,
        FbFrame::LandscapeFlipped,
        FbFrame::Portrait,
    ] {
        let portrait = frame == FbFrame::Portrait;
        let disk = fat::Disk::new();
        let manager: VolumeManager<_, _, 8, 8, 1> =
            VolumeManager::new_with_limits(&disk, fat::Clock, 0);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        // Odd widths, and one tall enough to take several bands.
        let plates = plates(&[(37, 21, Pack::Good), (301, 200, Pack::Good)]);
        let slots = write_book(&root, &plates, portrait);
        let store = store(&plates, &slots, portrait);
        let fb = draw(&root, &store, frame);
        assert!(
            fb.bytes()[..] == expected(&store, &plates, frame).bytes()[..],
            "{frame:?}"
        );
    }
}

#[test]
fn anything_amiss_draws_the_placeholder_and_the_rest_still_draw() {
    for amiss in [
        Pack::NotDecoded,
        Pack::Failed,
        Pack::OtherImage,
        Pack::OtherSize,
        Pack::BadMagic,
        Pack::CutShort,
    ] {
        let disk = fat::Disk::new();
        let manager: VolumeManager<_, _, 8, 8, 1> =
            VolumeManager::new_with_limits(&disk, fat::Clock, 0);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        // A record cut short is one the pack ends inside: the last.
        let plates = if amiss == Pack::CutShort {
            plates(&[(40, 30, Pack::Good), (33, 20, Pack::Good), (90, 41, amiss)])
        } else {
            plates(&[(40, 30, Pack::Good), (90, 41, amiss), (33, 20, Pack::Good)])
        };
        let slots = write_book(&root, &plates, false);
        let store = store(&plates, &slots, false);
        let fb = draw(&root, &store, FbFrame::Landscape);
        assert!(
            fb.bytes()[..] == expected(&store, &plates, FbFrame::Landscape).bytes()[..],
            "{amiss:?}"
        );
    }
}

#[test]
fn a_token_without_a_slot_or_a_missing_pack_draws_placeholders() {
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let mut plates = plates(&[(40, 30, Pack::Good), (50, 20, Pack::Good)]);
    let slots = write_book(&root, &plates, false);
    // A slot past the index's entries.
    let store_past = store(&plates, &[slots[0], 4000], false);
    plates[1].pack = Pack::NotDecoded;
    let fb = draw(&root, &store_past, FbFrame::Landscape);
    assert!(fb.bytes()[..] == expected(&store_past, &plates, FbFrame::Landscape).bytes()[..]);

    // No pack for the orientation: every image is a placeholder.
    let key = proto::cache::cache_key_for(NAME, SIZE);
    let dir = book_images::book_cache_dir(&root, key.as_str()).unwrap();
    dir.delete_file_in_dir(LANDSCAPE_PACK_FILE).unwrap();
    drop(dir);
    plates[0].pack = Pack::NotDecoded;
    let store = store(&plates, &slots, false);
    let fb = draw(&root, &store, FbFrame::Landscape);
    assert!(fb.bytes()[..] == expected(&store, &plates, FbFrame::Landscape).bytes()[..]);
}

/// The in-memory card, failing one block read: the `fail_at`th from now.
struct Flaky<'a> {
    disk: &'a fat::Disk,
    attempts: std::cell::Cell<usize>,
    fail_at: std::cell::Cell<Option<usize>>,
}

impl embedded_sdmmc::BlockDevice for &Flaky<'_> {
    type Error = &'static str;
    fn read(
        &self,
        blocks: &mut [embedded_sdmmc::Block],
        first: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        let attempt = self.attempts.get();
        self.attempts.set(attempt + 1);
        if self.fail_at.get() == Some(attempt) {
            return Err("injected read failure");
        }
        (&self.disk).read(blocks, first)
    }
    fn write(
        &self,
        blocks: &[embedded_sdmmc::Block],
        first: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        (&self.disk).write(blocks, first)
    }
    fn num_blocks(&self) -> Result<embedded_sdmmc::BlockCount, Self::Error> {
        (&self.disk).num_blocks()
    }
}

#[test]
fn a_failed_read_clears_what_it_drew_and_stops_reading() {
    let disk = fat::Disk::new();
    let flaky = Flaky {
        disk: &disk,
        attempts: std::cell::Cell::new(0),
        fail_at: std::cell::Cell::new(None),
    };
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&flaky, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    // A first image whose rows take dozens of card blocks and several bands,
    // and a small one after it.
    let mut plates = plates(&[(700, 300, Pack::Good), (40, 30, Pack::Good)]);
    let slots = write_book(&root, &plates, false);
    let store = store(&plates, &slots, false);

    // How many block reads a whole draw takes.
    let start = flaky.attempts.get();
    let fb = draw(&root, &store, FbFrame::Landscape);
    let whole = flaky.attempts.get() - start;
    assert!(fb.bytes()[..] == expected(&store, &plates, FbFrame::Landscape).bytes()[..]);
    // One read fails part way through the first image's rows: what it drew
    // is cleared for its placeholder, and nothing more is read, so the
    // second image, which a retry would now read, is a placeholder too.
    for fail in [whole / 2, whole - 8] {
        let start = flaky.attempts.get();
        flaky.fail_at.set(Some(start + fail));
        let fb = draw(&root, &store, FbFrame::Landscape);
        flaky.fail_at.set(None);
        assert_eq!(
            flaky.attempts.get() - start,
            fail + 1,
            "no read after the failure"
        );
        plates[0].pack = Pack::NotDecoded;
        plates[1].pack = Pack::NotDecoded;
        assert!(fb.bytes()[..] == expected(&store, &plates, FbFrame::Landscape).bytes()[..]);
        plates[0].pack = Pack::Good;
        plates[1].pack = Pack::Good;
    }
}

#[test]
fn a_page_without_images_reads_nothing() {
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let plates = plates(&[(40, 30, Pack::Good)]);
    write_book(&root, &plates, false);
    let store = store(&[], &[], false);
    let before = disk.reads.get();
    let mut calls = 0;
    let mut band = vec![0u8; display::BAND_BYTES];
    page_images::draw_page(
        &root,
        &store,
        book_id(),
        store.pages[0],
        &mut band,
        || 0,
        |_| calls += 1,
    );
    assert_eq!(calls, 1);
    assert_eq!(disk.reads.get(), before, "no card reads");
}

#[test]
fn a_record_cut_short_inside_the_pack_reads_on_to_its_claimed_length_only() {
    // A record cut short that is not the pack's last: the bound a draw
    // checks is the pack's end, which the records after it cover, so it
    // draws, its rows running on into the next record's first byte (an
    // index only ever points at a whole record; this is damage), and the
    // read stops at the record's own length, so the image after it draws
    // as it should.
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let kinds = [
        (40, 30, Pack::Good),
        (90, 41, Pack::CutShort),
        (33, 20, Pack::Good),
    ];
    let plates = plates(&kinds);
    let slots = write_book(&root, &plates, false);
    let store = store(&plates, &slots, false);
    let fb = draw(&root, &store, FbFrame::Landscape);

    let mut drawn = self::plates(&kinds.map(|(width, height, _)| (width, height, Pack::Good)));
    assert_eq!(drawn[1].rows, plates[1].rows);
    // The magic's first byte, which the next record starts with.
    *drawn[1].rows.last_mut().unwrap() = proto::image::pack::PACK_RECORD_MAGIC[0];
    assert!(fb.bytes()[..] == expected(&store, &drawn, FbFrame::Landscape).bytes()[..]);
}
