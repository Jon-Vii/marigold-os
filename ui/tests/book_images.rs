//! A book's images on the card during a build (`fw::book_images`), and the
//! custom-font page draw that hands image blocks to the image drawer, run
//! against the pinned FAT code on an in-memory card.
#![allow(dead_code, unused_imports, unexpected_cfgs)]
extern crate self as esp_println;
extern crate self as hal_ext;
#[path = "../../hal-ext/src/nvm.rs"]
pub mod nvm;
pub use std::println;
#[path = "../../fw/src/book_images.rs"]
mod book_images;
#[path = "../../fw/src/custom_font.rs"]
mod custom_font;
#[path = "support/fat.rs"]
mod fat;
#[path = "support/jpeg.rs"]
mod jpeg;
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

use book_images::{BookImages, ImagePlacement, ProbeStop};
use embedded_sdmmc::{Mode, VolumeIdx, VolumeManager};
use proto::epub::{ReadAt, ZipInflateScratch, ZipStream};
use proto::image::index::{ImagePlace, IMAGE_INDEX_FILE};
use reader_store::ReaderStore;
use std::cell::{Cell, RefCell};
use ui::reading::PageBox;
use zip::{zip, zip_raw, Archive};

const KEY: &str = "E0000001";
const ID: (u32, u32) = (1234, 5678);
const CHAPTER: &str = "OEBPS/text/ch01.xhtml";
/// 213 × 300.
const SMALL_PNG: &[u8] = include_bytes!("../../fixtures/images/orientation-small.png");
/// 640 × 900.
const LARGE_PNG: &[u8] = include_bytes!("../../fixtures/images/orientation-test.png");

/// The body of the APP1 the EXIF JPEGs carry before their frame header.
const APP1_BYTES: usize = 65_000;

/// A deflate stream of stored blocks holding `plain`, of any length.
fn deflated(plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pieces = plain.chunks(65_535).peekable();
    while let Some(piece) = pieces.next() {
        out.push(u8::from(pieces.peek().is_none()));
        out.extend_from_slice(&(piece.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(piece.len() as u16)).to_le_bytes());
        out.extend_from_slice(piece);
    }
    out
}

/// A book with a chapter, a stylesheet, and images of every kind a probe
/// sorts: stored and deflated PNGs and JPEGs (two behind a 64 KB APP1), a
/// progressive JPEG, the start of a JPEG named as a PNG, damaged ones (a
/// broken header, a local header naming another file, a stream that never
/// starts), and one a chapter never names.
fn book() -> (Vec<u8>, Vec<(u32, u32)>) {
    let mut broken = SMALL_PNG[..20].to_vec();
    broken.extend_from_slice(&[0xff; 20]);
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0, 16];
    let photo = jpeg::photo(160, 120, false);
    let exif = jpeg::with_app1(&photo, APP1_BYTES);
    let progressive = jpeg::photo(64, 48, true);
    // Empty stored blocks, never final: they inflate to nothing forever,
    // past any cap a probe sets.
    let endless: Vec<u8> = std::iter::repeat_n([0u8, 0, 0, 0xff, 0xff], 60_000)
        .flatten()
        .collect();
    let (mut bytes, spans) = zip_raw(&[
        (
            CHAPTER,
            8,
            deflated(b"<html><body><p>Text</p></body></html>"),
            37,
        ),
        ("OEBPS/style.css", 0, b"p { margin: 0 }".to_vec(), 15),
        (
            "OEBPS/images/small.png",
            0,
            SMALL_PNG.to_vec(),
            SMALL_PNG.len() as u32,
        ),
        (
            "OEBPS/images/large.png",
            8,
            deflated(LARGE_PNG),
            LARGE_PNG.len() as u32,
        ),
        (
            "OEBPS/images/photo.jpg",
            0,
            photo.clone(),
            photo.len() as u32,
        ),
        ("OEBPS/images/fake.png", 0, jpeg.to_vec(), 6),
        ("OEBPS/images/broken.png", 8, deflated(&broken), 40),
        (
            "OEBPS/images/renamed.png",
            0,
            SMALL_PNG.to_vec(),
            SMALL_PNG.len() as u32,
        ),
        ("OEBPS/images/endless.png", 8, endless, 2273),
        (
            "OEBPS/images/unused.png",
            0,
            SMALL_PNG.to_vec(),
            SMALL_PNG.len() as u32,
        ),
        ("OEBPS/images/exif.jpg", 0, exif.clone(), exif.len() as u32),
        (
            "OEBPS/images/zipped.jpeg",
            8,
            deflated(&exif),
            exif.len() as u32,
        ),
        (
            "OEBPS/images/progressive.jpg",
            0,
            progressive.clone(),
            progressive.len() as u32,
        ),
    ]);
    // The local header names another file; the central directory does not.
    let at = bytes
        .windows(7)
        .position(|name| name == b"renamed")
        .unwrap();
    bytes[at] = b'R';
    (bytes, spans)
}

struct Scratch {
    walk: Vec<u8>,
    window: Vec<u8>,
    compressed: Vec<u8>,
    inflate: ZipInflateScratch,
}

fn scratch() -> Scratch {
    // Dirty, as borrowed scratch is on the device.
    Scratch {
        walk: vec![0xa5; 8192],
        window: vec![0xa5; 24 * 1024],
        compressed: vec![0xa5; 8192],
        inflate: ZipInflateScratch::new(),
    }
}

fn place<D, T, const ND: usize, const NF: usize, const NV: usize>(
    images: &BookImages<'_, D, T, ND, NF, NV>,
    src: &str,
    first_pass: bool,
) -> ImagePlacement
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    place_at(images, src, first_pass, ImagePlace { spine: 0, order: 0 })
}

fn place_at<D, T, const ND: usize, const NF: usize, const NV: usize>(
    images: &BookImages<'_, D, T, ND, NF, NV>,
    src: &str,
    first_pass: bool,
    at: ImagePlace,
) -> ImagePlacement
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    let mut path = [0xa5u8; proto::epub::MAX_ENTRY_NAME_BYTES];
    images
        .place(CHAPTER, src, &mut path, false, first_pass, at)
        .expect("the card answers")
}

#[test]
fn a_session_builds_the_index_once_and_reads_no_image_to_do_it() {
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let (bytes, spans) = book();
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    // Too small to cache the directory, so the build streams it.
    let mut tail = [0u8; 300];
    let mut zip = ZipStream::new(
        Archive {
            bytes: &bytes,
            reads: &reads,
            fails: &fails,
        },
        &mut tail,
    )
    .unwrap();
    let mut scratch = scratch();
    reads.borrow_mut().clear();
    assert_eq!(zip.has_image_entries(&mut scratch.walk), Ok(true));
    assert!(reads.borrow().is_empty(), "known from the open");

    let dir = book_images::book_cache_dir(&root, KEY).expect("cache directory");
    let images = book_images::open_book_images(
        &dir,
        ID,
        &mut zip,
        &mut scratch.walk,
        &mut scratch.window,
        &mut false,
    )
    .expect("index built");
    // Only the central directory: no local header and no image byte.
    let entries_end = spans.last().unwrap().1;
    assert!(!reads.borrow().is_empty());
    assert!(
        reads
            .borrow()
            .iter()
            .all(|&(offset, _)| offset >= entries_end),
        "{:?}",
        reads.borrow()
    );
    drop(images);

    // The next session opens it without writing a byte.
    let writes = disk.writes.get();
    let images = book_images::open_book_images(
        &dir,
        ID,
        &mut zip,
        &mut scratch.walk,
        &mut scratch.window,
        &mut false,
    )
    .expect("index opens");
    assert!(matches!(
        place(&images, "../images/small.png", true),
        ImagePlacement::Unprobed { .. }
    ));
    drop(images);
    let marked = disk.writes.get();
    assert!(marked > writes, "the mark is written");

    // One for another copy of the book, or a damaged one, is built again,
    // and names every image by the same slot.
    for damage in [Damage::OtherBook, Damage::Header] {
        let slot = |images: &BookImages<'_, _, _, 8, 8, 1>| match place(
            images,
            "../images/large.png",
            true,
        ) {
            ImagePlacement::Unprobed { slot } => slot,
            other => panic!("{other:?}"),
        };
        let identity = if damage == Damage::OtherBook {
            (ID.0, ID.1 + 1)
        } else {
            ID
        };
        if damage == Damage::Header {
            let file = dir
                .open_file_in_dir(IMAGE_INDEX_FILE, Mode::ReadWriteAppend)
                .unwrap();
            file.seek_from_start(30).unwrap();
            file.write(&[0x55]).unwrap();
        }
        let before = {
            let images = book_images::open_book_images(
                &dir,
                ID,
                &mut zip,
                &mut scratch.walk,
                &mut scratch.window,
                &mut false,
            )
            .unwrap();
            slot(&images)
        };
        let images = book_images::open_book_images(
            &dir,
            identity,
            &mut zip,
            &mut scratch.walk,
            &mut scratch.window,
            &mut false,
        )
        .expect("rebuilt");
        assert_eq!(slot(&images), before, "{damage:?}");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Damage {
    OtherBook,
    Header,
}

#[test]
fn a_first_pass_marks_images_the_probe_sorts_and_the_second_pass_shows() {
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let (bytes, spans) = book();
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    let mut tail = [0u8; 4096];
    let mut zip = ZipStream::new(
        Archive {
            bytes: &bytes,
            reads: &reads,
            fails: &fails,
        },
        &mut tail,
    )
    .unwrap();
    let mut scratch = scratch();
    let dir = book_images::book_cache_dir(&root, KEY).unwrap();
    let images = book_images::open_book_images(
        &dir,
        ID,
        &mut zip,
        &mut scratch.walk,
        &mut scratch.window,
        &mut false,
    )
    .unwrap();

    // Sources that name nothing in the book.
    for src in [
        "missing.png",
        "http://example.com/a.png",
        "../../../escape.png",
        "",
        "../images/",
    ] {
        assert_eq!(place(&images, src, true), ImagePlacement::Missing, "{src}");
    }
    // The first pass marks what it meets, once: PNGs and JPEGs alike.
    let names = [
        "small.png",
        "large.png",
        "fake.png",
        "broken.png",
        "renamed.png",
        "endless.png",
        "photo.jpg",
        "exif.jpg",
        "zipped.jpeg",
        "progressive.jpg",
    ];
    let mut slots = Vec::new();
    for name in names {
        let src = format!("../images/{name}");
        let ImagePlacement::Unprobed { slot } = place(&images, &src, true) else {
            panic!("{src} is not probed yet");
        };
        let writes = disk.writes.get();
        assert_eq!(
            place(&images, &src, true),
            ImagePlacement::Unprobed { slot }
        );
        assert_eq!(disk.writes.get(), writes, "{src} is marked once");
        slots.push(slot);
    }
    // Sources are resolved, with query and fragment dropped.
    assert_eq!(
        place(&images, "/OEBPS/images/large.png?x#y", true),
        ImagePlacement::Unprobed { slot: slots[1] }
    );
    // A second pass never waits for a probe.
    assert_eq!(
        place(&images, "../images/unused.png", false),
        ImagePlacement::Missing
    );

    let probe = |zip: &mut ZipStream<'_, Archive<'_>>, scratch: &mut Scratch, gives_way: bool| {
        images.probe_marked(
            zip,
            0..=u16::MAX - 1,
            &mut scratch.compressed,
            &mut scratch.window,
            &mut scratch.inflate,
            || gives_way,
        )
    };
    // A read that fails stops the probing and records nothing for its
    // image, which is probed again next time.
    // Every read a probe makes, from here on.
    reads.borrow_mut().clear();
    let small = spans[2];
    fails.set(Some((small.0, small.0 + 1)));
    assert_eq!(
        probe(&mut zip, &mut scratch, false),
        Err(ProbeStop::Archive)
    );
    assert!(matches!(
        place(&images, "../images/small.png", true),
        ImagePlacement::Unprobed { .. }
    ));
    fails.set(None);
    // A press stops it after the image in hand.
    assert_eq!(
        probe(&mut zip, &mut scratch, true),
        Err(ProbeStop::Interrupted)
    );
    let probed = probe(&mut zip, &mut scratch, false).unwrap();
    assert!((1..names.len()).contains(&probed), "{probed}");
    assert_eq!(probe(&mut zip, &mut scratch, false), Ok(0), "all probed");
    let unused = spans[9];
    assert!(
        reads
            .borrow()
            .iter()
            .all(|&(offset, _)| offset < unused.0 || offset >= unused.1),
        "an image no chapter named is never read"
    );
    // A stream that never starts is read only as far as the cap.
    let endless = spans[8];
    let endless_read: usize = reads
        .borrow()
        .iter()
        .filter(|&&(offset, _)| offset >= endless.0 && offset < endless.1)
        .map(|&(_, len)| len)
        .sum();
    let cap = book_images::PROBE_INPUT_CAP as usize;
    assert!(
        endless_read > cap / 2 && endless_read <= cap + 512 + 64,
        "{endless_read} bytes"
    );
    // A stored JPEG's probe passes over the APP1 before its frame header
    // without reading it: one read of its start, then on past the APP1.
    let exif = spans[10];
    let data = exif.0 + 30 + "OEBPS/images/exif.jpg".len() as u32;
    let exif_reads: Vec<(u32, usize)> = reads
        .borrow()
        .iter()
        .copied()
        .filter(|&(offset, _)| offset >= data && offset < exif.1)
        .collect();
    let read: usize = exif_reads.iter().map(|&(_, len)| len).sum();
    assert!(read <= 4 * 512, "{exif_reads:?}");
    assert!(
        exif_reads
            .iter()
            .all(|&(offset, _)| offset < data + 512 || offset >= data + 6 + APP1_BYTES as u32),
        "{exif_reads:?}"
    );

    // The second pass shows what the decoder takes, fitted to the page.
    assert_eq!(
        place(&images, "../images/small.png", false),
        ImagePlacement::Shown {
            slot: slots[0],
            block: (213, 300),
            // An illustration: too tall to go inside a line.
            inline: None,
            inline_marked: false,
        }
    );
    assert_eq!(
        place(&images, "../images/large.png", false),
        ImagePlacement::Shown {
            slot: slots[1],
            block: ui::reading::fit_image(PageBox::LANDSCAPE, 640, 900),
            inline: None,
            inline_marked: false,
        },
        "a deflated image probes through the inflater"
    );
    // JPEGs, stored, behind a large APP1, and deflated through it, show
    // like PNGs.
    for (name, slot) in [("photo.jpg", 6), ("exif.jpg", 7), ("zipped.jpeg", 8)] {
        assert_eq!(
            place(&images, &format!("../images/{name}"), false),
            ImagePlacement::Shown {
                slot: slots[slot],
                block: (160, 120),
                inline: ui::reading::fits_inline(160, 120).then(|| ui::reading::fit_inline_image(
                    PageBox::LANDSCAPE,
                    160,
                    120
                )),
                inline_marked: false,
            },
            "{name}"
        );
    }
    // A progressive JPEG, the start of a JPEG behind a PNG's name, and
    // damaged images keep their alt text, and are not probed again.
    for name in [
        "progressive.jpg",
        "fake.png",
        "broken.png",
        "renamed.png",
        "endless.png",
    ] {
        assert_eq!(
            place(&images, &format!("../images/{name}"), true),
            ImagePlacement::Missing,
            "{name}"
        );
    }
    // Showing an image already wanted writes nothing.
    let writes = disk.writes.get();
    assert!(matches!(
        place(&images, "../images/small.png", true),
        ImagePlacement::Shown { .. }
    ));
    assert_eq!(disk.writes.get(), writes);
}

#[test]
fn a_damaged_central_directory_counts_as_no_images_and_the_book_still_opens() {
    let opf = b"<package/>";
    let files: [(&str, bool, &[u8]); 4] = [
        ("META-INF/container.xml", false, b"<container/>"),
        ("OEBPS/content.opf", false, opf),
        (
            "OEBPS/ch01.xhtml",
            true,
            b"<html><body><p>Text</p></body></html>",
        ),
        ("OEBPS/style.css", false, b"p { margin: 0 }"),
    ];
    let (mut bytes, _) = zip(&files);
    // The last central-directory entry's signature.
    let at = bytes
        .windows(15)
        .rposition(|name| name == b"OEBPS/style.css")
        .unwrap()
        - 46;
    assert_eq!(&bytes[at..at + 4], &0x0201_4b50u32.to_le_bytes());
    bytes[at] ^= 0xff;
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    // Cached whole, and too big to cache.
    for tail_len in [4096usize, 100] {
        let mut tail = vec![0u8; tail_len];
        let mut zip = ZipStream::new(
            Archive {
                bytes: &bytes,
                reads: &reads,
                fails: &fails,
            },
            &mut tail,
        )
        .unwrap();
        assert!(zip.has_image_entries(&mut [0u8; 512]).is_err());
        assert!(!book_images::book_has_images(&mut zip, &mut [0u8; 512]));
        for (name, _, _) in &files[..3] {
            assert!(
                zip.find_entry(name, &mut [0u8; 46], &mut [0u8; 160])
                    .is_ok(),
                "{name} with a {tail_len}-byte tail"
            );
        }
    }
}

#[test]
fn the_custom_face_hands_image_blocks_to_the_image_drawer() {
    use display::font::{FontFamily, FontStyle, TypeSettings};
    use proto::text::{TextAlign, TextRole};
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    {
        root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
        let xteink = root.open_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
        xteink
            .make_dir_in_dir(proto::font_pack::FONT_PACK_DIR)
            .unwrap();
        let fonts = xteink.open_dir(proto::font_pack::FONT_PACK_DIR).unwrap();
        let pack = fonts
            .open_file_in_dir(proto::font_pack::FONT_PACK_FILE, Mode::ReadWriteCreate)
            .unwrap();
        // The bench's one-face pack: every ASCII character is an 8 × 8 A.
        pack.write(include_bytes!("../../fw/src/custom_font/bench_font.fnt"))
            .unwrap();
    }
    let manifest = reader_cache_files::read_custom_font_manifest(&root).expect("pack");
    let mut store = ReaderStore::new();
    store.set_custom_font(
        Some(manifest.name.as_str()),
        manifest.identity,
        &manifest.faces[..manifest.face_count],
    );
    store.set_layout(
        TypeSettings {
            family: FontFamily::Custom,
            ..TypeSettings::DEFAULT
        },
        false,
    );
    let (regular, body) = (FontStyle::Regular, TextRole::Body);
    assert!(store.push_line_block("AAAA", regular, body, TextAlign::Left, true, 0));
    let (width, height) = ui::reading::fit_image(PageBox::LANDSCAPE, 213, 120);
    assert!(store.push_image_block(height, 0, |out| {
        proto::image::token::encode_image_token(3, width, Some("A plate"), out).map(str::len)
    }));
    assert!(store.push_line_block("AAAA", regular, body, TextAlign::Left, true, 0));
    reader_layout::rebuild_page_index(&mut store);
    assert_eq!(store.page_count, 1);

    let mut metrics = custom_font::MetricCache::new();
    let mut fb = display::fb::Framebuffer::new();
    fb.set_frame(display::fb::FbFrame::Landscape);
    let mut drawn = Vec::new();
    assert!(custom_font::draw_reading_page_body(
        &root,
        &mut fb,
        &store,
        &mut metrics,
        store.pages[0],
        |_, image| drawn.push((image.x, image.y, image.width, image.height, image.slot)),
    ));
    assert_eq!(drawn.len(), 1);
    let (x, y, w, h, slot) = drawn[0];
    assert_eq!((w, h, slot), (width, height, Some(3)));
    let ink = |fb: &display::fb::Framebuffer, x0: i16, y0: i16, x1: i16, y1: i16| {
        (y0..y1)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| !fb.pixel(x as usize, y as usize))
            .count()
    };
    // The lines were drawn in the custom face; the token was not.
    assert!(ink(&fb, 0, 0, fb.frame_width() as i16, y) > 0);
    assert_eq!(ink(&fb, x, y, x + w as i16, y + h as i16), 0);

    // The placeholder drawer outlines the image's rect.
    fb.clear(true);
    assert!(custom_font::draw_reading_page_body(
        &root,
        &mut fb,
        &store,
        &mut metrics,
        store.pages[0],
        ui::reading::draw_image_placeholder,
    ));
    let (x1, y1) = (x as usize + w as usize - 1, y as usize + h as usize - 1);
    assert!(!fb.pixel(x as usize, y as usize) && !fb.pixel(x1, y1));
}

#[test]
fn the_custom_face_stands_inline_images_in_their_lines() {
    use display::font::{FontFamily, FontStyle, TypeSettings};
    use proto::image::token::{encode_inline_image_token, InlineImageToken};
    use proto::text::{TextAlign, TextRole};
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    {
        root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
        let xteink = root.open_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
        xteink
            .make_dir_in_dir(proto::font_pack::FONT_PACK_DIR)
            .unwrap();
        let fonts = xteink.open_dir(proto::font_pack::FONT_PACK_DIR).unwrap();
        let pack = fonts
            .open_file_in_dir(proto::font_pack::FONT_PACK_FILE, Mode::ReadWriteCreate)
            .unwrap();
        pack.write(include_bytes!("../../fw/src/custom_font/bench_font.fnt"))
            .unwrap();
    }
    let manifest = reader_cache_files::read_custom_font_manifest(&root).expect("pack");
    let mut store = ReaderStore::new();
    store.set_custom_font(
        Some(manifest.name.as_str()),
        manifest.identity,
        &manifest.faces[..manifest.face_count],
    );
    store.set_layout(
        TypeSettings {
            family: FontFamily::Custom,
            ..TypeSettings::DEFAULT
        },
        false,
    );
    let mut token = [0u8; proto::image::token::INLINE_IMAGE_TOKEN_BYTES];
    let token = encode_inline_image_token(
        InlineImageToken {
            slot: 5,
            width: 30,
            height: 40,
        },
        &mut token,
    );
    let line = format!("AAAA {token} AAAA AAAA AAAA AAAA AAAA AAAA AAAA AAAA AAAA AAAA AAAA AAAA");
    let (regular, body) = (FontStyle::Regular, TextRole::Body);
    for (align, end) in [
        (TextAlign::Left, true),
        (TextAlign::Justify, false),
        (TextAlign::Center, true),
    ] {
        assert!(store.push_line_block(&line, regular, body, align, end, 0));
        store.mark_last_block_inline_images(40);
    }
    reader_layout::rebuild_page_index(&mut store);
    assert_eq!(store.page_count, 1);

    let mut metrics = custom_font::MetricCache::new();
    let mut fb = display::fb::Framebuffer::new();
    fb.set_frame(display::fb::FbFrame::Landscape);
    let mut drawn = Vec::new();
    assert!(custom_font::draw_reading_page_body(
        &root,
        &mut fb,
        &store,
        &mut metrics,
        store.pages[0],
        |_, image| drawn.push((image.x, image.y, image.width, image.height, image.slot)),
    ));
    let mut baselines = Vec::new();
    ui::reading::for_each_drawable_block(&store, store.pages[0], |block| {
        baselines.push(block.y);
        true
    });
    assert_eq!(drawn.len(), 3);
    let ink = |fb: &display::fb::Framebuffer, x0: i16, y0: i16, x1: i16, y1: i16| {
        (y0..y1)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| !fb.pixel(x as usize, y as usize))
            .count()
    };
    for (index, &(x, y, w, h, slot)) in drawn.iter().enumerate() {
        assert_eq!((w, h, slot), (30, 40, Some(5)));
        // On the baseline, with the line's text on either side of it and
        // none of its token drawn as glyphs.
        assert_eq!(y + h as i16, baselines[index]);
        assert_eq!(ink(&fb, x, y, x + w as i16, y + h as i16), 0);
        assert!(ink(&fb, 0, y, x, y + h as i16) > 0, "{index}");
        assert!(ink(&fb, x + w as i16, y, fb.frame_width() as i16, y + h as i16) > 0);
    }
    // Left and justified start at the same place but the justified line's
    // gap before the image grows; the centred line sits in the middle.
    let (left, justified, centred) = (drawn[0].0, drawn[1].0, drawn[2].0);
    assert!(justified > left, "{justified} > {left}");
    assert!(centred > justified);
}
