use crate::display_flush::Epd;
use crate::reader_store::{
    derive_catalog_label, source_hash, LibraryScanStatus, ReaderStore, LIBRARY_WINDOW,
};
use crate::sd_session;
use embassy_time::Instant;
use embedded_sdmmc::{Directory, File, LfnBuffer, Mode, TimeSource};
use esp_hal::gpio::Output;
use heapless::String;

const CATALOG_ROOT_DIR: &str = "XTEINK";
const CATALOG_FILE: &str = "CATALOG.BIN";
use proto::catalog::{
    catalog_identity_staged, catalog_record_identity, decode_catalog_record, encode_catalog_header,
    encode_catalog_record, encode_catalog_title, sort_catalog_identities, stage_catalog_identity,
    CatalogRecord, CATALOG_HEADER_BYTES, CATALOG_IDENTITY_BYTES, CATALOG_RECORD_BYTES,
    CATALOG_RECORD_TITLE_OFFSET, CATALOG_TITLE_BYTES,
};

#[inline(never)]
pub(crate) fn scan_books(epd: &mut Epd, sd_cs: &mut Output<'static>, library: &mut ReaderStore) {
    let start = Instant::now();
    esp_println::println!("sd: scan start");
    library.status = LibraryScanStatus::Scanning;

    let status = sd_session::with_root(epd, sd_cs, |root| {
        esp_println::println!("sd: card init begin");
        esp_println::println!("sd: open root");
        library.clear_catalog();
        library.status = LibraryScanStatus::Scanning;
        // The 16 KB section text arena doubles as the scan's staging and
        // identity scratch: a scan runs from the storage dispatcher (boot or
        // an explicit refresh), never while a page render is reading the
        // arena, and the section window is invalidated below so a stale page
        // can't be served from clobbered text afterwards.
        let scanned = write_catalog_streaming(root, &mut library.text);
        if let Ok(count) = scanned {
            if count > 0 {
                esp_println::println!("sd: catalog written, {} epub(s)", count);
                // Drop the cached data of books no longer on the card before
                // reloading the window: this is the one moment the full book
                // set is known and the catalog is fresh.
                sweep_orphan_caches(root, &mut library.text);
            }
        }
        // The arena held scan scratch, not section text: drop the resident
        // section (and any Chapters TOC window) so nothing renders from it.
        library.clear_lines();
        library.text_holds_toc = false;
        match scanned {
            Ok(0) => LibraryScanStatus::Empty,
            Ok(_) => {
                // Reload the header count + the first list window from the file
                // we just wrote, so the streaming readers and the store agree.
                let _ = read_catalog_window(root, library, 0);
                LibraryScanStatus::Ready
            }
            Err(()) => LibraryScanStatus::Error,
        }
    })
    .unwrap_or_else(|err| {
        esp_println::println!("sd: session failed: {:?}", err);
        LibraryScanStatus::Error
    });
    library.status = if status == LibraryScanStatus::Error && !library.catalog_is_empty() {
        LibraryScanStatus::Ready
    } else {
        status
    };
    esp_println::println!("sd: scan complete, {} epub(s)", library.catalog_count());
    esp_println::println!(
        "bench: storage_catalog action=scan status={:?} count={} elapsed_ms={} t_ms={}",
        library.status,
        library.catalog_count(),
        start.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
}

#[inline(never)]
pub(crate) fn load_catalog_cache(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
) -> bool {
    let start = Instant::now();
    esp_println::println!("sd: catalog cache load start");
    library.clear_catalog();
    // A valid header (even an empty catalog) counts as loaded; a missing or
    // wrong-version file returns false so the caller runs a fresh scan.
    let loaded = sd_session::with_root(epd, sd_cs, |root| {
        read_catalog_window(root, library, 0).is_ok()
    })
    .unwrap_or(false);
    library.status = if !loaded {
        LibraryScanStatus::NotScanned
    } else if library.catalog_is_empty() {
        LibraryScanStatus::Empty
    } else {
        LibraryScanStatus::Ready
    };
    if loaded {
        esp_println::println!(
            "sd: catalog cache loaded {} epub(s)",
            library.catalog_count()
        );
    } else {
        esp_println::println!("sd: catalog cache unavailable");
    }
    esp_println::println!(
        "bench: storage_catalog action=load ok={} count={} elapsed_ms={} t_ms={}",
        loaded,
        library.catalog_count(),
        start.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    loaded
}

/// Walk both book locations (card root and `/BOOKS`), invoking `visit` with
/// each EPUB's `(display_path, open_name, in_books_dir, byte_size)`.
fn walk_epubs<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    visit: &mut impl FnMut(&str, &str, bool, u32),
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    collect_epubs(root, "/", false, visit);
    if let Ok(books) = root.open_dir("BOOKS") {
        collect_epubs(&books, "/books/", true, visit);
    }
}

/// Write CATALOG.BIN from the card without ever holding the whole library in
/// RAM. embedded-sdmmc locks the volume across a directory walk, so records
/// cannot be written mid-iteration; instead one walk counts the books (for the
/// header), then each later walk stages the next batch of records into the
/// caller's scratch region (the idle 16 KB section arena -- ~105 records per
/// pass, so ordinary libraries take two walks total) and appends them once the
/// walk has returned. Each staged record also gets its display title (the EPUB
/// title cached at last open, or the upload label) resolved once here, so
/// Library window reads never probe per-book files again. Returns the book
/// count actually written.
fn write_catalog_streaming<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    scratch: &mut [u8],
) -> Result<u16, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let batch_capacity = scratch.len() / CATALOG_RECORD_BYTES;
    if batch_capacity == 0 {
        return Err(());
    }
    let xteink = open_or_make_dir(root, CATALOG_ROOT_DIR)?;
    let file = xteink
        .open_file_in_dir(CATALOG_FILE, Mode::ReadWriteCreateOrTruncate)
        .map_err(|_| ())?;

    let mut counted = 0usize;
    {
        let mut count = |_: &str, _: &str, _: bool, _: u32| counted += 1;
        walk_epubs(root, &mut count);
    }
    let count = counted.min(u16::MAX as usize) as u16;

    let mut header = [0u8; CATALOG_HEADER_BYTES];
    encode_catalog_header(count, &mut header);
    file.write(&header).map_err(|_| ())?;

    let total = count as usize;
    let mut cursor = 0usize;
    while cursor < total {
        let mut batch_len = 0usize;
        let mut seen = 0usize;
        {
            let mut collect = |path: &str, open_name: &str, in_books_dir: bool, byte_size: u32| {
                if seen >= cursor && batch_len < batch_capacity {
                    let at = batch_len * CATALOG_RECORD_BYTES;
                    let record: &mut [u8; CATALOG_RECORD_BYTES] = (&mut scratch
                        [at..at + CATALOG_RECORD_BYTES])
                        .try_into()
                        .expect("record slice is exactly one record");
                    encode_catalog_record(
                        record,
                        path,
                        open_name,
                        "",
                        in_books_dir,
                        byte_size,
                        source_hash(path, byte_size),
                    );
                    batch_len += 1;
                }
                seen += 1;
            };
            walk_epubs(root, &mut collect);
        }
        if batch_len == 0 {
            break;
        }
        // The walk has returned, so file opens are legal again: resolve each
        // staged record's title (cheap for the common case -- a dir open that
        // fails before any file read) and patch it in before the append.
        for index in 0..batch_len {
            let at = index * CATALOG_RECORD_BYTES;
            let record: &[u8; CATALOG_RECORD_BYTES] = (&scratch[at..at + CATALOG_RECORD_BYTES])
                .try_into()
                .expect("record slice is exactly one record");
            let decoded = decode_catalog_record(record);
            let mut title = String::<64>::new();
            if cached_title_label(root, &decoded, &mut title).is_some() {
                let mut field = [0u8; CATALOG_TITLE_BYTES];
                encode_catalog_title(title.as_str(), &mut field);
                scratch[at + CATALOG_RECORD_TITLE_OFFSET
                    ..at + CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES]
                    .copy_from_slice(&field);
            }
        }
        file.write(&scratch[..batch_len * CATALOG_RECORD_BYTES])
            .map_err(|_| ())?;
        cursor += batch_len;
    }
    Ok(count)
}

/// Open CATALOG.BIN read-only, validate its header, and hand the file plus its
/// book count to `f`. Keeps the directory and file handles alive across the
/// call so the borrowed `File` stays valid.
fn with_catalog_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
    R,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    f: impl FnOnce(&File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>, u16) -> Result<R, ()>,
) -> Result<R, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CATALOG_ROOT_DIR).map_err(|_| ())?;
    let file = xteink
        .open_file_in_dir(CATALOG_FILE, Mode::ReadOnly)
        .map_err(|_| ())?;
    let mut header = [0u8; CATALOG_HEADER_BYTES];
    read_exact_file(&file, &mut header)?;
    let count = proto::catalog::decode_catalog_header(&header).ok_or(())?;
    f(&file, count)
}

/// Load the list window `[start, start+LIBRARY_WINDOW)` from the card into the
/// store, and set the total book count from the header. O(1) seek to the start
/// record -- no scan.
fn read_catalog_window<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &mut ReaderStore,
    start: usize,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_catalog_file(root, |file, count| {
        library.set_catalog_total(count);
        library.begin_window(start);
        if start >= count as usize {
            return Ok(());
        }
        seek_to_record(file, start)?;
        let take = LIBRARY_WINDOW.min(count as usize - start);
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        for _ in 0..take {
            read_exact_file(file, &mut record)?;
            let decoded = decode_catalog_record(&record);
            // Prefer the title persisted in the record (resolved at scan,
            // refreshed at book open) over the file-stem label, so uploaded
            // books (8.3 names) read as their real titles. An empty title
            // falls back to the stem. No per-row file probes: window
            // crossings cost exactly the record reads.
            let label = (!decoded.title.is_empty()).then_some(decoded.title.as_str());
            library.push_window_entry(
                decoded.display_name.as_str(),
                decoded.open_name.as_str(),
                decoded.in_books_dir,
                decoded.byte_size,
                decoded.source_hash,
                label,
            );
        }
        Ok(())
    })
}

/// Read a single catalog record by absolute index.
fn read_catalog_record_at<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    index: usize,
) -> Option<CatalogRecord>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_catalog_file(root, |file, count| {
        if index >= count as usize {
            return Err(());
        }
        seek_to_record(file, index)?;
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        read_exact_file(file, &mut record)?;
        Ok(decode_catalog_record(&record))
    })
    .ok()
}

/// Find the catalog index of the book with the given (path-hash, byte-size).
/// Tries `hint` (last-known index) first so an unchanged catalog resolves in
/// one read; only a miss streams the whole file.
fn find_in_catalog<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_hash: u32,
    byte_size: u32,
    hint: Option<u16>,
) -> Option<u16>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if source_hash == 0 && byte_size == 0 {
        return None;
    }
    with_catalog_file(root, |file, count| {
        if let Some(h) = hint {
            if (h as usize) < count as usize {
                if let Ok((rh, rs)) = record_identity(file, h as usize) {
                    if rh == source_hash && rs == byte_size {
                        return Ok(Some(h));
                    }
                }
            }
        }
        seek_to_record(file, 0)?;
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        for index in 0..count as usize {
            read_exact_file(file, &mut record)?;
            let (rh, rs) = catalog_record_identity(&record);
            if rh == source_hash && rs == byte_size {
                return Ok(Some(index as u16));
            }
        }
        Ok(None)
    })
    .ok()
    .flatten()
}

fn record_identity<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    index: usize,
) -> Result<(u32, u32), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    seek_to_record(file, index)?;
    let mut record = [0u8; CATALOG_RECORD_BYTES];
    read_exact_file(file, &mut record)?;
    Ok(catalog_record_identity(&record))
}

fn seek_to_record<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    index: usize,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let offset = (CATALOG_HEADER_BYTES + index * CATALOG_RECORD_BYTES) as u32;
    file.seek_from_start(offset).map_err(|_| ())
}

/// The list label override for a catalog record, read into `title` in place,
/// in order of authority: the EPUB title saved in the book's cache when it was
/// last opened, then the readable filename stashed at upload (for uploads not
/// yet opened, whose 8.3 name is unreadable). Returns `None` (file-stem
/// fallback) when neither exists. Resolved once per book at scan time -- the
/// result persists in the record's title field, so window reads never call
/// this. Cheap for the common case -- each lookup is a dir open that fails
/// before any file read.
fn cached_title_label<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    decoded: &CatalogRecord,
    title: &'a mut String<64>,
) -> Option<&'a str>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let key = proto::cache::cache_key_for(decoded.display_name.as_str(), decoded.byte_size);
    let mut raw_name = String::<64>::new();
    if crate::reader_cache_files::read_cached_book_title(
        root,
        key.as_str(),
        (decoded.source_hash, decoded.byte_size),
        title,
    ) {
        Some(title.as_str())
    } else if upload_store::read_upload_label(root, decoded.open_name.as_str(), &mut raw_name) {
        crate::reader_store::derive_catalog_label(
            raw_name.as_str(),
            decoded.open_name.as_str(),
            title,
        );
        Some(title.as_str())
    } else {
        None
    }
}

/// Refill the resident list window so it covers the visible rows around
/// `selection`, reading from the card only when the window doesn't already
/// cover them. Called before each Library render; cheap behind the panel.
#[inline(never)]
pub(crate) fn ensure_library_window(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    selection: u16,
    portrait: bool,
) {
    let total = library.catalog_count();
    if total == 0 {
        library.begin_window(0);
        return;
    }
    let selection = (selection as usize).min(total - 1);
    let start = ui::render::library_scroll_start(selection, total, portrait);
    let need = ui::render::library_visible_rows(portrait).min(total - start);
    if library.window_covers(start, need) {
        return;
    }
    let _ = sd_session::with_root(epd, sd_cs, |root| read_catalog_window(root, library, start));
}

/// Make `index` the active book by reading its catalog record into the store,
/// so the reading path's `catalog_entry(index)` resolves without depending on
/// the list window. Idempotent when already active.
#[inline(never)]
pub(crate) fn load_active_entry(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
) -> bool {
    if library.active_index() == Some(index) {
        return true;
    }
    // The record carries the title persisted at scan/open, so the active
    // book's fallback label (Home colophon before a reopen) matches what the
    // list shows without any per-book cache probe.
    let resolved = sd_session::with_root(epd, sd_cs, |root| read_catalog_record_at(root, index))
        .ok()
        .flatten();
    match resolved {
        Some(record) => {
            library.set_active_entry(
                index,
                record.display_name.as_str(),
                record.open_name.as_str(),
                record.in_books_dir,
                record.byte_size,
                record.source_hash,
                (!record.title.is_empty()).then_some(record.title.as_str()),
            );
            true
        }
        None => false,
    }
}

/// Persist a book's just-learned title into its catalog record, so the next
/// window read (and the next boot's catalog cache) label it without probing
/// the book's cache. Runs inside the open's existing SD session. The record
/// at `index` is patched only when its identity still matches (the catalog
/// may have been rewritten under a stale index -- then the identity resolves
/// the true index) and only when the stored title actually differs, so the
/// common reopen costs one record read and no write.
pub(crate) fn update_catalog_title<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    index: usize,
    source_identity: (u32, u32),
    title: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if title.is_empty() {
        return false;
    }
    let Ok(xteink) = root.open_dir(CATALOG_ROOT_DIR) else {
        return false;
    };
    let Ok(file) = xteink.open_file_in_dir(CATALOG_FILE, Mode::ReadWriteAppend) else {
        return false;
    };
    if file.seek_from_start(0).is_err() {
        return false;
    }
    let mut header = [0u8; CATALOG_HEADER_BYTES];
    if read_exact_file(&file, &mut header).is_err() {
        return false;
    }
    let Some(count) = proto::catalog::decode_catalog_header(&header) else {
        return false;
    };
    // Trust the caller's index only if the record identity still matches;
    // otherwise resolve by identity on the already-open file (one streamed
    // pass, rare -- the catalog was rewritten under a stale index).
    let hinted = index < count as usize
        && record_identity(&file, index)
            .map(|identity| identity == source_identity)
            .unwrap_or(false);
    let target = if hinted {
        index
    } else {
        let mut found = None;
        if seek_to_record(&file, 0).is_err() {
            return false;
        }
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        for candidate in 0..count as usize {
            if read_exact_file(&file, &mut record).is_err() {
                return false;
            }
            if catalog_record_identity(&record) == source_identity {
                found = Some(candidate);
                break;
            }
        }
        let Some(found) = found else {
            return false;
        };
        found
    };
    let mut field = [0u8; CATALOG_TITLE_BYTES];
    encode_catalog_title(title, &mut field);
    let title_offset =
        (CATALOG_HEADER_BYTES + target * CATALOG_RECORD_BYTES + CATALOG_RECORD_TITLE_OFFSET) as u32;
    let mut stored = [0u8; CATALOG_TITLE_BYTES];
    if file.seek_from_start(title_offset).is_err() || read_exact_file(&file, &mut stored).is_err() {
        return false;
    }
    if stored == field {
        return true;
    }
    file.seek_from_start(title_offset).is_ok() && file.write(&field).is_ok()
}

/// Resolve a saved (path-hash, byte-size) back to its catalog index, the
/// reverse of `source_identity`. `hint` is the last-known index (from the saved
/// book_id); an unchanged catalog resolves in one read.
#[inline(never)]
pub(crate) fn find_index_by_identity(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    source_hash: u32,
    byte_size: u32,
    hint: Option<u16>,
) -> Option<u16> {
    sd_session::with_root(epd, sd_cs, |root| {
        find_in_catalog(root, source_hash, byte_size, hint)
    })
    .ok()
    .flatten()
}

/// Empty every book cache under CACHE2 whose book is no longer in the freshly
/// written catalog -- the orphans left when a book is deleted (through the shelf
/// or by pulling the card). Each cache is matched by its stored source identity,
/// not its key name, so a live book's cache is never swept. Reading position
/// lives in the global STATE.BIN and is untouched. Bounded per pass; any excess
/// is handled by the next scan.
///
/// The catalog's identities (8 B each) load into `scratch` once, sorted, so
/// each cache dir checks membership with an in-RAM binary search --
/// O((N + C) log N) rather than streaming the whole catalog off the card per
/// cache dir. Should the catalog outgrow the scratch (2,048 books against
/// the 16 KB arena), the overflow falls back to the streamed per-cache
/// lookup, keeping the sweep exact.
fn sweep_orphan_caches<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    scratch: &mut [u8],
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    const SWEEP_MAX_PER_PASS: usize = 48;
    let (staged, truncated) = stage_catalog_identities(root, scratch);
    // Collect cache-dir names up front: embedded-sdmmc forbids opening files
    // while a directory iteration holds the lock.
    let mut keys: heapless::Vec<String<8>, SWEEP_MAX_PER_PASS> = heapless::Vec::new();
    if let Ok(xteink) = root.open_dir(proto::cache::CACHE_ROOT_DIR) {
        if let Ok(cache) = xteink.open_dir(proto::cache::CACHE_V2_DIR) {
            let _ = cache.iterate_dir(|entry| {
                if !entry.attributes.is_directory() {
                    return;
                }
                let mut name = String::<8>::new();
                let _ = write!(name, "{}", entry.name);
                if name.is_empty() || name.as_str() == "." || name.as_str() == ".." {
                    return;
                }
                // Past capacity silently drops; the leftover keys sweep next scan.
                let _ = keys.push(name);
            });
        }
    }
    let mut swept = 0u32;
    for key in &keys {
        let header = crate::reader_cache_files::read_cache_header(root, key.as_str());
        // A readable cache that still maps to a catalog book stays. Anything
        // else -- no book, or an unreadable BOOK.BIN -- is reclaimed.
        let live = header
            .as_ref()
            .map(|h| {
                catalog_identity_staged(scratch, staged, h.source_hash, h.source_size)
                    || (truncated
                        && find_in_catalog(root, h.source_hash, h.source_size, None).is_some())
            })
            .unwrap_or(false);
        if live {
            continue;
        }
        let section_count = header.map(|h| h.section_count).unwrap_or(0);
        crate::reader_cache_files::empty_cache_dir(root, key.as_str(), section_count);
        swept += 1;
    }
    if swept > 0 {
        esp_println::println!("cache: swept {} orphan cache(s)", swept);
    }
}

/// Load every catalog record's `(source_hash, byte_size)` into `scratch` in
/// one streamed pass, then sort them for `catalog_identity_staged`'s binary
/// search. Returns `(staged_count, truncated)`; `truncated` also covers an
/// unreadable catalog, so callers keep the streamed fallback and a broken
/// catalog behaves exactly as before.
fn stage_catalog_identities<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    scratch: &mut [u8],
) -> (usize, bool)
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let capacity = scratch.len() / CATALOG_IDENTITY_BYTES;
    let (staged, truncated) = with_catalog_file(root, |file, count| {
        seek_to_record(file, 0)?;
        let take = (count as usize).min(capacity);
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        for index in 0..take {
            read_exact_file(file, &mut record)?;
            let (hash, size) = catalog_record_identity(&record);
            if !stage_catalog_identity(scratch, index, hash, size) {
                return Ok((index, true));
            }
        }
        Ok((take, count as usize > take))
    })
    .unwrap_or((0, true));
    sort_catalog_identities(scratch, staged);
    (staged, truncated)
}

/// Stream the whole catalog into the browser shelf buffer as
/// `flag|open_name|label` lines (B = /BOOKS, R = card root). Truncates to the
/// buffer; returns the bytes written.
#[inline(never)]
pub(crate) fn write_catalog_listing(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    out: &mut [u8],
) -> usize {
    sd_session::with_root(epd, sd_cs, |root| {
        with_catalog_file(root, |file, count| {
            seek_to_record(file, 0)?;
            let mut record = [0u8; CATALOG_RECORD_BYTES];
            let mut at = 0usize;
            for _ in 0..count as usize {
                if read_exact_file(file, &mut record).is_err() {
                    break;
                }
                let decoded = decode_catalog_record(&record);
                // The shelf shows the same label as the Library list: the
                // persisted title when the book has one, else the stem label.
                let mut label = String::<64>::new();
                if decoded.title.is_empty() {
                    derive_catalog_label(
                        decoded.display_name.as_str(),
                        decoded.open_name.as_str(),
                        &mut label,
                    );
                } else {
                    let _ = label.push_str(decoded.title.as_str());
                }
                let open_name = decoded.open_name.as_bytes();
                let line_len = 1 + 1 + open_name.len() + 1 + label.len() + 1;
                if at + line_len > out.len() {
                    break;
                }
                out[at] = if decoded.in_books_dir { b'B' } else { b'R' };
                at += 1;
                out[at] = b'|';
                at += 1;
                out[at..at + open_name.len()].copy_from_slice(open_name);
                at += open_name.len();
                out[at] = b'|';
                at += 1;
                out[at..at + label.len()].copy_from_slice(label.as_bytes());
                at += label.len();
                out[at] = b'\n';
                at += 1;
            }
            Ok(at)
        })
    })
    .ok()
    .and_then(Result::ok)
    .unwrap_or(0)
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

fn collect_epubs<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    dir: &embedded_sdmmc::Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    prefix: &str,
    in_books_dir: bool,
    visit: &mut impl FnMut(&str, &str, bool, u32),
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut lfn_storage = [0u8; 192];
    let mut lfn_buffer = LfnBuffer::new(&mut lfn_storage);
    let _ = dir.iterate_dir_lfn(&mut lfn_buffer, |entry, long_name| {
        if entry.attributes.is_directory() || entry.attributes.is_volume() {
            return;
        }

        let mut name = String::<64>::new();
        let mut open_name = String::<16>::new();
        use core::fmt::Write;
        let _ = write!(open_name, "{}", entry.name);
        let Some(file_name) = long_name else {
            let _ = write!(name, "{}", entry.name);
            if !is_epub_name(&name) {
                return;
            }
            visit_prefixed(prefix, &name, &open_name, in_books_dir, entry.size, visit);
            return;
        };

        if is_epub_name(file_name) {
            visit_prefixed(
                prefix,
                file_name,
                &open_name,
                in_books_dir,
                entry.size,
                visit,
            );
        }
    });
}

fn visit_prefixed(
    prefix: &str,
    name: &str,
    open_name: &str,
    in_books_dir: bool,
    byte_size: u32,
    visit: &mut impl FnMut(&str, &str, bool, u32),
) {
    let mut path = String::<64>::new();
    proto::storage::catalog_display_path(prefix, name, &mut path);
    visit(&path, open_name, in_books_dir, byte_size);
}

fn is_epub_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    // Uploaded books carry 8.3 names whose extension truncates to ".epu".
    if bytes.len() >= 4 {
        let tail = &bytes[bytes.len() - 4..];
        if tail[0] == b'.'
            && tail[1].eq_ignore_ascii_case(&b'e')
            && tail[2].eq_ignore_ascii_case(&b'p')
            && tail[3].eq_ignore_ascii_case(&b'u')
        {
            return true;
        }
    }
    if bytes.len() >= 5 {
        let ext = &bytes[bytes.len() - 5..];
        if ext[0] == b'.'
            && ext[1].eq_ignore_ascii_case(&b'e')
            && ext[2].eq_ignore_ascii_case(&b'p')
            && ext[3].eq_ignore_ascii_case(&b'u')
            && ext[4].eq_ignore_ascii_case(&b'b')
        {
            return true;
        }
    }
    if bytes.len() >= 4 {
        let ext = &bytes[bytes.len() - 4..];
        return ext[0] == b'.'
            && ext[1].eq_ignore_ascii_case(&b'e')
            && ext[2].eq_ignore_ascii_case(&b'p')
            && ext[3].eq_ignore_ascii_case(&b'u');
    }
    false
}
