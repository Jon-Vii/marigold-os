use crate::display_flush::Epd;
use crate::library_sd::{SdSpiDevice, StaticTime};
use crate::reader_layout;
use crate::reader_store::{BookLoadStatus, ReaderStore, MAX_READER_BLOCK_TEXT};
use display::font::{literata, FontStyle};
use embassy_time::Instant;
use embedded_hal::spi::SpiBus as BlockingSpiBus;
use embedded_sdmmc::{Directory, File, Mode, SdCard, TimeSource, VolumeIdx, VolumeManager};
use esp_hal::gpio::Output;
use esp_hal::prelude::*;
use hal_ext::nvm::AppStateRecord;
use heapless::String;
use proto::book::BookId;
use proto::cache::{
    decode_block, decode_page, decode_section_header, encode_block, encode_book_header,
    encode_page, encode_section_header, encode_spine, encode_toc, BlockRecord, BookCacheHeader,
    SectionHeader, SpineRecord, TocRecord as CacheTocRecord, BLOCK_RECORD_BYTES, BOOK_HEADER_BYTES,
    PAGE_RECORD_BYTES, SECTION_HEADER_BYTES, SPINE_RECORD_BYTES, TOC_RECORD_BYTES,
};
use proto::epub::{
    parse_css_text_align, parse_epub2_ncx_to_sink, parse_epub3_nav_to_sink, parse_opf,
    xhtml_blocks_to_sink, CssRules, EpubTocSink, ReadAt, TocError, XhtmlBlockSink, XhtmlError,
    ZipInflateScratch, ZipStream, MAX_ENTRY_NAME_BYTES,
};
use proto::text::{TextAlign, TextRole};

const READER_COMPRESSED_SCRATCH: usize = 32_768;
const READER_OPF_SCRATCH: usize = 20_480;
const READER_CSS_SCRATCH: usize = 8_192;
const READER_XHTML_SCRATCH: usize = 36_864;
const CACHE_ROOT_DIR: &str = "XTEINK";
const CACHE_DIR: &str = "CACHE";
const CACHE_SECTIONS_DIR: &str = "SECTIONS";
const CACHE_BOOK_FILE: &str = "BOOK.BIN";
const STATE_FILE: &str = "STATE.BIN";

pub(crate) struct ReaderCacheScratch {
    tail: [u8; 4096],
    header: [u8; 46],
    name: [u8; MAX_ENTRY_NAME_BYTES],
    compressed: [u8; READER_COMPRESSED_SCRATCH],
    container: [u8; 4096],
    opf: [u8; READER_OPF_SCRATCH],
    css: [u8; READER_CSS_SCRATCH],
    xhtml: [u8; READER_XHTML_SCRATCH],
    zip_inflate: ZipInflateScratch,
}

struct CssScratch<'a> {
    header: &'a mut [u8; 46],
    name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
    css: &'a mut [u8; READER_CSS_SCRATCH],
    zip_inflate: &'a mut ZipInflateScratch,
}

struct TocScratch<'a> {
    header: &'a mut [u8; 46],
    name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
    xhtml: &'a mut [u8; READER_XHTML_SCRATCH],
    zip_inflate: &'a mut ZipInflateScratch,
}

struct LibraryTocSink<'a, 'p> {
    library: &'a mut ReaderStore,
    package: &'p proto::epub::EpubPackage<'p>,
}

impl EpubTocSink for LibraryTocSink<'_, '_> {
    fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
        let spine_index = self
            .package
            .spine
            .iter()
            .position(|item| href_matches_spine(href, item.href))
            .map(|index| index as i16)
            .unwrap_or(-1);
        if self
            .library
            .push_toc_record(title, href, level, spine_index)
        {
            Ok(())
        } else {
            Err(TocError::TooManyItems)
        }
    }
}

impl ReaderCacheScratch {
    #[allow(clippy::large_stack_arrays)]
    pub(crate) fn new() -> Self {
        Self {
            tail: [0; 4096],
            header: [0; 46],
            name: [0; MAX_ENTRY_NAME_BYTES],
            compressed: [0; READER_COMPRESSED_SCRATCH],
            container: [0; 4096],
            opf: [0; READER_OPF_SCRATCH],
            css: [0; READER_CSS_SCRATCH],
            xhtml: [0; READER_XHTML_SCRATCH],
            zip_inflate: ZipInflateScratch::new(),
        }
    }
}

pub(crate) fn build_or_load_book_cache(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
    requested_chapter: u8,
    target_pages: usize,
    scratch: &mut ReaderCacheScratch,
) {
    esp_println::println!(
        "epub: cache open index {} chapter {} target {}",
        index,
        requested_chapter,
        target_pages
    );
    library.loaded_index = None;
    library.reader_status = BookLoadStatus::Loading;
    library.title.clear();
    library.author.clear();
    library.error.clear();
    library.clear_toc();
    library.clear_lines();

    if index >= library.count {
        set_preview_error(library, "BAD INDEX");
        library.reader_status = BookLoadStatus::Error;
        return;
    }

    epd.deselect_display();
    sd_cs.set_high();
    epd.spi_mut().change_bus_frequency(400_u32.kHz());
    let startup_clocks = [0xFF; 10];
    if BlockingSpiBus::write(epd.spi_mut(), &startup_clocks).is_err() {
        epd.spi_mut().change_bus_frequency(40_u32.MHz());
        set_preview_error(library, "SPI CLOCKS");
        library.reader_status = BookLoadStatus::Error;
        return;
    }

    let status = 'open: {
        let spi = SdSpiDevice {
            spi: epd.spi_mut(),
            cs: sd_cs,
            delay: esp_hal::delay::Delay::new(),
        };
        let card = SdCard::new(spi, esp_hal::delay::Delay::new());
        if let Err(err) = card.num_bytes() {
            esp_println::println!("epub: card init failed: {:?}", err);
            set_preview_error(library, "CARD INIT");
            break 'open BookLoadStatus::Error;
        }
        card.spi(|device| device.spi.change_bus_frequency(8_u32.MHz()));

        let volume_mgr: VolumeManager<_, _, 4, 4, 1> = VolumeManager::new(card, StaticTime);
        let volume = match volume_mgr.open_volume(VolumeIdx(0)) {
            Ok(volume) => volume,
            Err(err) => {
                esp_println::println!("epub: open volume failed: {:?}", err);
                set_preview_error(library, "VOLUME");
                break 'open BookLoadStatus::Error;
            }
        };
        let root = match volume.open_root_dir() {
            Ok(root) => root,
            Err(err) => {
                esp_println::println!("epub: open root failed: {:?}", err);
                set_preview_error(library, "ROOT");
                break 'open BookLoadStatus::Error;
            }
        };
        let mut open_name = String::<16>::new();
        let mut display_name = String::<64>::new();
        let in_books_dir = library.entries[index].in_books_dir;
        let _ = open_name.push_str(&library.entries[index].open_name);
        let _ = display_name.push_str(&library.entries[index].display_name);

        let load_result = if in_books_dir {
            match root.open_dir("BOOKS") {
                Ok(books) => match books.open_file_in_dir(open_name.as_str(), Mode::ReadOnly) {
                    Ok(file) => build_or_load_epub_cache_from_file(
                        file,
                        &root,
                        &display_name,
                        requested_chapter,
                        target_pages,
                        library,
                        scratch,
                    ),
                    Err(err) => {
                        esp_println::println!("epub: open file failed: {:?}", err);
                        break 'open BookLoadStatus::Error;
                    }
                },
                Err(err) => {
                    esp_println::println!("epub: open /books failed: {:?}", err);
                    set_preview_error(library, "BOOKS DIR");
                    break 'open BookLoadStatus::Error;
                }
            }
        } else {
            match root.open_file_in_dir(open_name.as_str(), Mode::ReadOnly) {
                Ok(file) => build_or_load_epub_cache_from_file(
                    file,
                    &root,
                    &display_name,
                    requested_chapter,
                    target_pages,
                    library,
                    scratch,
                ),
                Err(err) => {
                    esp_println::println!("epub: open file failed: {:?}", err);
                    set_preview_error(library, "FILE");
                    break 'open BookLoadStatus::Error;
                }
            }
        };

        match load_result {
            Ok(()) => BookLoadStatus::Ready,
            Err(err) => {
                esp_println::println!("epub: load failed: {:?}", err);
                set_preview_error_from_error(library, err);
                BookLoadStatus::Error
            }
        }
    };

    epd.spi_mut().change_bus_frequency(40_u32.MHz());
    if matches!(status, BookLoadStatus::Ready | BookLoadStatus::Error) {
        library.loaded_index = Some(index);
        library.loaded_chapter = requested_chapter;
    }
    library.reader_status = status;
}

pub(crate) fn store_app_state(epd: &mut Epd, sd_cs: &mut Output<'static>, record: AppStateRecord) {
    epd.deselect_display();
    sd_cs.set_high();
    epd.spi_mut().change_bus_frequency(400_u32.kHz());
    let startup_clocks = [0xFF; 10];
    if BlockingSpiBus::write(epd.spi_mut(), &startup_clocks).is_err() {
        epd.spi_mut().change_bus_frequency(40_u32.MHz());
        return;
    }

    let spi = SdSpiDevice {
        spi: epd.spi_mut(),
        cs: sd_cs,
        delay: esp_hal::delay::Delay::new(),
    };
    let card = SdCard::new(spi, esp_hal::delay::Delay::new());
    if card.num_bytes().is_ok() {
        card.spi(|device| device.spi.change_bus_frequency(8_u32.MHz()));
        let volume_mgr: VolumeManager<_, _, 4, 4, 1> = VolumeManager::new(card, StaticTime);
        if let Ok(volume) = volume_mgr.open_volume(VolumeIdx(0)) {
            if let Ok(root) = volume.open_root_dir() {
                let _ = write_state_file(&root, record);
            }
        };
    }
    epd.spi_mut().change_bus_frequency(40_u32.MHz());
}

fn set_preview_error(library: &mut ReaderStore, message: &str) {
    library.error.clear();
    let _ = library.error.push_str(message);
}

fn set_preview_error_from_error(library: &mut ReaderStore, error: ReaderCacheError) {
    let message = match error {
        ReaderCacheError::Zip(proto::epub::ZipError::OutputTooSmall) => "EPUB TOO BIG",
        ReaderCacheError::Zip(proto::epub::ZipError::EntryBufferTooSmall) => "PATH LONG",
        ReaderCacheError::Zip(proto::epub::ZipError::UnsupportedCompression) => "ZIP METHOD",
        ReaderCacheError::Zip(proto::epub::ZipError::EntryNotFound) => "ZIP MISSING",
        ReaderCacheError::Zip(proto::epub::ZipError::Inflate) => "ZIP INFLATE",
        ReaderCacheError::Zip(_) => "ZIP",
        ReaderCacheError::Epub(proto::epub::EpubError::TooManyManifestItems) => "OPF MANIFEST",
        ReaderCacheError::Epub(proto::epub::EpubError::TooManySpineItems) => "OPF SPINE",
        ReaderCacheError::Epub(proto::epub::EpubError::MissingOpfPath) => "NO OPF",
        ReaderCacheError::Epub(proto::epub::EpubError::MissingOpf) => "NO OPF FILE",
        ReaderCacheError::Epub(proto::epub::EpubError::Utf8) => "OPF UTF8",
        ReaderCacheError::Epub(proto::epub::EpubError::Zip(_)) => "OPF ZIP",
        ReaderCacheError::Epub(_) => "OPF",
        ReaderCacheError::Xhtml(proto::epub::XhtmlError::TooManyRuns) => "TEXT FULL",
        ReaderCacheError::Utf8 => "UTF8",
        ReaderCacheError::MissingOpfPath => "NO OPF",
        ReaderCacheError::MissingSpine => "NO SPINE",
        ReaderCacheError::NoBodyText => "NO BODY TEXT",
        ReaderCacheError::EntryNameTooLong => "PATH LONG",
    };
    set_preview_error(library, message);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReaderCacheError {
    Zip(proto::epub::ZipError),
    Epub(proto::epub::EpubError),
    Xhtml(proto::epub::XhtmlError),
    Utf8,
    MissingOpfPath,
    MissingSpine,
    NoBodyText,
    EntryNameTooLong,
}

impl From<proto::epub::ZipError> for ReaderCacheError {
    fn from(value: proto::epub::ZipError) -> Self {
        Self::Zip(value)
    }
}

impl From<proto::epub::EpubError> for ReaderCacheError {
    fn from(value: proto::epub::EpubError) -> Self {
        Self::Epub(value)
    }
}

impl From<proto::epub::XhtmlError> for ReaderCacheError {
    fn from(value: proto::epub::XhtmlError) -> Self {
        Self::Xhtml(value)
    }
}

fn build_or_load_epub_cache_from_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_path: &str,
    requested_chapter: u8,
    target_pages: usize,
    library: &mut ReaderStore,
    scratch: &mut ReaderCacheScratch,
) -> Result<(), ReaderCacheError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let open_started = Instant::now();
    let source_len = file.length();
    let cache_key = cache_key_for(source_path, source_len);
    library.cache_key.clear();
    let _ = library.cache_key.push_str(cache_key.as_str());

    let reader = SdFileReadAt { file };
    let mut zip = ZipStream::new(reader, &mut scratch.tail)?;
    esp_println::println!(
        "epub: zip ready after {} ms",
        open_started.elapsed().as_millis()
    );

    let container_entry = zip.find_entry(
        "META-INF/container.xml",
        &mut scratch.header,
        &mut scratch.name,
    )?;
    let container_len = zip.read_entry_streamed(
        container_entry,
        &mut scratch.compressed,
        &mut scratch.container,
        &mut scratch.zip_inflate,
    )?;
    let container_xml = core::str::from_utf8(&scratch.container[..container_len])
        .map_err(|_| ReaderCacheError::Utf8)?;
    let opf_path = find_full_path(container_xml).ok_or(ReaderCacheError::MissingOpfPath)?;

    let opf_entry = zip.find_entry(opf_path, &mut scratch.header, &mut scratch.name)?;
    esp_println::println!(
        "epub: opf compressed={} uncompressed={}",
        opf_entry.compressed_size,
        opf_entry.uncompressed_size
    );
    let opf_len = zip.read_entry_streamed(
        opf_entry,
        &mut scratch.compressed,
        &mut scratch.opf,
        &mut scratch.zip_inflate,
    )?;
    let opf_xml =
        core::str::from_utf8(&scratch.opf[..opf_len]).map_err(|_| ReaderCacheError::Utf8)?;
    let package = parse_opf(opf_xml, BookId(2), source_path, 0, opf_path)?;
    esp_println::println!(
        "epub: opf parsed after {} ms",
        open_started.elapsed().as_millis()
    );

    copy_string(&mut library.title, package.meta.title);
    copy_string(&mut library.author, package.meta.author);
    load_epub_toc(
        &mut zip,
        opf_path,
        &package,
        library,
        TocScratch {
            header: &mut scratch.header,
            name: &mut scratch.name,
            compressed: &mut scratch.compressed,
            xhtml: &mut scratch.xhtml,
            zip_inflate: &mut scratch.zip_inflate,
        },
    );
    esp_println::println!(
        "epub: toc parsed after {} ms ({} item(s))",
        open_started.elapsed().as_millis(),
        library.toc_count
    );

    let mut css_rules = CssRules::new();
    load_css_rules(
        &mut zip,
        opf_path,
        &package,
        CssScratch {
            header: &mut scratch.header,
            name: &mut scratch.name,
            compressed: &mut scratch.compressed,
            css: &mut scratch.css,
            zip_inflate: &mut scratch.zip_inflate,
        },
        &mut css_rules,
    );
    esp_println::println!(
        "epub: css parsed after {} ms ({} rule(s))",
        open_started.elapsed().as_millis(),
        css_rules.rules.len()
    );

    let mut xhtml_path = String::<MAX_ENTRY_NAME_BYTES>::new();
    let mut saw_spine = false;
    let mut section_incomplete = false;
    let start_spine = requested_start_spine(&package, library, requested_chapter);
    let _ = ensure_cache_dirs(root, cache_key.as_str());
    write_book_cache(root, cache_key.as_str(), &package, library);
    if let Some(cached_pages) =
        load_section_cache(root, cache_key.as_str(), start_spine as u16, library)
    {
        if cached_pages >= target_pages {
            reader_layout::rebuild_toc_page_targets(library);
            esp_println::println!(
                "epub: section cache hit after {} ms ({} page(s), spine {})",
                open_started.elapsed().as_millis(),
                library.page_count,
                start_spine
            );
            return Ok(());
        }
        library.clear_lines();
    }

    for (spine_index, spine) in package
        .spine
        .iter()
        .enumerate()
        .skip(start_spine)
        .filter(|(_, item)| !item.href.is_empty() && !spine_item_is_navigation(item, &package))
    {
        saw_spine = true;
        resolve_epub_href(opf_path, spine.href, &mut xhtml_path)?;
        let Ok(xhtml_entry) = zip.find_entry(&xhtml_path, &mut scratch.header, &mut scratch.name)
        else {
            continue;
        };
        esp_println::println!(
            "epub: spine {} compressed={} uncompressed={}",
            xhtml_path.as_str(),
            xhtml_entry.compressed_size,
            xhtml_entry.uncompressed_size
        );
        let (xhtml_len, xhtml_complete) = zip.read_entry_prefix_streamed(
            xhtml_entry,
            &mut scratch.compressed,
            &mut scratch.xhtml,
            &mut scratch.zip_inflate,
        )?;
        let xhtml_len = valid_utf8_prefix_len(&scratch.xhtml[..xhtml_len], xhtml_complete)?;
        section_incomplete |= !xhtml_complete;
        let xhtml = core::str::from_utf8(&scratch.xhtml[..xhtml_len])
            .map_err(|_| ReaderCacheError::Utf8)?;
        if library.block_count > 0 {
            library.force_next_block_to_new_page();
        }
        let target_pages = target_pages.max(1).min(library.pages.len());
        let mut sink = LibraryBlockSink {
            library,
            spine_index: spine_index.min(u16::MAX as usize) as u16,
            line: String::new(),
            line_role: TextRole::Body,
            line_align: TextAlign::Justify,
            line_style: FontStyle::Regular,
            pending_space: false,
            dropping_paragraph: false,
            stopped: false,
            target_pages,
        };
        xhtml_blocks_to_sink(xhtml, Some(&css_rules), &mut sink)?;
        let stopped = sink.stopped;
        reader_layout::rebuild_page_index(library, 22, 472);
        if library.block_count >= library.blocks.len().saturating_sub(4) {
            break;
        }
        if !xhtml_complete || stopped || library.page_count >= target_pages {
            break;
        }
    }

    if library.block_count > 0 {
        reader_layout::rebuild_page_index(library, 22, 472);
        reader_layout::rebuild_toc_page_targets(library);
        library.cached_spine = start_spine.min(u16::MAX as usize) as u16;
        library.section_partial = section_incomplete
            || library.page_count >= target_pages
            || library.block_count >= library.blocks.len().saturating_sub(4);
        write_section_cache(root, cache_key.as_str(), library.cached_spine, library);
        esp_println::println!(
            "epub: initial cache ready after {} ms ({} page(s), {} block(s), key {})",
            open_started.elapsed().as_millis(),
            library.page_count,
            library.block_count,
            cache_key.as_str()
        );
        Ok(())
    } else if saw_spine {
        Err(ReaderCacheError::NoBodyText)
    } else {
        Err(ReaderCacheError::MissingSpine)
    }
}

fn spine_item_is_navigation(
    item: &proto::epub::SpineItem<'_>,
    package: &proto::epub::EpubPackage<'_>,
) -> bool {
    let lower_href = LowerAscii::<160>::new(item.href);
    let lower_props = LowerAscii::<96>::new(item.properties);
    item.media_type == "application/x-dtbncx+xml"
        || package
            .nav_href
            .map(|href| href == item.href)
            .unwrap_or(false)
        || package
            .ncx_href
            .map(|href| href == item.href)
            .unwrap_or(false)
        || lower_props.word_eq("nav")
        || lower_href.ends_with("toc.xhtml")
        || lower_href.ends_with("toc.html")
        || lower_href.ends_with("nav.xhtml")
        || lower_href.ends_with("nav.html")
}

fn requested_start_spine(
    package: &proto::epub::EpubPackage<'_>,
    library: &ReaderStore,
    requested_chapter: u8,
) -> usize {
    if requested_chapter > 0 {
        if let Some(record) = library.toc.get(requested_chapter as usize) {
            if record.spine_index >= 0 {
                return (record.spine_index as usize).min(package.spine.len().saturating_sub(1));
            }
        }
    }
    package
        .text_reference_href
        .and_then(|href| {
            package
                .spine
                .iter()
                .position(|item| strip_fragment(item.href) == href)
        })
        .unwrap_or(0)
}

fn valid_utf8_prefix_len(bytes: &[u8], complete: bool) -> Result<usize, ReaderCacheError> {
    match core::str::from_utf8(bytes) {
        Ok(_) => Ok(bytes.len()),
        Err(err) if !complete && err.valid_up_to() > 0 => Ok(err.valid_up_to()),
        Err(_) => Err(ReaderCacheError::Utf8),
    }
}

fn load_css_rules<R>(
    zip: &mut ZipStream<R>,
    opf_path: &str,
    package: &proto::epub::EpubPackage<'_>,
    scratch: CssScratch<'_>,
    rules: &mut CssRules,
) where
    R: ReadAt,
{
    rules.clear();
    let mut css_path = String::<MAX_ENTRY_NAME_BYTES>::new();
    for item in package
        .manifest
        .iter()
        .filter(|item| item.media_type.contains("css") || item.href.ends_with(".css"))
    {
        if resolve_epub_href(opf_path, item.href, &mut css_path).is_err() {
            continue;
        }
        let Ok(css_entry) = zip.find_entry(&css_path, &mut *scratch.header, &mut *scratch.name)
        else {
            continue;
        };
        let Ok(css_len) = zip.read_entry_streamed(
            css_entry,
            &mut *scratch.compressed,
            &mut *scratch.css,
            &mut *scratch.zip_inflate,
        ) else {
            continue;
        };
        let Ok(css_text) = core::str::from_utf8(&scratch.css[..css_len]) else {
            continue;
        };
        parse_css_text_align(css_text, rules);
    }
}

fn load_epub_toc<R>(
    zip: &mut ZipStream<R>,
    opf_path: &str,
    package: &proto::epub::EpubPackage<'_>,
    library: &mut ReaderStore,
    scratch: TocScratch<'_>,
) where
    R: ReadAt,
{
    library.clear_toc();
    let Some(toc_href) = package.nav_href.or(package.ncx_href) else {
        return;
    };
    let mut toc_path = String::<MAX_ENTRY_NAME_BYTES>::new();
    if resolve_epub_href(opf_path, toc_href, &mut toc_path).is_err() {
        return;
    }
    let Ok(toc_entry) = zip.find_entry(&toc_path, scratch.header, scratch.name) else {
        return;
    };
    let Ok(toc_len) = zip.read_entry_streamed(
        toc_entry,
        scratch.compressed,
        scratch.xhtml,
        &mut *scratch.zip_inflate,
    ) else {
        return;
    };
    let Ok(toc_text) = core::str::from_utf8(&scratch.xhtml[..toc_len]) else {
        return;
    };

    let mut sink = LibraryTocSink { library, package };
    if package.nav_href == Some(toc_href) {
        let _ = parse_epub3_nav_to_sink(toc_text, &mut sink);
    } else {
        let _ = parse_epub2_ncx_to_sink(toc_text, &mut sink);
    }
}

fn ensure_cache_dirs<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    let cache = open_or_make_dir(&xteink, CACHE_DIR)?;
    let book = open_or_make_dir(&cache, key)?;
    let _ = open_or_make_dir(&book, CACHE_SECTIONS_DIR)?;
    Ok(())
}

fn write_state_file<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record: AppStateRecord,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    let file = xteink
        .open_file_in_dir(STATE_FILE, Mode::ReadWriteCreateOrTruncate)
        .map_err(|_| ())?;
    file.write(&record.encode()).map_err(|_| ())
}

fn open_or_make_dir<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    parent: &'a Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> Result<Directory<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match parent.open_dir(name) {
        Ok(dir) => Ok(dir),
        Err(_) => {
            let _ = parent.make_dir_in_dir(name);
            parent.open_dir(name).map_err(|_| ())
        }
    }
}

fn write_book_cache<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    package: &proto::epub::EpubPackage<'_>,
    library: &ReaderStore,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return;
    };
    let Ok(cache) = xteink.open_dir(CACHE_DIR) else {
        return;
    };
    let Ok(book_dir) = cache.open_dir(key) else {
        return;
    };
    let Ok(file) = book_dir.open_file_in_dir(CACHE_BOOK_FILE, Mode::ReadWriteCreateOrTruncate)
    else {
        return;
    };
    let string_bytes = book_string_bytes(package, library);
    let header = BookCacheHeader {
        spine_count: package.spine.len().min(u16::MAX as usize) as u16,
        toc_count: library.toc_count.min(u16::MAX as usize) as u16,
        string_bytes,
    };
    let mut record = [0u8; TOC_RECORD_BYTES];
    if encode_book_header(header, &mut record[..BOOK_HEADER_BYTES]).is_err()
        || file.write(&record[..BOOK_HEADER_BYTES]).is_err()
    {
        return;
    }

    let mut offset = book_meta_string_bytes(package, library);
    for (spine_index, spine) in package.spine.iter().enumerate() {
        let href_len = spine.href.len().min(u16::MAX as usize) as u16;
        let toc_index = library
            .toc
            .iter()
            .take(library.toc_count)
            .position(|toc| toc.spine_index == spine_index as i16)
            .map(|index| index as i16)
            .unwrap_or(-1);
        let spine_record = SpineRecord {
            href_offset: offset,
            href_len,
            toc_index,
            byte_size: 0,
        };
        if encode_spine(spine_record, &mut record[..SPINE_RECORD_BYTES]).is_err()
            || file.write(&record[..SPINE_RECORD_BYTES]).is_err()
        {
            return;
        }
        offset = offset.saturating_add(href_len as u32);
    }

    for toc in library.toc.iter().take(library.toc_count).copied() {
        let title_offset = offset;
        offset = offset.saturating_add(toc.title_len as u32);
        let href_offset = offset;
        offset = offset.saturating_add(toc.href_len as u32);
        let cache_toc = CacheTocRecord {
            title_offset,
            title_len: toc.title_len,
            href_offset,
            href_len: toc.href_len,
            anchor_offset: 0,
            anchor_len: 0,
            level: toc.level,
            spine_index: toc.spine_index,
        };
        if encode_toc(cache_toc, &mut record[..TOC_RECORD_BYTES]).is_err()
            || file.write(&record[..TOC_RECORD_BYTES]).is_err()
        {
            return;
        }
    }

    let _ = file.write(package.meta.title.as_bytes());
    let _ = file.write(&[0]);
    let _ = file.write(package.meta.author.as_bytes());
    let _ = file.write(&[0]);
    let _ = file.write(package.meta.source_path.as_bytes());
    let _ = file.write(&[0]);
    for spine in package.spine.iter() {
        let _ = file.write(spine.href.as_bytes());
    }
    for index in 0..library.toc_count {
        let _ = file.write(library.toc_title(index).as_bytes());
        let href = library.toc_href(index);
        let _ = file.write(href.as_bytes());
    }
}

fn load_section_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    spine: u16,
    library: &mut ReaderStore,
) -> Option<usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_DIR).ok()?;
    let book_dir = cache.open_dir(key).ok()?;
    let sections = book_dir.open_dir(CACHE_SECTIONS_DIR).ok()?;
    let mut name = String::<12>::new();
    section_file_name(spine, &mut name);
    let file = sections
        .open_file_in_dir(name.as_str(), Mode::ReadOnly)
        .ok()?;
    let mut header_bytes = [0u8; SECTION_HEADER_BYTES];
    read_exact_file(&file, &mut header_bytes).ok()?;
    let header = decode_section_header(&header_bytes).ok()?;
    let page_count = header.page_count as usize;
    let block_count = header.block_count as usize;
    let text_bytes = header.text_bytes as usize;
    if page_count > library.pages.len()
        || block_count > library.blocks.len()
        || text_bytes > library.text.len()
    {
        return None;
    }

    let mut record_bytes = [0u8; 16];
    for index in 0..page_count {
        read_exact_file(&file, &mut record_bytes[..PAGE_RECORD_BYTES]).ok()?;
        library.pages[index] = decode_page(&record_bytes[..PAGE_RECORD_BYTES]).ok()?;
        library.page_spine[index] = spine;
    }
    for index in 0..block_count {
        read_exact_file(&file, &mut record_bytes[..BLOCK_RECORD_BYTES]).ok()?;
        let block = decode_block(&record_bytes[..BLOCK_RECORD_BYTES]).ok()?;
        library.blocks[index] = block;
        library.block_styles[index] = display_style_for_proto_style(block.style);
        library.block_spine[index] = spine;
    }
    for index in 0..block_count {
        let mut flag = [0u8; 1];
        read_exact_file(&file, &mut flag).ok()?;
        library.block_paragraph_end[index] = flag[0] != 0;
    }
    read_exact_file(&file, &mut library.text[..text_bytes]).ok()?;
    library.page_count = page_count;
    library.block_count = block_count;
    library.text_len = text_bytes;
    library.cached_spine = spine;
    library.section_partial = header.partial;
    Some(page_count)
}

fn book_string_bytes(package: &proto::epub::EpubPackage<'_>, library: &ReaderStore) -> u32 {
    let mut total = book_meta_string_bytes(package, library);
    for spine in package.spine.iter() {
        total = total.saturating_add(spine.href.len().min(u16::MAX as usize) as u32);
    }
    for index in 0..library.toc_count {
        total = total.saturating_add(library.toc_title(index).len().min(u16::MAX as usize) as u32);
        total = total.saturating_add(library.toc_href(index).len().min(u16::MAX as usize) as u32);
    }
    total
}

fn book_meta_string_bytes(package: &proto::epub::EpubPackage<'_>, _library: &ReaderStore) -> u32 {
    package.meta.title.len().saturating_add(1) as u32
        + package.meta.author.len().saturating_add(1) as u32
        + package.meta.source_path.len().saturating_add(1) as u32
}

fn write_section_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    spine: u16,
    library: &ReaderStore,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return;
    };
    let Ok(cache) = xteink.open_dir(CACHE_DIR) else {
        return;
    };
    let Ok(book_dir) = cache.open_dir(key) else {
        return;
    };
    let Ok(sections) = book_dir.open_dir(CACHE_SECTIONS_DIR) else {
        return;
    };
    let mut name = String::<12>::new();
    section_file_name(spine, &mut name);
    let Ok(file) = sections.open_file_in_dir(name.as_str(), Mode::ReadWriteCreateOrTruncate) else {
        return;
    };
    let header = SectionHeader {
        page_count: library.page_count.min(u16::MAX as usize) as u16,
        block_count: library.block_count.min(u16::MAX as usize) as u16,
        line_count: 0,
        word_count: 0,
        text_bytes: library.text_len.min(u32::MAX as usize) as u32,
        viewport_width: 800,
        viewport_height: 480,
        font_config: 1,
        bytes_consumed: 0,
        total_bytes: 0,
        partial: library.section_partial,
    };
    let mut bytes = [0u8; SECTION_HEADER_BYTES];
    if encode_section_header(header, &mut bytes).is_err() || file.write(&bytes).is_err() {
        return;
    }
    let mut record = [0u8; 16];
    for page in library.pages.iter().take(library.page_count) {
        if encode_page(*page, &mut record[..PAGE_RECORD_BYTES]).is_err()
            || file.write(&record[..PAGE_RECORD_BYTES]).is_err()
        {
            return;
        }
    }
    for block in library.blocks.iter().take(library.block_count) {
        if encode_block(*block, &mut record[..BLOCK_RECORD_BYTES]).is_err()
            || file.write(&record[..BLOCK_RECORD_BYTES]).is_err()
        {
            return;
        }
    }
    for flag in library
        .block_paragraph_end
        .iter()
        .take(library.block_count)
        .copied()
    {
        if file.write(&[flag as u8]).is_err() {
            return;
        }
    }
    let _ = file.write(&library.text[..library.text_len]);
}

fn read_exact_file<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    mut out: &mut [u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    while !out.is_empty() {
        let read = file.read(out).map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        let tmp = out;
        out = &mut tmp[read..];
    }
    Ok(())
}

fn cache_key_for(source_path: &str, source_len: u32) -> String<8> {
    let mut hash = 0x811c_9dc5u32;
    for byte in source_path.bytes().chain(source_len.to_le_bytes()) {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let mut out = String::<8>::new();
    let _ = out.push('E');
    push_hex(&mut out, hash, 7);
    out
}

fn section_file_name(spine: u16, out: &mut String<12>) {
    out.clear();
    let _ = out.push('S');
    push_dec3(out, spine);
    let _ = out.push_str(".BIN");
}

fn push_hex<const N: usize>(out: &mut String<N>, value: u32, digits: u8) {
    for shift in (0..digits).rev() {
        let nibble = ((value >> (shift * 4)) & 0x0F) as u8;
        let ch = if nibble < 10 {
            b'0' + nibble
        } else {
            b'A' + nibble - 10
        };
        let _ = out.push(ch as char);
    }
}

fn push_dec3<const N: usize>(out: &mut String<N>, value: u16) {
    let value = value.min(999);
    let _ = out.push((b'0' + ((value / 100) % 10) as u8) as char);
    let _ = out.push((b'0' + ((value / 10) % 10) as u8) as char);
    let _ = out.push((b'0' + (value % 10) as u8) as char);
}

struct SdFileReadAt<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> ReadAt
    for SdFileReadAt<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    type Error = embedded_sdmmc::Error<D::Error>;

    fn len(&mut self) -> Result<u32, Self::Error> {
        Ok(self.file.length())
    }

    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error> {
        self.file.seek_from_start(offset)?;
        self.file.read(out)
    }
}

fn find_full_path(xml: &str) -> Option<&str> {
    let key = "full-path";
    let start = xml.find(key)?;
    let after_key = &xml[start + key.len()..];
    let equals = after_key.find('=')?;
    let after_equals = after_key[equals + 1..].trim_start();
    let quote = after_equals.as_bytes().first().copied()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let value = &after_equals[1..];
    let end = value.as_bytes().iter().position(|byte| *byte == quote)?;
    Some(&value[..end])
}

fn resolve_epub_href(
    opf_path: &str,
    href: &str,
    out: &mut String<MAX_ENTRY_NAME_BYTES>,
) -> Result<(), ReaderCacheError> {
    out.clear();
    if href.starts_with('/') {
        out.push_str(href.trim_start_matches('/'))
            .map_err(|_| ReaderCacheError::EntryNameTooLong)?;
        return Ok(());
    }
    if let Some((dir, _)) = opf_path.rsplit_once('/') {
        out.push_str(dir)
            .and_then(|_| out.push('/'))
            .map_err(|_| ReaderCacheError::EntryNameTooLong)?;
    }
    let href_no_fragment = href.split('#').next().unwrap_or(href);
    out.push_str(href_no_fragment)
        .map_err(|_| ReaderCacheError::EntryNameTooLong)
}

fn strip_fragment(value: &str) -> &str {
    value.split('#').next().unwrap_or(value)
}

fn href_matches_spine(toc_href: &str, spine_href: &str) -> bool {
    let toc_href = strip_fragment(toc_href);
    toc_href == spine_href
        || toc_href.ends_with(spine_href)
        || spine_href.ends_with(toc_href.rsplit('/').next().unwrap_or(toc_href))
}

fn copy_string<const N: usize>(out: &mut String<N>, value: &str) {
    out.clear();
    for ch in value.chars() {
        if out.push(ch).is_err() {
            break;
        }
    }
}

struct LibraryBlockSink<'a> {
    library: &'a mut ReaderStore,
    spine_index: u16,
    line: String<MAX_READER_BLOCK_TEXT>,
    line_role: TextRole,
    line_align: TextAlign,
    line_style: FontStyle,
    pending_space: bool,
    dropping_paragraph: bool,
    stopped: bool,
    target_pages: usize,
}

impl XhtmlBlockSink for LibraryBlockSink<'_> {
    fn push_block(
        &mut self,
        text: &str,
        role: TextRole,
        style: proto::text::FontStyle,
        align: TextAlign,
        paragraph_end: bool,
    ) -> Result<(), XhtmlError> {
        if self.stopped {
            return Ok(());
        }
        if self.library.block_count >= self.library.blocks.len() {
            return Ok(());
        }
        push_styled_preview_fragment(
            self,
            text,
            preview_style_for_proto_style(style, role),
            role,
            align,
            paragraph_end,
        );
        reader_layout::rebuild_page_index(self.library, 22, 472);
        self.stopped = self.library.page_count >= self.target_pages
            || self.library.block_count >= self.library.blocks.len().saturating_sub(4);
        Ok(())
    }
}

fn preview_style_for_proto_style(style: proto::text::FontStyle, role: TextRole) -> FontStyle {
    match style {
        proto::text::FontStyle::BoldItalic => FontStyle::BoldItalic,
        proto::text::FontStyle::Bold => FontStyle::Bold,
        proto::text::FontStyle::Italic => FontStyle::Italic,
        proto::text::FontStyle::Regular => {
            if matches!(
                role,
                TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
            ) {
                FontStyle::Bold
            } else {
                FontStyle::Regular
            }
        }
    }
}

fn display_style_for_proto_style(style: proto::text::FontStyle) -> FontStyle {
    match style {
        proto::text::FontStyle::BoldItalic => FontStyle::BoldItalic,
        proto::text::FontStyle::Bold => FontStyle::Bold,
        proto::text::FontStyle::Italic => FontStyle::Italic,
        proto::text::FontStyle::Regular => FontStyle::Regular,
    }
}

fn push_styled_preview_fragment(
    sink: &mut LibraryBlockSink<'_>,
    text: &str,
    style: FontStyle,
    role: TextRole,
    align: TextAlign,
    paragraph_end: bool,
) {
    if sink.dropping_paragraph {
        if paragraph_end {
            sink.dropping_paragraph = false;
            sink.pending_space = false;
        }
        return;
    }
    let starts_with_space = text
        .chars()
        .next()
        .map(|ch| ch.is_whitespace())
        .unwrap_or(false);
    let ends_with_space = text
        .chars()
        .next_back()
        .map(|ch| ch.is_whitespace())
        .unwrap_or(false);
    let mut normalized = String::<MAX_READER_BLOCK_TEXT>::new();
    push_normalized_decoded(text, &mut normalized);
    trim_trailing_space(&mut normalized);
    if !sanitize_preview_block(&mut normalized) {
        sink.dropping_paragraph = !paragraph_end;
        sink.pending_space = false;
        return;
    }
    if normalized.is_empty() {
        sink.pending_space |= starts_with_space || ends_with_space;
        if paragraph_end {
            flush_styled_preview_line(sink, true);
        }
        return;
    }

    normalize_decorative_separator(&mut normalized);
    let align = block_align_for(align, normalized.as_str(), role);
    let font = literata(style);
    let x = reader_layout::reader_x_for(role);
    let max_x = reader_layout::reader_max_x_for(role, align);

    if !sink.line.is_empty() && (sink.line_role != role || sink.line_align != align) {
        flush_styled_preview_line(sink, false);
    }
    if sink.line.is_empty() {
        sink.line_role = role;
        sink.line_align = align;
        sink.line_style = FontStyle::Regular;
    }

    let mut first_word = true;
    for word in normalized.split_whitespace() {
        let attach = is_leading_punctuation_word(word) && !sink.line.is_empty();
        let mut candidate = sink.line.clone();
        let leading_space = !sink.line.is_empty()
            && !attach
            && (sink.pending_space || !first_word || starts_with_space);
        if append_styled_word(&mut candidate, word, style, leading_space).is_err() {
            flush_styled_preview_line(sink, false);
            let _ = append_styled_word(&mut sink.line, word, style, false);
            sink.line_role = role;
            sink.line_align = align;
            sink.line_style = style;
            sink.pending_space = false;
            first_word = false;
            continue;
        }

        if !sink.line.is_empty()
            && reader_layout::styled_text_ink_width(candidate.as_str(), font)
                + x
                + reader_layout::READER_WRAP_SAFETY
                > max_x
        {
            flush_styled_preview_line(sink, false);
            let _ = append_styled_word(&mut sink.line, word, style, false);
            sink.line_role = role;
            sink.line_align = align;
            sink.line_style = style;
            sink.pending_space = false;
        } else {
            sink.line = candidate;
            sink.line_role = role;
            sink.line_align = align;
            sink.line_style = style;
            sink.pending_space = false;
        }
        first_word = false;
    }

    sink.pending_space |= ends_with_space;
    if paragraph_end {
        flush_styled_preview_line(sink, true);
    }
}

fn append_styled_word<const N: usize>(
    line: &mut String<N>,
    word: &str,
    style: FontStyle,
    leading_space: bool,
) -> Result<(), ()> {
    if leading_space {
        line.push(' ').map_err(|_| ())?;
    }
    append_style_marker(line, style)?;
    line.push_str(word).map_err(|_| ())
}

fn append_style_marker<const N: usize>(line: &mut String<N>, style: FontStyle) -> Result<(), ()> {
    line.push(reader_layout::STYLE_MARKER).map_err(|_| ())?;
    line.push(reader_layout::style_marker_code(style))
        .map_err(|_| ())
}

fn flush_styled_preview_line(sink: &mut LibraryBlockSink<'_>, paragraph_end: bool) {
    if sink.line.is_empty() {
        if paragraph_end && sink.library.block_count > 0 {
            sink.library.block_paragraph_end[sink.library.block_count - 1] = true;
        }
        return;
    }

    let line = sink.line.clone();
    let role = sink.line_role;
    let align = sink.line_align;
    let style = reader_layout::first_styled_line_style(line.as_str()).unwrap_or(FontStyle::Regular);
    let _ = push_preview_line_record(
        line.as_str(),
        style,
        role,
        align,
        paragraph_end,
        sink.spine_index,
        sink.library,
    );
    sink.line.clear();
    sink.line_style = FontStyle::Regular;
    sink.pending_space = false;
}

fn is_leading_punctuation_word(word: &str) -> bool {
    word.chars()
        .next()
        .map(|ch| {
            matches!(
                ch,
                ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\u{2019}' | '\u{201D}'
            )
        })
        .unwrap_or(false)
}

fn push_preview_line_record(
    line: &str,
    style: FontStyle,
    role: TextRole,
    align: TextAlign,
    paragraph_end: bool,
    spine_index: u16,
    library: &mut ReaderStore,
) -> bool {
    let line = line.trim();
    if line.is_empty() || library.block_count >= library.blocks.len() {
        return true;
    }
    let start = library.text_len;
    let bytes = line.as_bytes();
    if start + bytes.len() > library.text.len() || bytes.len() > u16::MAX as usize {
        return false;
    }
    library.text[start..start + bytes.len()].copy_from_slice(bytes);
    library.text_len += bytes.len();
    library.blocks[library.block_count] = BlockRecord {
        text_offset: start as u32,
        text_len: bytes.len() as u16,
        line_count: 1,
        role,
        style: proto_style_for_display_style(style),
        align,
    };
    library.block_styles[library.block_count] = style;
    library.block_spine[library.block_count] = spine_index;
    library.block_paragraph_end[library.block_count] = paragraph_end;
    library.block_count += 1;
    true
}

fn proto_style_for_display_style(style: FontStyle) -> proto::text::FontStyle {
    match style {
        FontStyle::Regular => proto::text::FontStyle::Regular,
        FontStyle::Italic => proto::text::FontStyle::Italic,
        FontStyle::Bold => proto::text::FontStyle::Bold,
        FontStyle::BoldItalic => proto::text::FontStyle::BoldItalic,
    }
}

fn block_align_for(run_align: TextAlign, block: &str, role: TextRole) -> TextAlign {
    if run_align == TextAlign::Center
        || matches!(
            role,
            TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
        )
        || is_decorative_separator(block)
    {
        TextAlign::Center
    } else {
        run_align
    }
}

fn normalize_decorative_separator<const N: usize>(block: &mut String<N>) {
    if !is_decorative_separator(block.as_str()) {
        return;
    }
    block.clear();
    let _ = block.push_str("* * *");
}

fn is_decorative_separator(text: &str) -> bool {
    let mut saw_mark = false;
    let mut mark_count = 0u8;
    for ch in text.chars() {
        if ch == '*' {
            saw_mark = true;
            mark_count = mark_count.saturating_add(1);
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        return false;
    }
    saw_mark && mark_count >= 3
}

fn push_normalized_decoded<const N: usize>(text: &str, out: &mut String<N>) {
    let mut previous_space = true;
    let mut cursor = 0usize;
    while cursor < text.len() {
        let rest = &text[cursor..];
        if let Some(decoded) = decode_entity(rest) {
            if decoded.is_whitespace() {
                if !previous_space && out.push(' ').is_err() {
                    break;
                }
                previous_space = true;
            } else if push_normalized_char(decoded, out).is_err() {
                break;
            } else {
                previous_space = false;
            }
            cursor += rest.find(';').map(|index| index + 1).unwrap_or(1);
            continue;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            if !previous_space && out.push(' ').is_err() {
                break;
            }
            previous_space = true;
        } else if push_normalized_char(ch, out).is_err() {
            break;
        } else {
            previous_space = false;
        }
        cursor += ch.len_utf8();
    }
}

fn push_normalized_char<const N: usize>(ch: char, out: &mut String<N>) -> Result<(), ()> {
    match ch {
        '\u{00A0}' => out.push(' ').map_err(|_| ()),
        ch if ch as u32 <= u16::MAX as u32 => out.push(ch).map_err(|_| ()),
        _ => out.push('?').map_err(|_| ()),
    }
}

fn decode_entity(input: &str) -> Option<char> {
    if let Some(decoded) = decode_numeric_entity(input) {
        Some(decoded)
    } else if input.starts_with("&amp;") {
        Some('&')
    } else if input.starts_with("&lt;") {
        Some('<')
    } else if input.starts_with("&gt;") {
        Some('>')
    } else if input.starts_with("&quot;") {
        Some('"')
    } else if input.starts_with("&apos;") {
        Some('\'')
    } else if input.starts_with("&nbsp;") {
        Some(' ')
    } else if input.starts_with("&mdash;") {
        Some('—')
    } else if input.starts_with("&ndash;") {
        Some('–')
    } else if input.starts_with("&lsquo;") {
        Some('‘')
    } else if input.starts_with("&rsquo;") {
        Some('’')
    } else if input.starts_with("&ldquo;") {
        Some('“')
    } else if input.starts_with("&rdquo;") {
        Some('”')
    } else if input.starts_with("&hellip;") {
        Some('…')
    } else {
        None
    }
}

fn decode_numeric_entity(input: &str) -> Option<char> {
    let rest = input.strip_prefix("&#")?;
    let end = rest.find(';')?;
    let entity = &rest[..end];
    let value = if let Some(hex) = entity
        .strip_prefix('x')
        .or_else(|| entity.strip_prefix('X'))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        entity.parse::<u32>().ok()?
    };
    char::from_u32(value)
}

fn is_epub_titlepage_label(text: &str) -> bool {
    let lower = LowerAscii::<128>::new(text);
    lower.starts_with(": ")
        || lower.eq("title")
        || lower.eq("author")
        || lower.eq("creator")
        || lower.eq("language")
        || lower.eq("english")
        || lower.eq("english:")
        || lower.eq("release date")
        || lower.eq("original publication")
        || lower.starts_with("most recently updated")
        || lower.starts_with("other information")
        || lower.starts_with("other formats")
        || lower.starts_with("credits")
        || lower.starts_with("produced by")
        || lower.starts_with("transcribed from")
        || lower.starts_with("project gutenberg")
        || lower.starts_with("the project gutenberg")
}

fn sanitize_preview_block<const N: usize>(block: &mut String<N>) -> bool {
    trim_trailing_space(block);
    trim_leading_space(block);
    if block.is_empty() {
        return false;
    }
    if is_epub_titlepage_label(block) || contains_gutenberg_metadata(block.as_str()) {
        return false;
    }
    if is_decorative_separator(block.as_str()) {
        normalize_decorative_separator(block);
        return true;
    }
    if let Some(rest) = decorative_prefix_rest(block.as_str()) {
        if rest.is_empty() {
            normalize_decorative_separator(block);
            return true;
        }
        if is_epub_titlepage_label(rest) || contains_gutenberg_metadata(rest) {
            return false;
        }
    }
    true
}

fn decorative_prefix_rest(text: &str) -> Option<&str> {
    let mut mark_count = 0u8;
    let mut end = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == '*' {
            mark_count = mark_count.saturating_add(1);
            end = index + ch.len_utf8();
            continue;
        }
        if ch.is_whitespace() {
            end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    if mark_count >= 3 {
        Some(text[end..].trim())
    } else {
        None
    }
}

fn contains_gutenberg_metadata(text: &str) -> bool {
    let lower = LowerAscii::<160>::new(text);
    lower.contains("most recently updated")
        || lower.contains("project gutenberg ebook")
        || lower.contains("start of the project gutenberg")
        || lower.contains("end of the project gutenberg")
        || lower.contains("other information and formats")
        || lower.contains("this ebook is for the use of anyone")
        || lower.contains("project gutenberg license")
        || lower.contains("www.gutenberg.org")
        || lower.contains("laws of the country where you are located")
}

fn trim_trailing_space<const N: usize>(text: &mut String<N>) {
    while text.as_str().as_bytes().last().copied() == Some(b' ') {
        text.pop();
    }
}

fn trim_leading_space<const N: usize>(text: &mut String<N>) {
    let trim_len = text
        .as_str()
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    if trim_len == 0 {
        return;
    }
    let mut trimmed = String::<N>::new();
    let _ = trimmed.push_str(&text.as_str()[trim_len..]);
    *text = trimmed;
}

struct LowerAscii<const N: usize> {
    text: String<N>,
}

impl<const N: usize> LowerAscii<N> {
    fn new(input: &str) -> Self {
        let mut text = String::new();
        for byte in input.bytes() {
            if text.push((byte as char).to_ascii_lowercase()).is_err() {
                break;
            }
        }
        Self { text }
    }

    fn eq(&self, other: &str) -> bool {
        self.text.as_str() == other
    }

    fn starts_with(&self, other: &str) -> bool {
        self.text.as_str().starts_with(other)
    }

    fn ends_with(&self, other: &str) -> bool {
        self.text.as_str().ends_with(other)
    }

    fn contains(&self, other: &str) -> bool {
        self.text.as_str().contains(other)
    }

    fn word_eq(&self, other: &str) -> bool {
        self.text
            .as_str()
            .split_ascii_whitespace()
            .any(|word| word == other)
    }
}
