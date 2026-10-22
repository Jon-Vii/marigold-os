//! Decoding a book's images into its packs (`fw::image_decode`), run
//! against the pinned FAT code on an in-memory card and a ZIP in RAM.
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
#[path = "../../fw/src/image_decode.rs"]
mod image_decode;
#[path = "../../fw/src/page_images.rs"]
mod page_images;
#[path = "../../fw/src/reader_cache_files.rs"]
mod reader_cache_files;
#[path = "../../fw/src/reader_layout.rs"]
mod reader_layout;
#[path = "../../fw/src/reader_store.rs"]
mod reader_store;
#[path = "support/zip.rs"]
mod zip;
mod sd_session {
    use embedded_sdmmc::{Directory, Mode, TimeSource};
    pub fn remove_file_reclaiming_clusters<
        D,
        T,
        const ND: usize,
        const NF: usize,
        const NV: usize,
    >(
        d: &Directory<'_, D, T, ND, NF, NV>,
        name: &str,
    ) -> bool
    where
        D: embedded_sdmmc::BlockDevice,
        T: TimeSource,
    {
        let Ok(f) = d.open_file_in_dir(name, Mode::ReadWriteTruncate) else {
            return false;
        };
        if f.close().is_err() {
            return false;
        }
        d.delete_file_in_dir(name).is_ok()
    }
}

use book_images::{BookImages, ImagePlacement};
use display::fb::{FbFrame, Framebuffer};
use display::font::TypeSettings;
use embedded_sdmmc::{Directory, Mode, VolumeIdx, VolumeManager};
use image_decode::{
    pending_size, reading_distance, window_pages, DecodeOutcome, IdleImageStep, IdleImageWork,
    ImageScratch, ImageSession, Nearest, PageImage, PageImages, SPOOL_FILE, SPOOL_MAX_BYTES,
};
use proto::epub::{ZipInflateScratch, ZipStream};
use proto::image::dither::Ditherer;
use proto::image::index::{DecodeStatus, ImageEntry, ImagePlace, ProbeStatus};
use proto::image::pack::{pack_file, PackRecord, PACK_RECORD_HEADER_BYTES};
use proto::image::png::PngDecoder;
use reader_store::ReaderStore;
use std::cell::{Cell, RefCell};
use ui::reading::{fit_image, PageBox};
use zip::{zip_raw, Archive};

const KEY: &str = "E0000001";
const ID: (u32, u32) = (1234, 5678);
const CHAPTER: &str = "OEBPS/text/ch01.xhtml";
/// 213 × 300, gray.
const SMALL_PNG: &[u8] = include_bytes!("../../fixtures/images/orientation-small.png");
/// 640 × 900, gray.
const LARGE_PNG: &[u8] = include_bytes!("../../fixtures/images/orientation-test.png");

fn now() -> u64 {
    0
}

/// A deflate stream of stored blocks holding `plain`: enough for the
/// inflater to stream it, of any length.
fn deflate_stored(plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pieces = plain.chunks(65_535).peekable();
    if pieces.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(piece) = pieces.next() {
        out.push(u8::from(pieces.peek().is_none()));
        out.extend_from_slice(&(piece.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(piece.len() as u16)).to_le_bytes());
        out.extend_from_slice(piece);
    }
    out
}

/// A PNG of `width × height` whose scanlines (filter byte included) are
/// `raw`, compressed as stored blocks; chunk CRCs are left zero, which the
/// decoder does not check.
fn png(width: u32, height: u32, depth: u8, color: u8, palette: &[u8], raw: &[u8]) -> Vec<u8> {
    let chunk = |out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0; 4]);
    };
    let mut out = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[depth, color, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    if !palette.is_empty() {
        chunk(&mut out, b"PLTE", palette);
    }
    let mut zlib = vec![0x78, 0x01];
    zlib.extend(deflate_stored(raw));
    zlib.extend_from_slice(&[0; 4]);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A 4-bit palette image, as Rackham's plates in *Alice* are: sixteen grays
/// in a diagonal ramp.
fn palette_png(width: u32, height: u32) -> Vec<u8> {
    let palette: Vec<u8> = (0..16u8).flat_map(|i| [i * 17, i * 17, i * 17]).collect();
    let stride = (width as usize).div_ceil(2);
    let mut raw = Vec::new();
    for y in 0..height as usize {
        raw.push(0);
        for x in 0..stride {
            let level = |px: usize| ((px + y) / 7 % 16) as u8;
            raw.push(level(2 * x) << 4 | level(2 * x + 1));
        }
    }
    png(width, height, 4, 3, &palette, &raw)
}

/// What the pack should hold for `file` at `width × height`: the decoder
/// and the ditherer run in RAM.
fn reference_rows(file: &[u8], width: u16, height: u16) -> Vec<u8> {
    let mut scratch = vec![0x5au8; 64 * 1024];
    let (cells, rest) = scratch.split_at_mut(Ditherer::scratch_bytes(width.into()));
    let mut ditherer = Ditherer::new(cells, width.into()).unwrap();
    let mut decoder = PngDecoder::new(rest, width.into(), height.into()).unwrap();
    let mut packed = vec![0u8; Ditherer::packed_len(width.into())];
    let mut rows = Vec::new();
    let mut sink = |_: u32, luma: &[u8]| {
        ditherer.row(luma, &mut packed).unwrap();
        rows.extend_from_slice(&packed);
        Ok(())
    };
    let mut inflate = ZipInflateScratch::new();
    decoder.feed(inflate.inflater(), file, &mut sink).unwrap();
    decoder.finish(inflate.inflater(), &mut sink).unwrap();
    rows
}

struct Scratch {
    walk: Vec<u8>,
    window: Vec<u8>,
    compressed: Vec<u8>,
    name: Vec<u8>,
    list: Vec<u8>,
    inflate: ZipInflateScratch,
}

/// Dirty, as borrowed scratch is on the device.
fn scratch() -> Scratch {
    Scratch {
        walk: vec![0xa5; 8192],
        window: vec![0xa5; 24 * 1024],
        compressed: vec![0xa5; 8192],
        name: vec![0xa5; proto::epub::MAX_ENTRY_NAME_BYTES],
        list: vec![0xa5; 4096],
        inflate: ZipInflateScratch::new(),
    }
}

fn parts(scratch: &mut Scratch) -> ImageScratch<'_> {
    let (input, stage) = scratch.compressed.split_at_mut(4096);
    ImageScratch {
        input,
        stage,
        window: &mut scratch.window,
        name: &mut scratch.name,
        inflate: &mut scratch.inflate,
    }
}

/// A book with `images` (name, method, stored bytes, declared size) under
/// `OEBPS/images/`.
fn book(images: &[(String, u16, Vec<u8>, u32)]) -> (Vec<u8>, Vec<(u32, u32)>) {
    let chapter = b"<html><body><p>Text</p></body></html>".to_vec();
    let mut files = vec![(
        CHAPTER.to_string(),
        0u16,
        chapter.clone(),
        chapter.len() as u32,
    )];
    for (name, method, data, plain) in images {
        files.push((
            format!("OEBPS/images/{name}"),
            *method,
            data.clone(),
            *plain,
        ));
    }
    let files: Vec<(&str, u16, Vec<u8>, u32)> = files
        .iter()
        .map(|(name, method, data, plain)| (name.as_str(), *method, data.clone(), *plain))
        .collect();
    zip_raw(&files)
}

fn stored(name: &str, file: &[u8]) -> (String, u16, Vec<u8>, u32) {
    (name.to_string(), 0, file.to_vec(), file.len() as u32)
}

fn deflated(name: &str, file: &[u8]) -> (String, u16, Vec<u8>, u32) {
    (name.to_string(), 8, deflate_stored(file), file.len() as u32)
}

fn place_at<D, T, const ND: usize, const NF: usize, const NV: usize>(
    images: &BookImages<'_, D, T, ND, NF, NV>,
    name: &str,
    portrait: bool,
    first_pass: bool,
    at: ImagePlace,
) -> ImagePlacement
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    let mut path = [0xa5u8; proto::epub::MAX_ENTRY_NAME_BYTES];
    images
        .place(
            CHAPTER,
            &format!("../images/{name}"),
            &mut path,
            portrait,
            first_pass,
            at,
        )
        .expect("the card answers")
}

/// Builds the book's index and lays its images out as a chapter would, in
/// the order given, each at its place: a first pass, the probes, and the
/// second pass. Returns each image's slot.
fn lay_out<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    bytes: &[u8],
    images: &[(&str, ImagePlace)],
    scratch: &mut Scratch,
) -> Vec<u16>
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    let mut tail = [0u8; 4096];
    let mut zip = ZipStream::new(
        Archive {
            bytes,
            reads: &reads,
            fails: &fails,
        },
        &mut tail,
    )
    .unwrap();
    let index = book_images::open_book_images(
        dir,
        ID,
        &mut zip,
        &mut scratch.walk,
        &mut scratch.window,
        &mut false,
    )
    .expect("index built");
    for &(name, at) in images {
        place_at(&index, name, false, true, at);
    }
    index
        .probe_marked(
            &mut zip,
            0..=u16::MAX - 1,
            &mut scratch.compressed,
            &mut scratch.window,
            &mut scratch.inflate,
            || false,
        )
        .expect("probed");
    images
        .iter()
        .map(
            |&(name, at)| match place_at(&index, name, false, false, at) {
                ImagePlacement::Shown { slot, .. } => slot,
                other => panic!("{name}: {other:?}"),
            },
        )
        .collect()
}

fn entry<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    slot: u16,
) -> ImageEntry
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    book_images::open_existing_book_images(dir, ID)
        .expect("index")
        .read(slot)
        .unwrap()
        .unwrap()
}

/// Decodes one page image in a session of its own, as a request does, and
/// closes it: a session keeps the index and the pack open, and the card
/// opens a file once at a time.
#[allow(clippy::too_many_arguments)]
fn decode_one<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    bytes: &[u8],
    reads: &RefCell<Vec<(u32, usize)>>,
    fails: &Cell<Option<(u32, u32)>>,
    image: PageImage,
    portrait: bool,
    scratch: &mut Scratch,
    gives_way: &dyn Fn() -> bool,
) -> Option<DecodeOutcome>
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    let images = book_images::open_existing_book_images(dir, ID).expect("index");
    let archive = || {
        Some(Archive {
            bytes,
            reads,
            fails,
        })
    };
    let mut session = ImageSession::new(dir, images, archive, portrait, now);
    session.decode_page_image(image, &mut parts(scratch), gives_way)
}

/// A rest pass in a session of its own: whether it left the book with
/// nothing to decode, and how many it decoded.
fn rest_pass<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    bytes: &[u8],
    reader: ImagePlace,
    list: &mut [u8],
    scratch: &mut Scratch,
    out_of_time: &dyn Fn() -> bool,
) -> (bool, u16)
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    let images = book_images::open_existing_book_images(dir, ID).expect("index");
    let archive = || {
        Some(Archive {
            bytes,
            reads: &reads,
            fails: &fails,
        })
    };
    let mut session = ImageSession::new(dir, images, archive, false, now);
    let clean = session.decode_nearest(reader, list, &mut parts(scratch), &|| false, out_of_time);
    (clean, session.stats.decoded)
}

/// The record at `offset` in an orientation's pack: its header and rows.
fn pack_record<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    portrait: bool,
    offset: u32,
) -> (PackRecord, Vec<u8>)
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    let file = dir
        .open_file_in_dir(pack_file(portrait), Mode::ReadOnly)
        .unwrap();
    file.seek_from_start(offset).unwrap();
    let mut header = [0u8; PACK_RECORD_HEADER_BYTES];
    read_all(&file, &mut header);
    let record = PackRecord::decode(&header).expect("a record");
    let mut rows = vec![0u8; record.rows_len() as usize];
    read_all(&file, &mut rows);
    (record, rows)
}

fn pack_len<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    portrait: bool,
) -> u32
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    dir.open_file_in_dir(pack_file(portrait), Mode::ReadOnly)
        .map_or(0, |file| file.length())
}

fn read_all<D, T, const ND: usize, const NF: usize, const NV: usize>(
    file: &embedded_sdmmc::File<'_, D, T, ND, NF, NV>,
    mut out: &mut [u8],
) where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    while !out.is_empty() {
        let read = file.read(out).unwrap();
        assert!(read > 0, "short pack");
        out = &mut out[read..];
    }
}

fn has_file<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    name: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    dir.find_directory_entry(name).is_ok()
}

const AT: ImagePlace = ImagePlace { spine: 0, order: 0 };

#[test]
fn stored_and_deflated_images_decode_into_each_orientations_pack() {
    let alice = palette_png(377, 520);
    let (bytes, _) = book(&[
        stored("small.png", SMALL_PNG),
        deflated("large.png", LARGE_PNG),
        deflated("plate.png", &alice),
    ]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let names = ["small.png", "large.png", "plate.png"];
    let files: [&[u8]; 3] = [SMALL_PNG, LARGE_PNG, &alice];
    let slots = lay_out(&dir, &bytes, &names.map(|name| (name, AT)), &mut scratch);

    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    for portrait in [false, true] {
        let mut offsets = Vec::new();
        for (index, &slot) in slots.iter().enumerate() {
            let before = entry(&dir, slot);
            let size = pending_size(&before, portrait).expect("still to decode");
            assert_eq!(
                size,
                fit_image(
                    PageBox::for_portrait(portrait),
                    before.width.into(),
                    before.height.into()
                )
            );
            let page = PageImage {
                slot,
                drawn: Some(size),
            };
            let decode = |scratch: &mut Scratch| {
                decode_one(
                    &dir,
                    &bytes,
                    &reads,
                    &fails,
                    page,
                    portrait,
                    scratch,
                    &|| false,
                )
            };
            assert_eq!(
                decode(&mut scratch),
                Some(DecodeOutcome::Decoded),
                "{} portrait={portrait}",
                names[index]
            );
            // Once decoded here, nothing more to do.
            assert_eq!(decode(&mut scratch), None);
            let after = entry(&dir, slot);
            let state = after.decode(portrait);
            assert_eq!(state.status, DecodeStatus::Decoded);
            assert!(!after.wanted);
            // The other orientation's decode is untouched.
            assert_eq!(after.decode(!portrait), before.decode(!portrait));
            let (record, rows) = pack_record(&dir, portrait, state.pack_offset);
            assert!(record.matches(after.path_hash, size.0, size.1));
            assert_eq!(
                rows,
                reference_rows(files[index], size.0, size.1),
                "{} portrait={portrait}",
                names[index]
            );
            offsets.push(state.pack_offset);
            assert!(!has_file(&dir, SPOOL_FILE), "the spool is deleted");
        }
        assert_eq!(offsets[0], 0, "records append");
        assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
    }
    assert!(pack_len(&dir, false) > 0 && pack_len(&dir, true) > 0);
}

#[test]
fn a_press_cancels_a_decode_and_leaves_only_an_orphan_tail() {
    let (bytes, _) = book(&[
        stored("large.png", LARGE_PNG),
        deflated("deflated.png", LARGE_PNG),
    ]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let slots = lay_out(
        &dir,
        &bytes,
        &[("large.png", AT), ("deflated.png", AT)],
        &mut scratch,
    );
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    let page = |slot| PageImage { slot, drawn: None };

    // A press part way through: rows went out, the index heard nothing.
    let asked = Cell::new(0);
    let late_press = || {
        asked.set(asked.get() + 1);
        asked.get() > 300
    };
    let before = entry(&dir, slots[0]);
    let decode = |slot, scratch: &mut Scratch, gives_way: &dyn Fn() -> bool| {
        decode_one(
            &dir,
            &bytes,
            &reads,
            &fails,
            page(slot),
            false,
            scratch,
            gives_way,
        )
    };
    assert_eq!(
        decode(slots[0], &mut scratch, &late_press),
        Some(DecodeOutcome::Cancelled)
    );
    assert_eq!(entry(&dir, slots[0]), before, "no index update");
    let orphan = pack_len(&dir, false);
    assert!(orphan > 0, "an orphan tail");

    // A press while a deflated image spools: nothing decoded, no spool.
    assert_eq!(
        decode(slots[1], &mut scratch, &|| true),
        Some(DecodeOutcome::Cancelled)
    );
    assert!(!has_file(&dir, SPOOL_FILE));
    assert_eq!(
        entry(&dir, slots[1]).decode(false).status,
        DecodeStatus::NotDecoded
    );

    // Tried again, each decodes whole after the orphan, and nothing points
    // at the orphan.
    for &slot in &slots {
        assert_eq!(
            decode(slot, &mut scratch, &|| false),
            Some(DecodeOutcome::Decoded)
        );
    }
    let first = entry(&dir, slots[0]).decode(false).pack_offset;
    assert!(first >= orphan, "{first} after the orphan's {orphan}");
    let (record, rows) = pack_record(&dir, false, first);
    assert!(record.matches(before.path_hash, record.width, record.height));
    assert_eq!(rows, reference_rows(LARGE_PNG, record.width, record.height));
}

#[test]
fn card_faults_are_retried_and_damaged_images_fail_for_their_orientation() {
    // A file cut short after its header, an image wider than the window
    // holds a row of, a deflated one over the spool's cap, and one whose
    // stream holds more than its entry says.
    let cut = SMALL_PNG[..400].to_vec();
    let wide_raw: Vec<u8> = (0..2)
        .flat_map(|_| {
            let mut row = vec![0u8];
            row.extend(std::iter::repeat_n(0x80, 8000 * 8));
            row
        })
        .collect();
    let wide = png(8000, 2, 16, 6, &[], &wide_raw);
    let (bytes, spans) = book(&[
        stored("good.png", SMALL_PNG),
        stored("cut.png", &cut),
        stored("wide.png", &wide),
        (
            "huge.png".to_string(),
            8,
            deflate_stored(SMALL_PNG),
            SPOOL_MAX_BYTES + 1,
        ),
        ("liar.png".to_string(), 8, deflate_stored(SMALL_PNG), 100),
    ]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let names = ["good.png", "cut.png", "wide.png", "huge.png", "liar.png"];
    let slots = lay_out(&dir, &bytes, &names.map(|name| (name, AT)), &mut scratch);
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    let decode = |slot, scratch: &mut Scratch| {
        let page = PageImage { slot, drawn: None };
        decode_one(&dir, &bytes, &reads, &fails, page, false, scratch, &|| {
            false
        })
    };

    // A failed read of the book records nothing.
    let good = spans[1];
    fails.set(Some((good.0, good.1)));
    let before = entry(&dir, slots[0]);
    assert_eq!(decode(slots[0], &mut scratch), Some(DecodeOutcome::Retry));
    assert_eq!(entry(&dir, slots[0]), before);
    fails.set(None);
    // Nor does a card that fails its writes.
    disk.fail_after.set(Some(0));
    assert_eq!(decode(slots[0], &mut scratch), Some(DecodeOutcome::Retry));
    disk.fail_after.set(None);
    assert_eq!(entry(&dir, slots[0]), before);
    // It decodes once the card behaves.
    assert_eq!(decode(slots[0], &mut scratch), Some(DecodeOutcome::Decoded));

    // Damage is marked failed for this orientation only, and never tried
    // again here.
    for (index, &slot) in slots.iter().enumerate().skip(1) {
        assert_eq!(
            decode(slot, &mut scratch),
            Some(DecodeOutcome::Failed),
            "{}",
            names[index]
        );
        let after = entry(&dir, slot);
        assert_eq!(
            after.decode(false).status,
            DecodeStatus::Failed,
            "{}",
            names[index]
        );
        assert_eq!(after.decode(true).status, DecodeStatus::NotDecoded);
        assert_eq!(decode(slot, &mut scratch), None);
    }
    assert!(!has_file(&dir, SPOOL_FILE));
}

#[test]
fn a_page_drawing_another_size_than_the_index_fits_decodes_nothing() {
    let (bytes, _) = book(&[stored("small.png", SMALL_PNG)]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let slots = lay_out(&dir, &bytes, &[("small.png", AT)], &mut scratch);
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    let wrong = PageImage {
        slot: slots[0],
        drawn: Some((212, 300)),
    };
    assert_eq!(
        decode_one(
            &dir,
            &bytes,
            &reads,
            &fails,
            wrong,
            false,
            &mut scratch,
            &|| false
        ),
        None
    );
    assert!(reads.borrow().is_empty(), "the book is never read");
    assert_eq!(pack_len(&dir, false), 0);
}

#[test]
fn the_rest_of_the_book_decodes_in_reading_order_from_the_reader() {
    let places = [
        ("a.png", ImagePlace { spine: 2, order: 0 }),
        ("b.png", ImagePlace { spine: 0, order: 3 }),
        ("c.png", ImagePlace { spine: 5, order: 1 }),
        ("d.png", ImagePlace { spine: 2, order: 7 }),
        ("e.png", ImagePlace { spine: 3, order: 0 }),
    ];
    let (bytes, _) = book(&places.map(|(name, _)| stored(name, SMALL_PNG)));
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let slots = lay_out(&dir, &bytes, &places, &mut scratch);
    for (index, &slot) in slots.iter().enumerate() {
        assert_eq!(entry(&dir, slot).seen_at, Some(places[index].1));
    }
    let reader = ImagePlace { spine: 2, order: 5 };

    // Out of time after the first: the nearest went first.
    let decoded = Cell::new(false);
    let mut list = vec![0xa5u8; 16];
    assert_eq!(
        rest_pass(&dir, &bytes, reader, &mut list, &mut scratch, &|| decoded
            .replace(true)),
        (false, 1)
    );
    assert_eq!(
        entry(&dir, slots[3]).decode(false).status,
        DecodeStatus::Decoded
    );

    // The rest, two to a read of the index: forward from the reader to
    // the book's end, then from its start, until a read finds none left.
    assert_eq!(
        rest_pass(&dir, &bytes, reader, &mut list, &mut scratch, &|| false),
        (true, 4)
    );
    let mut order: Vec<(u32, &str)> = slots
        .iter()
        .enumerate()
        .map(|(index, &slot)| {
            let state = entry(&dir, slot).decode(false);
            assert_eq!(state.status, DecodeStatus::Decoded);
            (state.pack_offset, places[index].0)
        })
        .collect();
    order.sort();
    let order: Vec<&str> = order.into_iter().map(|(_, name)| name).collect();
    assert_eq!(order, ["d.png", "e.png", "c.png", "b.png", "a.png"]);
    // Nothing left: the pass finds nothing to do.
    assert_eq!(
        rest_pass(&dir, &bytes, reader, &mut list, &mut scratch, &|| false),
        (true, 0)
    );
}

#[test]
fn a_relayout_finds_each_image_where_it_was_and_writes_nothing() {
    let (bytes, _) = book(&[stored("small.png", SMALL_PNG)]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let at = ImagePlace { spine: 4, order: 9 };
    let slots = lay_out(&dir, &bytes, &[("small.png", at)], &mut scratch);
    let index = book_images::open_existing_book_images(&dir, ID).unwrap();
    let writes = disk.writes.get();
    // Met again where it was, or later in the book: nothing to write.
    for later in [
        at,
        ImagePlace {
            spine: 4,
            order: 10,
        },
        ImagePlace { spine: 6, order: 0 },
    ] {
        assert!(matches!(
            place_at(&index, "small.png", false, true, later),
            ImagePlacement::Shown { .. }
        ));
    }
    assert_eq!(disk.writes.get(), writes);
    // Met earlier: the earlier place is kept, in one write.
    let earlier = ImagePlace {
        spine: 1,
        order: 30,
    };
    place_at(&index, "small.png", false, true, earlier);
    assert_eq!(
        index.read(slots[0]).unwrap().unwrap().seen_at,
        Some(earlier)
    );
}

#[test]
fn a_section_on_the_card_lists_the_images_its_page_draws() {
    use display::font::TypeSettings;
    for portrait in [false, true] {
        let disk = fat::Disk::new();
        let manager: VolumeManager<_, _, 8, 8, 1> =
            VolumeManager::new_with_limits(&disk, fat::Clock, 0);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
        let mut store = ReaderStore::new();
        store.set_layout(TypeSettings::DEFAULT, portrait);
        let spine = 3;
        for block in 0..80usize {
            if block % 6 == 2 {
                let (width, height) =
                    fit_image(store.page_box(), 300, if block % 4 == 0 { 900 } else { 90 });
                assert!(store.push_image_block(height, spine, |out| {
                    proto::image::token::encode_image_token(block as u16, width, Some("alt"), out)
                        .map(str::len)
                }));
            } else if block % 6 == 4 {
                // A line with images inside it, at every offset from the
                // start, so some tokens straddle the pieces a section is
                // read in.
                let mut text = "w".repeat(block % 61);
                for image in 0..1 + block % 4 {
                    let mut token = [0u8; proto::image::token::INLINE_IMAGE_TOKEN_BYTES];
                    text.push_str(proto::image::token::encode_inline_image_token(
                        proto::image::token::InlineImageToken {
                            slot: (100 + block * 4 + image) as u16,
                            width: 17,
                            height: 12 + image as u16,
                        },
                        &mut token,
                    ));
                    text.push_str(&" and".repeat(image * 7 + block % 5));
                }
                text.push('.');
                assert!(store.push_line_block(
                    &text,
                    display::font::FontStyle::Regular,
                    proto::text::TextRole::Body,
                    proto::text::TextAlign::Left,
                    true,
                    spine,
                ));
                store.mark_last_block_inline_images(12 + (block % 4) as u16);
            } else {
                assert!(store.push_line_block(
                    &format!("line {block} of the chapter"),
                    display::font::FontStyle::Regular,
                    proto::text::TextRole::Body,
                    proto::text::TextAlign::Left,
                    block % 5 == 4,
                    spine,
                ));
            }
            reader_layout::extend_page_index(&mut store);
        }
        store.set_cached_spine(spine);
        store.set_current_section_range(40, store.page_count);
        assert!(reader_cache_files::write_v2_section_cache(
            &root, KEY, ID, 9, &store
        ));
        let section = proto::cache::BookV2SectionRecord {
            section: 9,
            spine,
            start_page: 40,
            page_count: store.page_count as u16,
            partial: false,
        };
        let dir = book_images::book_cache_dir(&root, KEY).unwrap();
        let mut seen_any = false;
        for page in 40..40 + store.page_count as u32 {
            let mut resident = vec![0u8; 512];
            let mut resident = PageImages::new(&mut resident);
            image_decode::resident_page_images(&store, page, &mut resident);
            let from_ram: Vec<u16> = (0..resident.len())
                .map(|at| resident.get(at).unwrap())
                .inspect(|image| assert!(image.drawn.is_some()))
                .map(|image| image.slot)
                .collect();
            let mut from_card = Vec::new();
            assert!(reader_cache_files::read_section_page_images(
                &dir,
                ID,
                &store,
                section,
                page,
                |slot| {
                    from_card.push(slot);
                    true
                }
            ));
            assert_eq!(from_card, from_ram, "page {page} portrait={portrait}");
            seen_any |= !from_ram.is_empty();
        }
        assert!(seen_any);
        // Images inside lines are listed too, from RAM and from the card.
        let mut listed = 0;
        for page in 40..40 + store.page_count as u32 {
            assert!(reader_cache_files::read_section_page_images(
                &dir,
                ID,
                &store,
                section,
                page,
                |slot| {
                    listed += usize::from(slot >= 100);
                    true
                }
            ));
        }
        assert_eq!(
            listed,
            (0..80).filter(|b| b % 6 == 4).map(|b| 1 + b % 4).sum()
        );
        // A page past the section, another book's copy, or another layout
        // lists nothing.
        let end = 40 + store.page_count as u32;
        assert!(!reader_cache_files::read_section_page_images(
            &dir,
            ID,
            &store,
            section,
            end,
            |_| true
        ));
        assert!(!reader_cache_files::read_section_page_images(
            &dir,
            (ID.0, ID.1 + 1),
            &store,
            section,
            40,
            |_| true
        ));
        store.set_layout(
            TypeSettings {
                spacing: display::font::LineSpacing::Relaxed,
                ..TypeSettings::DEFAULT
            },
            portrait,
        );
        assert!(!reader_cache_files::read_section_page_images(
            &dir,
            ID,
            &store,
            section,
            40,
            |_| true
        ));
    }
}

#[test]
fn the_window_is_the_next_three_pages_then_the_one_before() {
    assert_eq!(window_pages(10).collect::<Vec<_>>(), [11, 12, 13, 9, 10]);
    assert_eq!(window_pages(0).collect::<Vec<_>>(), [1, 2, 3, 0]);
    assert_eq!(
        window_pages(u32::MAX - 1).collect::<Vec<_>>(),
        [u32::MAX, u32::MAX - 2, u32::MAX - 1]
    );
}

#[test]
fn reading_order_runs_forward_from_the_reader_then_wraps() {
    let reader = ImagePlace { spine: 4, order: 2 };
    let at = |spine, order| Some(ImagePlace { spine, order });
    let mut places = [
        at(4, 1),
        at(0, 0),
        at(4, 2),
        at(9, 0),
        None,
        at(4, 3),
        at(5, 0),
        at(3, 60_000),
    ];
    places.sort_by_key(|&place| reading_distance(place, reader));
    assert_eq!(
        places,
        [
            at(4, 2),
            at(4, 3),
            at(5, 0),
            at(9, 0),
            at(0, 0),
            at(3, 60_000),
            at(4, 1),
            None
        ]
    );
}

#[test]
fn nearest_keeps_the_closest_as_many_as_fit_in_order() {
    let mut buf = vec![0xa5u8; 8 * 3 + 5];
    let mut nearest = Nearest::new(&mut buf);
    assert!(nearest.is_empty());
    for (slot, distance) in [(1, 50), (2, 7), (3, 900), (4, 3), (5, 60), (6, 7)] {
        nearest.offer(slot, distance);
    }
    let slots: Vec<u16> = nearest.into_slots().collect();
    assert_eq!(slots.len(), 3);
    assert_eq!(slots[0], 4);
    assert!(slots[1..] == [2, 6] || slots[1..] == [6, 2]);
    // No room at all keeps nothing.
    let mut none = Nearest::new(&mut []);
    none.offer(1, 0);
    assert!(none.is_empty());
}

#[test]
fn page_images_round_trip_and_stop_when_full() {
    let mut buf = vec![0xa5u8; 6 * 2 + 3];
    let mut list = PageImages::new(&mut buf);
    let first = PageImage {
        slot: 7,
        drawn: Some((213, 300)),
    };
    let second = PageImage {
        slot: 9,
        drawn: None,
    };
    assert!(list.push(first));
    assert!(list.push(second));
    assert!(!list.push(first), "full");
    assert_eq!(list.len(), 2);
    assert_eq!(list.get(0), Some(first));
    assert_eq!(list.get(1), Some(second));
    assert_eq!(list.get(2), None);
}

#[test]
fn only_probed_images_the_decoder_takes_wait_for_a_decode() {
    let mut entry = ImageEntry {
        probe: ProbeStatus::Supported,
        width: 640,
        height: 900,
        ..ImageEntry::default()
    };
    let landscape = fit_image(PageBox::LANDSCAPE, 640, 900);
    assert_eq!(pending_size(&entry, false), Some(landscape));
    assert_eq!(
        pending_size(&entry, true),
        Some(fit_image(PageBox::PORTRAIT, 640, 900))
    );
    entry.landscape.status = DecodeStatus::Failed;
    assert_eq!(pending_size(&entry, false), None);
    assert!(pending_size(&entry, true).is_some());
    entry.portrait.status = DecodeStatus::Decoded;
    assert_eq!(pending_size(&entry, true), None);
    for probe in [
        ProbeStatus::Unprobed,
        ProbeStatus::Unsupported,
        ProbeStatus::Failed,
    ] {
        let other = ImageEntry {
            probe,
            width: 640,
            height: 900,
            ..ImageEntry::default()
        };
        assert_eq!(pending_size(&other, false), None, "{probe:?}");
    }
}

#[test]
fn outcomes_have_their_bench_names() {
    assert_eq!(DecodeOutcome::Decoded.label(), "decoded");
    assert_eq!(DecodeOutcome::Cancelled.label(), "cancelled");
    assert_eq!(DecodeOutcome::Failed.label(), "failed");
    assert_eq!(DecodeOutcome::Retry.label(), "retry");
}

const NAME: &str = "Plates.epub";

/// A store holding one page, of one image block naming `slot` at `width
/// × height`, for the book whose cache directory is `NAME`'s.
fn one_image_store(slot: u16, width: u16, height: u16, portrait: bool) -> ReaderStore {
    let mut store = ReaderStore::new();
    store.set_active_entry(0, NAME, "PLATES.EPU", true, ID.1, ID.0, None);
    store.set_layout(TypeSettings::DEFAULT, portrait);
    store.set_book_has_images(true);
    assert!(store.push_image_block(height, 0, |out| {
        proto::image::token::encode_image_token(slot, width, Some("A plate"), out).map(str::len)
    }));
    reader_layout::rebuild_page_index(&mut store);
    store
}

#[test]
fn decoded_images_draw_from_the_pack_in_every_hold() {
    // The decode pass writes the packs, and the page draw reads them back:
    // stored, deflated and palette images, in both landscapes and portrait,
    // against the decoder and the ditherer run in RAM.
    let alice = palette_png(377, 520);
    let (bytes, _) = book(&[
        stored("small.png", SMALL_PNG),
        deflated("large.png", LARGE_PNG),
        deflated("plate.png", &alice),
    ]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let key = proto::cache::cache_key_for(NAME, ID.1);
    reader_cache_files::ensure_v2_cache_dirs(&root, key.as_str()).unwrap();
    let mut scratch = scratch();
    let names = ["small.png", "large.png", "plate.png"];
    let files: [&[u8]; 3] = [SMALL_PNG, LARGE_PNG, &alice];
    let slots = {
        let dir = book_images::book_cache_dir(&root, key.as_str()).unwrap();
        lay_out(&dir, &bytes, &names.map(|name| (name, AT)), &mut scratch)
    };
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    for portrait in [false, true] {
        for (index, &slot) in slots.iter().enumerate() {
            let dir = book_images::book_cache_dir(&root, key.as_str()).unwrap();
            let size = pending_size(&entry(&dir, slot), portrait).expect("to decode");
            let page = PageImage {
                slot,
                drawn: Some(size),
            };
            assert_eq!(
                decode_one(
                    &dir,
                    &bytes,
                    &reads,
                    &fails,
                    page,
                    portrait,
                    &mut scratch,
                    &|| false
                ),
                Some(DecodeOutcome::Decoded),
                "{}",
                names[index]
            );
        }
    }
    for frame in [
        FbFrame::Landscape,
        FbFrame::LandscapeFlipped,
        FbFrame::Portrait,
    ] {
        let portrait = frame == FbFrame::Portrait;
        for (index, &slot) in slots.iter().enumerate() {
            let size = {
                let dir = book_images::book_cache_dir(&root, key.as_str()).unwrap();
                let found = entry(&dir, slot);
                let page_box = PageBox::for_portrait(portrait);
                fit_image(page_box, found.width.into(), found.height.into())
            };
            let store = one_image_store(slot, size.0, size.1, portrait);
            let page = store.pages[0];
            let mut fb = Framebuffer::new();
            fb.set_frame(frame);
            // Dirty, as the borrowed band is on the device.
            let mut band = vec![0xa5u8; display::BAND_BYTES];
            page_images::draw_page(
                &root,
                &store,
                app_core::ReaderSource::sd(0).book_id(),
                page,
                &mut band,
                || 0,
                |images| ui::reading::draw_reading_page_body(&mut fb, &store, page, images),
            );
            let rows = reference_rows(files[index], size.0, size.1);
            let stride = usize::from(size.0.div_ceil(8));
            let mut want = Framebuffer::new();
            want.set_frame(frame);
            let mut drawn = 0;
            ui::reading::draw_reading_page_body(&mut want, &store, page, |fb, image| {
                drawn += 1;
                assert_eq!((image.width, image.height), size);
                for row in 0..usize::from(size.1) {
                    for column in 0..usize::from(size.0) {
                        let white = rows[row * stride + column / 8] & (0x80 >> (column % 8)) != 0;
                        fb.set_pixel(image.x as usize + column, image.y as usize + row, white);
                    }
                }
            });
            assert_eq!(drawn, 1);
            assert!(
                fb.bytes()[..] == want.bytes()[..],
                "{frame:?} {}",
                names[index]
            );
        }
    }
}

/// A store holding one line with the image in `slot` inside it, `width ×
/// height` as the line builder fitted it, between words.
fn inline_image_store(slot: u16, width: u16, height: u16, portrait: bool) -> ReaderStore {
    let mut store = ReaderStore::new();
    store.set_active_entry(0, NAME, "PLATES.EPU", true, ID.1, ID.0, None);
    store.set_layout(TypeSettings::DEFAULT, portrait);
    store.set_book_has_images(true);
    let mut token = [0u8; proto::image::token::INLINE_IMAGE_TOKEN_BYTES];
    let token = proto::image::token::encode_inline_image_token(
        proto::image::token::InlineImageToken {
            slot,
            width,
            height,
        },
        &mut token,
    );
    assert!(store.push_line_block(
        &format!("A large {token} initial"),
        display::font::FontStyle::Regular,
        proto::text::TextRole::Body,
        proto::text::TextAlign::Left,
        true,
        0,
    ));
    store.mark_last_block_inline_images(height);
    reader_layout::rebuild_page_index(&mut store);
    store
}

/// Draws `store`'s first page with the book's packs, and with the
/// reference rows of `file` at `size` in each image's rect, centred, for
/// comparison. Returns both frames and the rects the page asked for.
fn draw_both<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    store: &ReaderStore,
    file: &[u8],
    size: (u16, u16),
    frame: FbFrame,
) -> (Framebuffer, Framebuffer, Vec<(i16, i16, u16, u16)>)
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    let page = store.pages[0];
    let mut fb = Framebuffer::new();
    fb.set_frame(frame);
    let mut band = vec![0xa5u8; display::BAND_BYTES];
    page_images::draw_page(
        root,
        store,
        app_core::ReaderSource::sd(0).book_id(),
        page,
        &mut band,
        || 0,
        |images| ui::reading::draw_reading_page_body(&mut fb, store, page, images),
    );
    let rows = reference_rows(file, size.0, size.1);
    let stride = usize::from(size.0.div_ceil(8));
    let mut want = Framebuffer::new();
    want.set_frame(frame);
    let mut rects = Vec::new();
    ui::reading::draw_reading_page_body(&mut want, store, page, |fb, image| {
        rects.push((image.x, image.y, image.width, image.height));
        let x = image.x as usize + usize::from((image.width - size.0) / 2);
        let y = image.y as usize + usize::from((image.height - size.1) / 2);
        for row in 0..usize::from(size.1) {
            for column in 0..usize::from(size.0) {
                let white = rows[row * stride + column / 8] & (0x80 >> (column % 8)) != 0;
                fb.set_pixel(x + column, y + row, white);
            }
        }
    });
    (fb, want, rects)
}

#[test]
fn an_image_shown_inside_a_line_decodes_once_at_its_inline_size() {
    // An initial taller than the inline cap, laid out alone in one place
    // and inside a line in another: once a line shows it, it decodes at its
    // inline size, the block-size decode it had is forgotten, and the block
    // draws the smaller decode centred in its box.
    let initial = palette_png(100, 120);
    let (bytes, _) = book(&[deflated("large.png", &initial)]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let key = proto::cache::cache_key_for(NAME, ID.1);
    reader_cache_files::ensure_v2_cache_dirs(&root, key.as_str()).unwrap();
    let mut scratch = scratch();
    let dir = book_images::book_cache_dir(&root, key.as_str()).unwrap();
    let slot = lay_out(&dir, &bytes, &[("large.png", AT)], &mut scratch)[0];
    let (reads, fails) = (RefCell::new(Vec::new()), Cell::new(None));
    let block = |portrait| fit_image(PageBox::for_portrait(portrait), 100, 120);
    let inline =
        |portrait| ui::reading::fit_inline_image(PageBox::for_portrait(portrait), 100, 120);
    assert_eq!(inline(false), (53, 64));
    assert!(ui::reading::fits_inline(100, 120));
    // Decoded alone in a paragraph, in landscape.
    let page = PageImage {
        slot,
        drawn: Some(block(false)),
    };
    let decode = |page, portrait, scratch: &mut Scratch| {
        decode_one(
            &dir,
            &bytes,
            &reads,
            &fails,
            page,
            portrait,
            scratch,
            &|| false,
        )
    };
    assert_eq!(
        decode(page, false, &mut scratch),
        Some(DecodeOutcome::Decoded)
    );
    let before = pack_len(&dir, false);
    assert_eq!(pending_size(&entry(&dir, slot), false), None);

    // A line shows it: the mark is one write, and forgets that decode.
    {
        let images = book_images::open_existing_book_images(&dir, ID).unwrap();
        assert_eq!(images.mark_inline(slot), Ok(true));
        assert_eq!(images.mark_inline(slot), Ok(false), "marked once");
    }
    let found = entry(&dir, slot);
    assert!(found.inline && found.wanted);
    assert_eq!(found.landscape.status, DecodeStatus::NotDecoded);
    for portrait in [false, true] {
        assert_eq!(pending_size(&found, portrait), Some(inline(portrait)));
        assert_eq!(
            book_images::decoded_size(&found, portrait),
            inline(portrait)
        );
    }
    // A line's request decodes it at its inline size; so does a block's,
    // whose box holds that size. Each orientation decodes once.
    let line_page = PageImage {
        slot,
        drawn: Some(inline(false)),
    };
    assert_eq!(
        decode(line_page, false, &mut scratch),
        Some(DecodeOutcome::Decoded)
    );
    let block_page = PageImage {
        slot,
        drawn: Some(block(true)),
    };
    assert_eq!(
        decode(block_page, true, &mut scratch),
        Some(DecodeOutcome::Decoded)
    );
    assert_eq!(decode(line_page, false, &mut scratch), None);
    let found = entry(&dir, slot);
    let (record, _) = pack_record(&dir, false, found.landscape.pack_offset);
    assert!(
        found.landscape.pack_offset >= before,
        "the old record is an orphan"
    );
    assert_eq!((record.width, record.height), inline(false));
    // What the packs hold for images in use: the inline records alone.
    {
        let images = book_images::open_existing_book_images(&dir, ID).unwrap();
        let live = images.live_pack_bytes(false, &mut scratch.window).unwrap();
        assert_eq!(
            live,
            PackRecord::new(found.path_hash, 53, 64)
                .unwrap()
                .record_len()
        );
    }
    drop(dir);

    // Drawn inside a line, standing on its baseline; and alone, centred
    // in the box laid out for its block size.
    for frame in [
        FbFrame::Landscape,
        FbFrame::LandscapeFlipped,
        FbFrame::Portrait,
    ] {
        let portrait = frame == FbFrame::Portrait;
        let (width, height) = inline(portrait);
        let line = inline_image_store(slot, width, height, portrait);
        let (fb, want, rects) = draw_both(&root, &line, &initial, (width, height), frame);
        assert_eq!(rects.len(), 1);
        let baseline = PageBox::for_portrait(portrait).top
            + ui::reading::inline_line_ascent(TypeSettings::DEFAULT, line.blocks[0]);
        assert_eq!(rects[0].1 + rects[0].3 as i16, baseline);
        assert!(fb.bytes()[..] == want.bytes()[..], "{frame:?} inline");
        let (bw, bh) = block(portrait);
        let alone = one_image_store(slot, bw, bh, portrait);
        let (fb, want, rects) = draw_both(&root, &alone, &initial, (width, height), frame);
        assert_eq!(rects, [(rects[0].0, rects[0].1, bw, bh)]);
        assert!(fb.bytes()[..] == want.bytes()[..], "{frame:?} block");
    }
}

#[test]
fn the_inline_mark_forgets_only_decodes_whose_size_it_changes() {
    // A banner the same size inside a line and alone in landscape, but
    // narrowed further inside a portrait line.
    let banner = palette_png(600, 40);
    let landscape = (PageBox::LANDSCAPE, false);
    let portrait = (PageBox::PORTRAIT, true);
    assert!(ui::reading::fits_inline(600, 40));
    assert_eq!(
        fit_image(PageBox::LANDSCAPE, 600, 40),
        ui::reading::fit_inline_image(PageBox::LANDSCAPE, 600, 40)
    );
    assert_ne!(
        fit_image(PageBox::PORTRAIT, 600, 40),
        ui::reading::fit_inline_image(PageBox::PORTRAIT, 600, 40)
    );
    let (bytes, _) = book(&[stored("banner.png", &banner)]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let slot = lay_out(&dir, &bytes, &[("banner.png", AT)], &mut scratch)[0];
    let (reads, fails) = (RefCell::new(Vec::new()), Cell::new(None));
    for (page_box, orientation) in [landscape, portrait] {
        let page = PageImage {
            slot,
            drawn: Some(fit_image(page_box, 600, 40)),
        };
        assert_eq!(
            decode_one(
                &dir,
                &bytes,
                &reads,
                &fails,
                page,
                orientation,
                &mut scratch,
                &|| false
            ),
            Some(DecodeOutcome::Decoded)
        );
    }
    let images = book_images::open_existing_book_images(&dir, ID).unwrap();
    assert_eq!(images.mark_inline(slot), Ok(true));
    drop(images);
    let found = entry(&dir, slot);
    assert!(found.inline);
    // Landscape's decode is the size a line draws already: kept.
    assert_eq!(found.landscape.status, DecodeStatus::Decoded);
    assert_eq!(found.portrait.status, DecodeStatus::NotDecoded);
    assert_eq!(pending_size(&found, false), None);
    assert_eq!(
        pending_size(&found, true),
        Some(ui::reading::fit_inline_image(PageBox::PORTRAIT, 600, 40))
    );
}

#[test]
fn a_line_showing_an_image_that_lost_its_mark_marks_it_again() {
    // An index built again while the sections lasted: the line names the
    // inline size, the entry has no mark and a block-size decode.
    let (bytes, _) = book(&[deflated("large.png", LARGE_PNG)]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let slot = lay_out(&dir, &bytes, &[("large.png", AT)], &mut scratch)[0];
    let (reads, fails) = (RefCell::new(Vec::new()), Cell::new(None));
    let block = fit_image(PageBox::LANDSCAPE, 640, 900);
    let block_page = PageImage {
        slot,
        drawn: Some(block),
    };
    assert_eq!(
        decode_one(
            &dir,
            &bytes,
            &reads,
            &fails,
            block_page,
            false,
            &mut scratch,
            &|| false
        ),
        Some(DecodeOutcome::Decoded)
    );
    // An illustration this tall never goes inside a line, so a line asking
    // for some smaller size of it is damage: nothing is marked.
    let inline = ui::reading::fit_inline_image(PageBox::LANDSCAPE, 640, 900);
    let line_page = PageImage {
        slot,
        drawn: Some(inline),
    };
    assert!(!ui::reading::fits_inline(640, 900));
    assert_eq!(
        decode_one(
            &dir,
            &bytes,
            &reads,
            &fails,
            line_page,
            false,
            &mut scratch,
            &|| false
        ),
        None
    );
    assert!(!entry(&dir, slot).inline);

    // A drop cap, though, is marked again and decoded at the line's size.
    let initial = palette_png(75, 75);
    let (bytes, _) = book(&[stored("t.png", &initial)]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let slot = lay_out(&dir, &bytes, &[("t.png", AT)], &mut scratch)[0];
    let block_page = PageImage {
        slot,
        drawn: Some((75, 75)),
    };
    assert_eq!(
        decode_one(
            &dir,
            &bytes,
            &reads,
            &fails,
            block_page,
            false,
            &mut scratch,
            &|| false
        ),
        Some(DecodeOutcome::Decoded)
    );
    let images = book_images::open_existing_book_images(&dir, ID).unwrap();
    let archive = || {
        Some(Archive {
            bytes: &bytes,
            reads: &reads,
            fails: &fails,
        })
    };
    let mut session = ImageSession::new(&dir, images, archive, false, now);
    let line_page = PageImage {
        slot,
        drawn: Some((64, 64)),
    };
    assert_eq!(
        session.decode_page_image(line_page, &mut parts(&mut scratch), &|| false),
        Some(DecodeOutcome::Decoded)
    );
    assert!(session.stats.remarked && session.stats.forgot_decodes());
    drop(session);
    let found = entry(&dir, slot);
    assert!(found.inline);
    let (record, _) = pack_record(&dir, false, found.landscape.pack_offset);
    assert_eq!((record.width, record.height), (64, 64));
}

#[test]
fn the_reader_s_place_counts_images_inside_lines() {
    let (bytes, _) = book(&[
        stored("small.png", SMALL_PNG),
        stored("large.png", LARGE_PNG),
    ]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let at = |order| ImagePlace { spine: 2, order };
    let slots = lay_out(
        &dir,
        &bytes,
        &[("small.png", at(4)), ("large.png", at(9))],
        &mut scratch,
    );
    // A page of plain lines, then a page whose first block is a line with
    // the large image inside it.
    let mut store = ReaderStore::new();
    store.set_layout(TypeSettings::DEFAULT, false);
    for line in 0..50 {
        let text = if line == 20 {
            let mut token = [0u8; proto::image::token::INLINE_IMAGE_TOKEN_BYTES];
            let token = proto::image::token::encode_inline_image_token(
                proto::image::token::InlineImageToken {
                    slot: slots[1],
                    width: 45,
                    height: 64,
                },
                &mut token,
            );
            format!("words {token} more")
        } else {
            format!("line {line}")
        };
        assert!(store.push_line_block(
            &text,
            display::font::FontStyle::Regular,
            proto::text::TextRole::Body,
            proto::text::TextAlign::Left,
            true,
            2,
        ));
        if line == 20 {
            store.mark_last_block_inline_images(64);
        }
    }
    reader_layout::rebuild_page_index(&mut store);
    store.set_cached_spine(2);
    store.set_current_section_range(0, store.page_count);
    let pages = store.page_count as u32;
    store.set_book_index(pages, false, &[section(0, 2, 0, pages as u16)]);
    let images = book_images::open_existing_book_images(&dir, ID).unwrap();
    let page_of = |block: u16| {
        (0..store.page_count)
            .find(|&page| {
                let record = store.pages[page];
                (record.first_block..record.first_block + record.block_count).contains(&block)
            })
            .unwrap() as u32
    };
    // From a page before the line, the next image is the line's; from a
    // page after it, the reader is just past it.
    assert_eq!(image_decode::reader_place(&store, 0, &images), at(9));
    let last = store.page_count as u32 - 1;
    assert!(page_of(20) < last);
    assert_eq!(image_decode::reader_place(&store, last, &images), at(10));
}

#[test]
fn rebuilding_the_index_deletes_the_packs_and_the_spool() {
    let (bytes, _) = book(&[stored("small.png", SMALL_PNG)]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let slots = lay_out(&dir, &bytes, &[("small.png", AT)], &mut scratch);
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    for portrait in [false, true] {
        let page = PageImage {
            slot: slots[0],
            drawn: None,
        };
        assert_eq!(
            decode_one(
                &dir,
                &bytes,
                &reads,
                &fails,
                page,
                portrait,
                &mut scratch,
                &|| false
            ),
            Some(DecodeOutcome::Decoded)
        );
    }
    // A spool a power cut left behind.
    dir.open_file_in_dir(SPOOL_FILE, Mode::ReadWriteCreate)
        .unwrap()
        .write(&[1; 100])
        .unwrap();
    let mut tail = [0u8; 4096];
    let archive = Archive {
        bytes: &bytes,
        reads: &reads,
        fails: &fails,
    };
    let mut zip = ZipStream::new(archive, &mut tail).unwrap();
    let open = |zip: &mut ZipStream<'_, Archive<'_>>, scratch: &mut Scratch| {
        let mut rebuilt = false;
        book_images::open_book_images(
            &dir,
            ID,
            zip,
            &mut scratch.walk,
            &mut scratch.window,
            &mut rebuilt,
        )
        .unwrap();
        rebuilt
    };
    // A sound index opens as it is, packs and all.
    assert!(!open(&mut zip, &mut scratch));
    for name in [pack_file(false), pack_file(true), SPOOL_FILE] {
        assert!(has_file(&dir, name), "{name}");
    }
    // A damaged one is built again, and nothing of the old packs is kept
    // for the new index to point past.
    dir.open_file_in_dir(proto::image::index::IMAGE_INDEX_FILE, Mode::ReadWriteAppend)
        .map(|file| {
            file.seek_from_start(30).unwrap();
            file.write(&[0x55]).unwrap();
        })
        .unwrap();
    assert!(open(&mut zip, &mut scratch));
    for name in [pack_file(false), pack_file(true), SPOOL_FILE] {
        assert!(!has_file(&dir, name), "{name}");
    }
    let found = entry(&dir, slots[0]);
    assert_eq!(found.decode(false).status, DecodeStatus::NotDecoded);
    assert_eq!(found.decode(true).status, DecodeStatus::NotDecoded);
}

#[test]
fn a_pack_full_of_orphans_starts_afresh_and_one_full_of_images_fails() {
    let names = ["a.png", "b.png", "c.png"];
    let (bytes, _) = book(&names.map(|name| stored(name, SMALL_PNG)));
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let slots = lay_out(&dir, &bytes, &names.map(|name| (name, AT)), &mut scratch);
    let first = entry(&dir, slots[0]);
    let (width, height) = pending_size(&first, false).unwrap();
    let record = PackRecord::new(first.path_hash, width, height)
        .unwrap()
        .record_len();
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    let decode = |slot, cap, gives_way: &dyn Fn() -> bool, scratch: &mut Scratch| {
        let images = book_images::open_existing_book_images(&dir, ID).unwrap();
        let archive = || {
            Some(Archive {
                bytes: &bytes,
                reads: &reads,
                fails: &fails,
            })
        };
        let mut session = ImageSession::new(&dir, images, archive, false, now).with_pack_cap(cap);
        let page = PageImage { slot, drawn: None };
        let outcome = session.decode_page_image(page, &mut parts(scratch), gives_way);
        (outcome, session.stats.pack_restarted)
    };
    let cap = 5 * record;
    assert_eq!(
        decode(slots[0], cap, &|| false, &mut scratch),
        (Some(DecodeOutcome::Decoded), false)
    );
    // Cancelled decodes of another image fill the pack with orphan tails.
    let mut cancels = 0;
    while pack_len(&dir, false) + record <= cap {
        let asked = Cell::new(0);
        let late_press = || {
            asked.set(asked.get() + 1);
            asked.get() > 250
        };
        let before = pack_len(&dir, false);
        assert_eq!(
            decode(slots[2], cap, &late_press, &mut scratch).0,
            Some(DecodeOutcome::Cancelled)
        );
        assert!(pack_len(&dir, false) > before, "an orphan tail");
        cancels += 1;
        assert!(cancels < 20);
    }
    // The records images use, with the next, fill at most half the pack:
    // it is started afresh, and the image decodes into it.
    assert_eq!(
        decode(slots[2], cap, &|| false, &mut scratch),
        (Some(DecodeOutcome::Decoded), true)
    );
    assert_eq!(pack_len(&dir, false), record);
    assert_eq!(entry(&dir, slots[2]).decode(false).pack_offset, 0);
    let forgotten = entry(&dir, slots[0]);
    assert_eq!(forgotten.decode(false).status, DecodeStatus::NotDecoded);
    assert!(forgotten.wanted);
    // Decoded again, it goes after.
    assert_eq!(
        decode(slots[0], cap, &|| false, &mut scratch),
        (Some(DecodeOutcome::Decoded), false)
    );
    // A pack the images' own records fill is full: the next image fails
    // rather than undo the others.
    let tight = 2 * record + record / 2;
    assert_eq!(
        decode(slots[1], tight, &|| false, &mut scratch),
        (Some(DecodeOutcome::Failed), false)
    );
    assert_eq!(
        entry(&dir, slots[1]).decode(false).status,
        DecodeStatus::Failed
    );
    assert_eq!(
        entry(&dir, slots[0]).decode(false).status,
        DecodeStatus::Decoded
    );
}

#[test]
fn a_spool_gives_way_between_pieces_of_input_that_never_fill_its_output() {
    // A deflated image whose stream opens with its first bytes, which a
    // probe reads, then runs on through some 100 KB of empty blocks, which
    // inflate to nothing, before the rest.
    let mut stream = vec![0u8];
    stream.extend_from_slice(&64u16.to_le_bytes());
    stream.extend_from_slice(&(!64u16).to_le_bytes());
    stream.extend_from_slice(&SMALL_PNG[..64]);
    for _ in 0..20_000 {
        stream.extend_from_slice(&[0, 0, 0, 0xff, 0xff]);
    }
    let rest = deflate_stored(&SMALL_PNG[64..]);
    stream.extend_from_slice(&rest);
    let (bytes, spans) = book(&[("slow.png".to_string(), 8, stream, SMALL_PNG.len() as u32)]);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    let slots = lay_out(&dir, &bytes, &[("slow.png", AT)], &mut scratch);
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    let page = PageImage {
        slot: slots[0],
        drawn: None,
    };
    // A press after the first piece of input.
    let asked = Cell::new(0);
    let press = || {
        asked.set(asked.get() + 1);
        asked.get() > 1
    };
    reads.borrow_mut().clear();
    let before = entry(&dir, slots[0]);
    assert_eq!(
        decode_one(
            &dir,
            &bytes,
            &reads,
            &fails,
            page,
            false,
            &mut scratch,
            &press
        ),
        Some(DecodeOutcome::Cancelled)
    );
    let entry_span = spans[1];
    let read: usize = reads
        .borrow()
        .iter()
        .filter(|&&(offset, _)| offset >= entry_span.0 && offset < entry_span.1)
        .map(|&(_, len)| len)
        .sum();
    assert!(read <= 4096 + 256, "{read} bytes read before giving way");
    assert_eq!(entry(&dir, slots[0]), before);
    assert!(!has_file(&dir, SPOOL_FILE));
    // Left alone, it decodes.
    assert_eq!(
        decode_one(
            &dir,
            &bytes,
            &reads,
            &fails,
            page,
            false,
            &mut scratch,
            &|| false
        ),
        Some(DecodeOutcome::Decoded)
    );
}

#[test]
fn a_rest_pass_gives_way_while_it_reads_the_index() {
    let names = ["a.png", "b.png"];
    let (bytes, _) = book(&names.map(|name| stored(name, SMALL_PNG)));
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let mut scratch = scratch();
    lay_out(&dir, &bytes, &names.map(|name| (name, AT)), &mut scratch);
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    let images = book_images::open_existing_book_images(&dir, ID).unwrap();
    let archive = || {
        Some(Archive {
            bytes: &bytes,
            reads: &reads,
            fails: &fails,
        })
    };
    let mut session = ImageSession::new(&dir, images, archive, false, now);
    let asked = Cell::new(0);
    let press = || {
        asked.set(asked.get() + 1);
        true
    };
    let mut list = vec![0u8; 64];
    assert!(!session.decode_nearest(AT, &mut list, &mut parts(&mut scratch), &press, &|| false));
    assert_eq!(asked.get(), 1, "the first entry read gave way");
    assert_eq!(session.stats.decoded, 0);
    assert!(reads.borrow().is_empty(), "the book is never read");
}

fn section(id: u16, spine: u16, start: u32, pages: u16) -> proto::cache::BookV2SectionRecord {
    proto::cache::BookV2SectionRecord {
        section: id,
        spine,
        start_page: start,
        page_count: pages,
        partial: false,
    }
}

#[test]
fn a_settled_book_opens_no_image_session_after_a_page_turn() {
    // A laid-out book of 60 pages with images.
    let mut store = ReaderStore::new();
    store.set_active_entry(0, NAME, "PLATES.EPU", true, ID.1, ID.0, None);
    store.set_cache_key(proto::cache::cache_key_for(NAME, ID.1).as_str());
    store.set_book_index(60, false, &[section(0, 0, 0, 30), section(1, 1, 30, 30)]);
    store.set_book_has_images(true);
    let work = IdleImageWork::default();
    // Nothing known yet: the window around the page first, then the rest.
    assert_eq!(work.next(&store, 10, false), IdleImageStep::Window);
    let tried = IdleImageWork {
        window_tried: true,
        ..work
    };
    assert_eq!(tried.next(&store, 10, false), IdleImageStep::Rest);
    assert_eq!(
        tried.next(&store, 10, true),
        IdleImageStep::Nothing,
        "layout first"
    );
    let stuck = IdleImageWork {
        window_tried: true,
        rest_stuck: true,
    };
    assert_eq!(stuck.next(&store, 10, false), IdleImageStep::Nothing);

    // A window the note covers needs no pass: pages 9 to 13, and the turn
    // to 11 asks for 10 to 14.
    store.note_settled_pages(9, 14);
    assert!(store.images_settled(13) && !store.images_settled(14));
    assert_eq!(work.next(&store, 10, false), IdleImageStep::Rest);
    assert_eq!(work.next(&store, 11, false), IdleImageStep::Window);
    store.note_settled_pages(14, 15);
    assert_eq!(
        work.next(&store, 11, false),
        IdleImageStep::Rest,
        "the runs join"
    );
    // A run apart replaces it.
    store.note_settled_pages(40, 41);
    assert!(store.images_settled(40) && !store.images_settled(12));
    // The window at the book's end holds only the pages it has.
    store.note_settled_pages(58, 60);
    assert_eq!(work.next(&store, 59, false), IdleImageStep::Rest);

    // Once the rest pass finds nothing left, no page asks for anything:
    // every turn after leaves the card alone.
    store.note_book_images_settled();
    for page in [0, 10, 33, 59] {
        assert!(store.images_settled(page));
        assert_eq!(work.next(&store, page, false), IdleImageStep::Nothing);
    }
    // Until the book grows (its pages' note stands), the layout or the
    // book changes, or a book has no images at all.
    store.note_book_grew();
    assert_eq!(work.next(&store, 59, false), IdleImageStep::Rest);
    assert_eq!(work.next(&store, 30, false), IdleImageStep::Window);
    store.note_book_images_settled();
    store.set_layout(TypeSettings::DEFAULT, true);
    assert!(!store.images_settled(59));
    assert_eq!(work.next(&store, 59, false), IdleImageStep::Window);
    store.note_book_images_settled();
    store.set_cache_key(proto::cache::cache_key_for(NAME, ID.1).as_str());
    assert!(store.book_images_settled(), "the same book");
    store.set_cache_key("E0000002");
    assert!(!store.book_images_settled(), "another book");
    store.set_book_has_images(false);
    assert_eq!(work.next(&store, 10, false), IdleImageStep::Nothing);
}
