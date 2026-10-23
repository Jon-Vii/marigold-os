#![allow(dead_code, unused_imports)]
extern crate self as esp_println;
extern crate self as hal_ext;
#[path = "../../hal-ext/src/nvm.rs"]
pub mod nvm;
pub use std::println;
#[path = "../../fw/src/book_images.rs"]
mod book_images;
#[path = "support/fat.rs"]
mod fat;
#[path = "../../fw/src/reader_cache_files.rs"]
mod reader_cache_files;
#[path = "../../fw/src/reader_layout.rs"]
mod reader_layout;
#[path = "../../fw/src/reader_store.rs"]
mod reader_store;
#[path = "support/zip.rs"]
mod zip;
// Same truncate-then-delete operation as the board adapter, without ESP HAL.
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
use embedded_sdmmc::{VolumeIdx, VolumeManager};
use proto::cache::BookV2SectionRecord;
use reader_cache_files::{BookIndexLoadResult, CacheLoadResult};
use reader_store::{ReaderStore, SectionTable};
/// The index writer takes the resident table; tests describe sections as
/// plain records.
fn write_index<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &embedded_sdmmc::Directory<'_, D, T, ND, NF, NV>,
    key: &str,
    identity: (u32, u32),
    total_pages: u32,
    sections: &[BookV2SectionRecord],
    store: &ReaderStore,
    partial: bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    let mut table: SectionTable = SectionTable::new();
    assert!(
        table.set_records(sections),
        "fixture sections are contiguous"
    );
    reader_cache_files::write_v2_book_index(
        root,
        key,
        identity,
        total_pages,
        &table,
        store,
        partial,
    )
}

const KEY: &str = "E0000001";
const ID: (u32, u32) = (1234, 5678);

#[test]
fn book_index_generations_roundtrip_and_preserve_the_old_bank() {
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let mut store = ReaderStore::new();
    let mut sections = [BookV2SectionRecord {
        section: 0,
        spine: 0,
        start_page: 0,
        page_count: 1,
        partial: false,
    }];
    assert_eq!(
        reader_cache_files::prepare_book_replacement(&root, KEY),
        Some(0)
    );
    assert!(write_index(&root, KEY, ID, 1, &sections, &store, false));
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
        BookIndexLoadResult::Hit
    );
    assert_eq!(
        reader_cache_files::prepare_book_replacement(&root, KEY),
        Some(1024)
    );
    sections[0].section = 1024;
    sections[0].page_count = 2;
    assert!(write_index(&root, KEY, ID, 2, &sections, &store, false));
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
        BookIndexLoadResult::Hit
    );
    assert_eq!(store.book_total_pages, 2);
    assert_eq!(store.book_sections.iter().next().unwrap().section, 1024);
    assert_eq!(
        reader_cache_files::prepare_book_replacement(&root, KEY),
        Some(0)
    );
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
        BookIndexLoadResult::Hit
    );
    assert_eq!(
        store.book_total_pages, 2,
        "preparing replacement must preserve active index"
    );
}

fn seed_index(disk: &fat::Disk) {
    let manager: VolumeManager<_, _, 8, 8, 1> = VolumeManager::new_with_limits(disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let store = ReaderStore::new();
    let section = BookV2SectionRecord {
        section: 0,
        spine: 0,
        start_page: 0,
        page_count: 1,
        partial: false,
    };
    assert!(write_index(&root, KEY, ID, 1, &[section], &store, false));
}

#[test]
fn interrupted_index_writes_recover_a_complete_generation() {
    let original = fat::Disk::new();
    seed_index(&original);
    let snapshot = original.bytes.borrow().clone();
    for cut in 0..45 {
        let disk = fat::Disk::new();
        *disk.bytes.borrow_mut() = snapshot.clone();
        {
            let manager: VolumeManager<_, _, 8, 8, 1> =
                VolumeManager::new_with_limits(&disk, fat::Clock, 0);
            let volume = manager.open_volume(VolumeIdx(0)).unwrap();
            let root = volume.open_root_dir().unwrap();
            let store = ReaderStore::new();
            let section = BookV2SectionRecord {
                section: 0,
                spine: 0,
                start_page: 0,
                page_count: 2,
                partial: false,
            };
            disk.fail_after.set(Some(cut));
            let _ = write_index(&root, KEY, ID, 2, &[section], &store, false);
        }
        disk.fail_after.set(None);
        let manager: VolumeManager<_, _, 8, 8, 1> =
            VolumeManager::new_with_limits(&disk, fat::Clock, 0);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let mut store = ReaderStore::new();
        assert_eq!(
            reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
            BookIndexLoadResult::Hit,
            "write cut {cut}"
        );
        assert!([1, 2].contains(&store.book_total_pages));
    }
}

#[test]
fn read_failures_never_retire_or_truncate_the_active_index() {
    let original = fat::Disk::new();
    seed_index(&original);
    let snapshot = original.bytes.borrow().clone();
    for replacement in [false, true] {
        for cut in 0..32 {
            let disk = fat::Disk::new();
            *disk.bytes.borrow_mut() = snapshot.clone();
            {
                let manager: VolumeManager<_, _, 8, 8, 1> =
                    VolumeManager::new_with_limits(&disk, fat::Clock, 0);
                let volume = manager.open_volume(VolumeIdx(0)).unwrap();
                let root = volume.open_root_dir().unwrap();
                disk.fail_read_after.set(Some(cut));
                if replacement {
                    let _ = reader_cache_files::prepare_book_replacement(&root, KEY);
                } else {
                    let store = ReaderStore::new();
                    let section = BookV2SectionRecord {
                        section: 0,
                        spine: 0,
                        start_page: 0,
                        page_count: 2,
                        partial: false,
                    };
                    let _ = write_index(&root, KEY, ID, 2, &[section], &store, false);
                }
            }
            disk.fail_read_after.set(None);
            let manager: VolumeManager<_, _, 8, 8, 1> =
                VolumeManager::new_with_limits(&disk, fat::Clock, 0);
            let volume = manager.open_volume(VolumeIdx(0)).unwrap();
            let root = volume.open_root_dir().unwrap();
            let mut store = ReaderStore::new();
            assert_eq!(
                reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
                BookIndexLoadResult::Hit,
                "replacement={replacement} read cut={cut}"
            );
            assert!([1, 2].contains(&store.book_total_pages));
        }
    }
}

fn append_fixture_line(store: &mut ReaderStore, spine: u16, line: usize) {
    let text = format!("chapter {spine} line {line:03} keeps its place");
    let role = if line % 11 == 0 {
        proto::text::TextRole::BlockQuote
    } else {
        proto::text::TextRole::Body
    };
    assert!(store.push_line_block(
        &text,
        display::font::FontStyle::Regular,
        role,
        proto::text::TextAlign::Left,
        line % 7 == 6,
        spine
    ));
    reader_layout::extend_page_index(store);
}
fn page_text(store: &ReaderStore) -> Vec<Vec<String>> {
    store.pages[..store.page_count]
        .iter()
        .map(|p| {
            (usize::from(p.first_block)..usize::from(p.first_block + p.block_count))
                .map(|i| {
                    format!(
                        "{}:{}:{}",
                        store.block_text(i),
                        store.block_paragraph_start[i],
                        store.block_paragraph_end[i]
                    )
                })
                .collect()
        })
        .collect()
}

#[test]
fn spacing_reflow_matches_fresh_pagination_across_old_section_boundaries() {
    use display::font::{LineSpacing, TypeSettings};
    for portrait in [false, true] {
        let disk = fat::Disk::new();
        let manager: VolumeManager<_, _, 8, 8, 1> =
            VolumeManager::new_with_limits(&disk, fat::Clock, 0);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let mut store = ReaderStore::new();
        store.set_layout(
            TypeSettings {
                spacing: LineSpacing::Compact,
                ..TypeSettings::DEFAULT
            },
            portrait,
        );
        let mut records = Vec::new();
        let mut total = 0u32;
        for spine in 0..3u16 {
            store.clear_lines();
            store.push_toc_record(&format!("Chapter {spine}"), 1, spine as i16);
            for line in 0..200 {
                append_fixture_line(&mut store, spine, line);
                if store.page_count >= 3 || line == 199 {
                    store.set_cached_spine(spine);
                    assert!(reader_layout::flush_page_window(
                        &mut store,
                        line != 199,
                        |window| {
                            let section = records.len() as u16;
                            if !reader_cache_files::write_v2_section_cache(
                                &root, KEY, ID, section, window,
                            ) {
                                return false;
                            }
                            records.push(BookV2SectionRecord {
                                section,
                                spine,
                                start_page: total,
                                page_count: window.page_count as u16,
                                partial: false,
                            });
                            total += window.page_count as u32;
                            true
                        }
                    ));
                }
            }
        }
        assert!(
            records.len() > 3,
            "fixture spans multiple old sections per chapter"
        );
        assert!(write_index(&root, KEY, ID, total, &records, &store, false));
        let mut scratch = vec![0u8; 24576];
        for spacing in [
            LineSpacing::Relaxed,
            LineSpacing::Compact,
            LineSpacing::Normal,
            LineSpacing::Relaxed,
            LineSpacing::Normal,
            LineSpacing::Compact,
        ] {
            let settings = TypeSettings {
                spacing,
                ..TypeSettings::DEFAULT
            };
            store.set_layout(settings, portrait);
            assert_eq!(
                reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
                BookIndexLoadResult::SpacingMismatch
            );
            assert!(reader_cache_files::reflow_book_spacing(
                &root,
                KEY,
                ID,
                &mut store,
                &mut scratch
            ));
            assert_eq!(
                reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
                BookIndexLoadResult::Hit
            );
            let mut actual = Vec::new();
            let mut expected = Vec::new();
            let sections = store.book_sections.iter().collect::<Vec<_>>();
            for section in sections {
                assert!(matches!(
                    reader_cache_files::load_v2_section_cache(
                        &root,
                        KEY,
                        ID,
                        section.section,
                        section.spine,
                        section.page_count as usize,
                        &mut store
                    ),
                    CacheLoadResult::Hit {
                        repaginated: false,
                        ..
                    }
                ));
                actual.extend(page_text(&store));
            }
            for spine in 0..3u16 {
                assert_eq!(store.page_for_spine(spine), expected.len() as u32);
                let mut fresh = ReaderStore::new();
                fresh.set_layout(settings, portrait);
                for line in 0..200 {
                    append_fixture_line(&mut fresh, spine, line);
                }
                expected.extend(page_text(&fresh));
            }
            assert_eq!(actual, expected, "spacing={spacing:?}, portrait={portrait}");
            assert_eq!(store.book_total_pages as usize, expected.len());
            reader_layout::rebuild_toc_page_targets(&mut store);
            for spine in 0..3 {
                assert_eq!(
                    u32::from(store.toc_page[spine]),
                    store.page_for_spine(spine as u16)
                );
            }
        }
    }
}

#[test]
fn corrupted_new_generation_falls_back_for_loading_and_orphan_identity() {
    let disk = fat::Disk::new();
    seed_index(&disk);
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let mut store = ReaderStore::new();
    let section = BookV2SectionRecord {
        section: 0,
        spine: 0,
        start_page: 0,
        page_count: 2,
        partial: false,
    };
    assert!(write_index(&root, KEY, ID, 2, &[section], &store, false));
    {
        let x = root.open_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
        let c = x.open_dir(proto::cache::CACHE_V2_DIR).unwrap();
        let b = c.open_dir(KEY).unwrap();
        let file = b
            .open_file_in_dir("BOOKB.BIN", embedded_sdmmc::Mode::ReadWriteAppend)
            .unwrap();
        file.seek_from_start(8).unwrap();
        file.write(&9999u32.to_le_bytes()).unwrap();
        file.close().unwrap();
    }
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
        BookIndexLoadResult::Hit
    );
    assert_eq!(store.book_total_pages, 1);
}

#[test]
fn legacy_index_remains_readable_and_prefix_does_not_advertise_a_final_total() {
    let disk = fat::Disk::new();
    seed_index(&disk);
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    {
        let x = root.open_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
        let c = x.open_dir(proto::cache::CACHE_V2_DIR).unwrap();
        let b = c.open_dir(KEY).unwrap();
        let file = b
            .open_file_in_dir("BOOKA.BIN", embedded_sdmmc::Mode::ReadOnly)
            .unwrap();
        let mut body = vec![0u8; file.length() as usize - 16];
        assert_eq!(file.read(&mut body).unwrap(), body.len());
        file.close().unwrap();
        let legacy = b
            .open_file_in_dir("BOOK.BIN", embedded_sdmmc::Mode::ReadWriteCreateOrTruncate)
            .unwrap();
        legacy.write(&body).unwrap();
        legacy.close().unwrap();
        assert!(sd_session::remove_file_reclaiming_clusters(&b, "BOOKA.BIN"));
    }
    let mut store = ReaderStore::new();
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
        BookIndexLoadResult::Hit
    );
    assert_eq!(store.book_next_spine(), None);
    assert_eq!(
        reader_cache_files::prepare_book_replacement(&root, KEY),
        Some(1024)
    );
    store.set_book_next_spine(Some(1));
    store.set_book_generated_toc(true);
    let section = BookV2SectionRecord {
        section: 1024,
        spine: 0,
        start_page: 0,
        page_count: 1,
        partial: false,
    };
    assert!(write_index(&root, KEY, ID, 1, &[section], &store, false));
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
        BookIndexLoadResult::Hit
    );
    assert_eq!(store.book_next_spine(), Some(1));
    assert!(store.book_generated_toc());
    assert_eq!(
        reader_cache_files::read_v2_book_total_pages(&root, KEY, ID, &store),
        0
    );
    store.set_layout(
        display::font::TypeSettings {
            spacing: display::font::LineSpacing::Relaxed,
            ..display::font::TypeSettings::DEFAULT
        },
        false,
    );
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
        BookIndexLoadResult::Invalid,
        "do not reflow incomplete source"
    );
}

#[test]
fn advisory_legacy_titles_do_not_override_generation_page_state() {
    use embedded_sdmmc::Mode;
    use proto::cache::{CACHE_BOOK_FILE, CACHE_ROOT_DIR, CACHE_V2_DIR};
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let mut store = ReaderStore::new();
    store.set_book_labels("Legacy title", "Author");
    let mut section = BookV2SectionRecord {
        section: 0,
        spine: 0,
        start_page: 0,
        page_count: 1,
        partial: false,
    };
    assert!(write_index(&root, KEY, ID, 1, &[section], &store, false));
    // An old, still source-matching index can coexist with committed generations.
    {
        let xteink = root.open_dir(CACHE_ROOT_DIR).unwrap();
        let cache = xteink.open_dir(CACHE_V2_DIR).unwrap();
        let book = cache.open_dir(KEY).unwrap();
        let generation = book.open_file_in_dir("BOOKA.BIN", Mode::ReadOnly).unwrap();
        let mut body = vec![0u8; generation.length() as usize - 16];
        assert_eq!(generation.read(&mut body).unwrap(), body.len());
        generation.close().unwrap();
        let legacy = book
            .open_file_in_dir(CACHE_BOOK_FILE, Mode::ReadWriteCreateOrTruncate)
            .unwrap();
        legacy.write(&body).unwrap();
        legacy.close().unwrap();
    }
    store.set_book_labels("Current title", "Author");
    section.page_count = 2;
    assert!(write_index(&root, KEY, ID, 2, &[section], &store, false));
    let mut title = heapless::String::<64>::new();
    assert!(reader_cache_files::read_cached_book_title(
        &root, KEY, ID, &mut title
    ));
    assert_eq!(
        title.as_str(),
        "Legacy title",
        "advisory lookup skips generation probes on a legacy hit"
    );
    assert_eq!(
        reader_cache_files::read_v2_book_total_pages(&root, KEY, ID, &store),
        2
    );
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
        BookIndexLoadResult::Hit
    );
    assert_eq!(store.book_total_pages, 2);
    // A changed EPUB identity must skip the stale legacy metadata.
    let changed_id = (ID.0 + 1, ID.1 + 1);
    assert!(write_index(
        &root,
        KEY,
        changed_id,
        2,
        &[section],
        &store,
        false
    ));
    assert!(reader_cache_files::read_cached_book_title(
        &root, KEY, changed_id, &mut title
    ));
    assert_eq!(title.as_str(), "Current title");
    assert!(!reader_cache_files::read_cached_book_title(
        &root,
        KEY,
        (0, 0),
        &mut title
    ));
    assert_eq!(
        title.as_str(),
        "Current title",
        "failed lookup leaves caller label intact"
    );
}

#[test]
fn spacing_reflow_write_interruptions_preserve_a_readable_generation() {
    use display::font::{LineSpacing, TypeSettings};
    let seed = fat::Disk::new();
    {
        let manager: VolumeManager<_, _, 8, 8, 1> =
            VolumeManager::new_with_limits(&seed, fat::Clock, 0);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let mut store = ReaderStore::new();
        store.set_layout(
            TypeSettings {
                spacing: LineSpacing::Compact,
                ..TypeSettings::DEFAULT
            },
            false,
        );
        for line in 0..80 {
            append_fixture_line(&mut store, 0, line);
        }
        store.set_cached_spine(0);
        assert!(reader_cache_files::write_v2_section_cache(
            &root, KEY, ID, 0, &store
        ));
        let section = BookV2SectionRecord {
            section: 0,
            spine: 0,
            start_page: 0,
            page_count: store.page_count as u16,
            partial: false,
        };
        assert!(write_index(
            &root,
            KEY,
            ID,
            store.page_count as u32,
            &[section],
            &store,
            false
        ));
    }
    for cut in 0..100 {
        let disk = fat::Disk::new();
        disk.bytes
            .borrow_mut()
            .copy_from_slice(&seed.bytes.borrow());
        {
            let manager: VolumeManager<_, _, 8, 8, 1> =
                VolumeManager::new_with_limits(&disk, fat::Clock, 0);
            let volume = manager.open_volume(VolumeIdx(0)).unwrap();
            let root = volume.open_root_dir().unwrap();
            let mut store = ReaderStore::new();
            store.set_layout(
                TypeSettings {
                    spacing: LineSpacing::Relaxed,
                    ..TypeSettings::DEFAULT
                },
                false,
            );
            assert_eq!(
                reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
                BookIndexLoadResult::SpacingMismatch
            );
            disk.fail_after.set(Some(cut));
            let _ = reader_cache_files::reflow_book_spacing(
                &root,
                KEY,
                ID,
                &mut store,
                &mut [0; 24576],
            );
        }
        disk.fail_after.set(None);
        let manager: VolumeManager<_, _, 8, 8, 1> =
            VolumeManager::new_with_limits(&disk, fat::Clock, 0);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let mut store = ReaderStore::new();
        store.set_layout(
            TypeSettings {
                spacing: LineSpacing::Compact,
                ..TypeSettings::DEFAULT
            },
            false,
        );
        let result = reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store);
        assert!(
            matches!(
                result,
                BookIndexLoadResult::Hit | BookIndexLoadResult::SpacingMismatch
            ),
            "cut={cut} result={result:?}"
        );
        let spacing = if result == BookIndexLoadResult::Hit {
            LineSpacing::Compact
        } else {
            LineSpacing::Relaxed
        };
        store.set_layout(
            TypeSettings {
                spacing,
                ..TypeSettings::DEFAULT
            },
            false,
        );
        assert_eq!(
            reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
            BookIndexLoadResult::Hit
        );
        let sections = store.book_sections.iter().collect::<Vec<_>>();
        let mut actual = Vec::new();
        for section in sections {
            assert!(
                matches!(
                    reader_cache_files::load_v2_section_cache(
                        &root,
                        KEY,
                        ID,
                        section.section,
                        section.spine,
                        section.page_count as usize,
                        &mut store
                    ),
                    CacheLoadResult::Hit {
                        repaginated: false,
                        ..
                    }
                ),
                "cut={cut}"
            );
            actual.extend(page_text(&store));
        }
        let mut fresh = ReaderStore::new();
        fresh.set_layout(
            TypeSettings {
                spacing,
                ..TypeSettings::DEFAULT
            },
            false,
        );
        for line in 0..80 {
            append_fixture_line(&mut fresh, 0, line);
        }
        assert_eq!(actual, page_text(&fresh), "cut={cut}");
    }
}

#[test]
fn advisory_generation_titles_validate_source_without_reading_commit_footer() {
    use embedded_sdmmc::Mode;
    use proto::cache::{CACHE_ROOT_DIR, CACHE_V2_DIR};
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let mut store = ReaderStore::new();
    let section = BookV2SectionRecord {
        section: 0,
        spine: 0,
        start_page: 0,
        page_count: 1,
        partial: false,
    };
    store.set_book_labels("First source", "Author");
    assert!(write_index(&root, KEY, ID, 1, &[section], &store, false));
    let second_id = (ID.0 + 1, ID.1 + 1);
    store.set_book_labels("Second source", "Author");
    assert!(write_index(
        &root,
        KEY,
        second_id,
        1,
        &[section],
        &store,
        false
    ));
    let mut title = heapless::String::<64>::new();
    assert!(reader_cache_files::read_cached_book_title(
        &root, KEY, ID, &mut title
    ));
    assert_eq!(title.as_str(), "First source");
    // A readable source-matching label is useful even when its pagination
    // commit record is damaged; ordinary index loading still rejects it.
    {
        let xteink = root.open_dir(CACHE_ROOT_DIR).unwrap();
        let cache = xteink.open_dir(CACHE_V2_DIR).unwrap();
        let book = cache.open_dir(KEY).unwrap();
        let file = book
            .open_file_in_dir("BOOKB.BIN", Mode::ReadWriteAppend)
            .unwrap();
        file.seek_from_start(file.length() - 16).unwrap();
        file.write(&[0; 4]).unwrap();
        file.close().unwrap();
    }
    assert!(reader_cache_files::read_cached_book_title(
        &root, KEY, second_id, &mut title
    ));
    assert_eq!(title.as_str(), "Second source");
    assert_ne!(
        reader_cache_files::load_v2_book_index(&root, KEY, second_id, &mut store),
        BookIndexLoadResult::Hit
    );
}

#[test]
fn full_block_table_reports_failure_instead_of_dropping_the_line() {
    let mut store = ReaderStore::new();
    let capacity = store.block_capacity();
    for _ in 0..capacity {
        assert!(store.push_line_block(
            "w",
            display::font::FontStyle::Regular,
            proto::text::TextRole::Body,
            proto::text::TextAlign::Left,
            false,
            0,
        ));
    }
    // The builder flushes the section and retries on `false`; `true` here
    // used to mean "stored" while the line was silently discarded.
    assert!(!store.push_line_block(
        "this line must not vanish",
        display::font::FontStyle::Regular,
        proto::text::TextRole::Body,
        proto::text::TextAlign::Left,
        false,
        0,
    ));
    assert_eq!(store.block_count(), capacity);
}

#[test]
fn no_break_space_line_is_kept_as_a_blank_spacer() {
    let mut store = ReaderStore::new();
    assert!(store.push_line_block(
        "\u{a0}",
        display::font::FontStyle::Regular,
        proto::text::TextRole::Body,
        proto::text::TextAlign::Justify,
        true,
        0,
    ));
    assert_eq!(store.block_count(), 1);
    assert!(store.push_line_block(
        "   ",
        display::font::FontStyle::Regular,
        proto::text::TextRole::Body,
        proto::text::TextAlign::Justify,
        true,
        0,
    ));
    assert_eq!(
        store.block_count(),
        1,
        "ASCII-blank lines are still skipped"
    );
}

#[test]
fn justified_lines_fill_the_measure_except_a_paragraphs_last() {
    use display::fb::Framebuffer;
    let mut store = ReaderStore::new();
    let body = proto::text::TextRole::Body;
    let justify = proto::text::TextAlign::Justify;
    let regular = display::font::FontStyle::Regular;
    // A mid-paragraph line, a paragraph's last line, and a mid-paragraph
    // line too short to stretch (it would open rivers).
    assert!(store.push_line_block(
        "It was the best of times, it was the worst of times, it was the age of",
        regular,
        body,
        justify,
        false,
        0,
    ));
    assert!(store.push_line_block("wisdom.", regular, body, justify, true, 0));
    assert!(store.push_line_block("A short line", regular, body, justify, false, 0));
    reader_layout::rebuild_page_index(&mut store);

    let mut fb = Framebuffer::new();
    fb.clear(true);
    ui::reading::draw_reading_page_body(
        &mut fb,
        &store,
        store.pages[0],
        ui::reading::draw_image_placeholder,
    );
    let page_box = ui::reading::ReadingBlocks::page_box(&store);
    let target = page_box.right - ui::reading::READER_WRAP_SAFETY;
    let settings = ui::reading::ReadingBlocks::type_settings(&store);
    let advance = ui::reading::line_advance(settings, body);
    let blocks_y = {
        let mut ys = Vec::new();
        ui::reading::for_each_drawable_block(&store, store.pages[0], |block| {
            ys.push(block.y);
            true
        });
        ys
    };
    let rightmost_ink = |line: usize| -> i16 {
        // The x-height band just above the baseline: clear of the line
        // above's descenders and of this line's own.
        let baseline = blocks_y[line];
        let mut right = -1;
        for y in (baseline - 9)..baseline {
            for x in 0..fb.frame_width() as i16 {
                if !fb.pixel(x as usize, y as usize) {
                    right = right.max(x);
                }
            }
        }
        right
    };
    let first = rightmost_ink(0);
    assert!(
        (target - 6..=target).contains(&first),
        "mid-paragraph line ends at {first}, target {target}"
    );
    assert!(
        rightmost_ink(1) < target - 200,
        "last line keeps natural spacing"
    );
    assert!(
        rightmost_ink(2) < target - 200,
        "short line is not stretched"
    );
}

/// Free data clusters on the simulated FAT16 card, read from FAT #1.
fn free_clusters(disk: &fat::Disk) -> usize {
    let bytes = disk.bytes.borrow();
    // Partition at LBA 1, one reserved sector, then 64 FAT sectors.
    let fat = &bytes[2 * 512..(2 + 64) * 512];
    // Data clusters: 16384 sectors - reserved - 2 FATs - 32 root sectors.
    let clusters = 16384 - 1 - 2 * 64 - 32;
    (2..2 + clusters)
        .filter(|cluster| fat[cluster * 2] == 0 && fat[cluster * 2 + 1] == 0)
        .count()
}

#[test]
fn deleting_files_and_cache_directories_returns_every_cluster() {
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();

    let start = free_clusters(&disk);
    {
        let file = root
            .open_file_in_dir("A.BIN", embedded_sdmmc::Mode::ReadWriteCreateOrTruncate)
            .unwrap();
        file.write(&[7u8; 512 * 3]).unwrap();
    }
    assert_eq!(free_clusters(&disk), start - 3);
    assert!(sd_session::remove_file_reclaiming_clusters(&root, "A.BIN"));
    assert_eq!(
        free_clusters(&disk),
        start,
        "a removed file frees all its clusters"
    );

    // A whole book cache: index generations, a section, and its folders.
    // Another book's cache first, so the shared XTEINK/CACHE2 folders exist
    // before the baseline.
    assert!(reader_cache_files::prepare_book_replacement(&root, "E0000002").is_some());
    let baseline = free_clusters(&disk);
    let mut store = ReaderStore::new();
    assert_eq!(
        reader_cache_files::prepare_book_replacement(&root, KEY),
        Some(0)
    );
    for line in 0..30 {
        append_fixture_line(&mut store, 0, line);
    }
    store.set_cached_spine(0);
    assert!(reader_cache_files::write_v2_section_cache(
        &root, KEY, ID, 0, &store
    ));
    let sections = [BookV2SectionRecord {
        section: 0,
        spine: 0,
        start_page: 0,
        page_count: store.page_count as u16,
        partial: false,
    }];
    assert!(write_index(
        &root,
        KEY,
        ID,
        store.page_count as u32,
        &sections,
        &store,
        false
    ));
    // Both position generations: an orphan's saved place must not keep its
    // folder alive.
    reader_cache_files::write_position_file(&root, KEY, 3, 42).unwrap();
    reader_cache_files::write_position_file(&root, KEY, 3, 43).unwrap();
    // And the book's image index goes with its folder.
    {
        let xteink = root.open_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
        let cache = xteink.open_dir(proto::cache::CACHE_V2_DIR).unwrap();
        let book = cache.open_dir(KEY).unwrap();
        let index = book
            .open_file_in_dir(
                proto::image::index::IMAGE_INDEX_FILE,
                embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
            )
            .unwrap();
        index.write(&[1u8; 3000]).unwrap();
    }
    assert!(free_clusters(&disk) < baseline);
    assert!(reader_cache_files::empty_cache_dir(&root, KEY));
    {
        let xteink = root.open_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
        let cache = xteink.open_dir(proto::cache::CACHE_V2_DIR).unwrap();
        assert!(cache.open_dir(KEY).is_err(), "the folder itself is removed");
    }
    assert_eq!(
        free_clusters(&disk),
        baseline,
        "emptying a book cache frees its files and folders"
    );
}

#[test]
fn page_anchor_reads_the_old_pagination_only_after_a_layout_change() {
    use display::font::{FontSize, TypeSettings};
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let mut store = ReaderStore::new();
    let record = |section: u16, spine: u16, start_page: u32, page_count: u16| BookV2SectionRecord {
        section,
        spine,
        start_page,
        page_count,
        partial: false,
    };
    // Spine 1 spans pages 10..30 over two sections; spine 2 follows.
    let sections = [
        record(0, 0, 0, 10),
        record(1, 1, 10, 12),
        record(2, 1, 22, 8),
        record(3, 2, 30, 5),
    ];
    assert!(write_index(&root, KEY, ID, 35, &sections, &store, false));

    // Same layout: page numbers still hold.
    let same = reader_cache_files::read_v2_page_anchor(&root, KEY, ID, &store, 20);
    assert_eq!((same.source, same.page, same.place), (ID, 20, None));

    // Larger type: page 20 is pinned 10.5 pages into spine 1's 20.
    store.set_layout(
        TypeSettings {
            size: FontSize::Large,
            ..TypeSettings::DEFAULT
        },
        false,
    );
    let pinned = reader_cache_files::read_v2_page_anchor(&root, KEY, ID, &store, 20);
    assert_eq!(pinned.place, Some((1, reader_store::page_fraction(10, 20))));
    // The holder's spine is whole even when it is the book's last.
    let last = reader_cache_files::read_v2_page_anchor(&root, KEY, ID, &store, 34);
    assert_eq!(last.place, Some((2, reader_store::page_fraction(4, 5))));
    // Past the end, or another source's index, pins nothing.
    assert_eq!(
        reader_cache_files::read_v2_page_anchor(&root, KEY, ID, &store, 35).place,
        None
    );
    assert_eq!(
        reader_cache_files::read_v2_page_anchor(&root, KEY, (1, 2), &store, 20).place,
        None
    );
}

#[test]
fn the_text_start_round_trips_through_the_book_index() {
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let mut store = ReaderStore::new();
    let sections = [
        BookV2SectionRecord {
            section: 0,
            spine: 0,
            start_page: 0,
            page_count: 2,
            partial: false,
        },
        BookV2SectionRecord {
            section: 1,
            spine: 2,
            start_page: 2,
            page_count: 8,
            partial: false,
        },
    ];
    store.set_text_start_spine(Some(2));
    assert!(write_index(&root, KEY, ID, 10, &sections, &store, false));
    let mut loaded = ReaderStore::new();
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut loaded),
        BookIndexLoadResult::Hit
    );
    assert_eq!(loaded.text_start_spine(), Some(2));
}

#[test]
fn a_book_index_far_past_the_old_section_cap_round_trips() {
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    // Second bank (ids from 1024), 700 sections: ids reach 1723.
    let mut sections = Vec::new();
    let mut start = 0;
    for index in 0..700u16 {
        sections.push(BookV2SectionRecord {
            section: 1024 + index,
            spine: index / 2,
            start_page: start,
            page_count: 3,
            partial: false,
        });
        start += 3;
    }
    let store = ReaderStore::new();
    assert!(write_index(&root, KEY, ID, start, &sections, &store, false));
    let mut loaded = ReaderStore::new();
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut loaded),
        BookIndexLoadResult::Hit
    );
    assert_eq!(loaded.book_sections.iter().collect::<Vec<_>>(), sections);
    assert_eq!(loaded.book_total_pages, start);

    // A damaged record clears the table rather than leaving half of it.
    let mut broken = sections.clone();
    broken[500].start_page += 1;
    let mut table: SectionTable = SectionTable::new();
    assert!(!table.set_records(&broken));
}

fn record(section: u16, spine: u16, start_page: u32, page_count: u16) -> BookV2SectionRecord {
    BookV2SectionRecord {
        section,
        spine,
        start_page,
        page_count,
        partial: false,
    }
}

const SEED: u32 = reader_store::PROVISIONAL_PAGE_BASE;

/// Spine 5 laid out ahead in a seed run: 12 provisional pages.
fn seed_spine_five(store: &mut ReaderStore, first_id: u16) {
    store.seed.first_spine = Some(5);
    store.seed.next_spine = Some(6);
    assert!(store.seed.sections.push(record(first_id, 5, SEED, 8)));
    assert!(store
        .seed
        .sections
        .push(record(first_id + 1, 5, SEED + 8, 4)));
}

#[test]
fn a_seed_run_round_trips_with_and_without_a_prefix() {
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();

    // Nothing before the run laid out yet: the index holds only the run.
    let mut store = ReaderStore::new();
    store.set_book_next_spine(Some(0));
    seed_spine_five(&mut store, 2048);
    assert!(write_index(&root, KEY, ID, 0, &[], &store, false));
    let mut loaded = ReaderStore::new();
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut loaded),
        BookIndexLoadResult::Hit
    );
    assert_eq!(loaded.book_total_pages, 0);
    assert_eq!(loaded.book_next_spine(), Some(0));
    assert_eq!(
        (loaded.seed.first_spine, loaded.seed.next_spine),
        (Some(5), Some(6))
    );
    assert_eq!(loaded.seed.sections.len(), 2);
    assert_eq!(loaded.requested_page(SEED + 9, None), Some(SEED + 9));
    assert_eq!(loaded.requested_page(0, None), None);
    // The run's ids name its bank, so a replacement keeps it as the active
    // generation and builds into the other.
    assert_eq!(
        reader_cache_files::prepare_book_replacement(&root, KEY),
        Some(1024)
    );
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut loaded),
        BookIndexLoadResult::Hit
    );

    // With a prefix ahead of it, both tables come back.
    let mut store = ReaderStore::new();
    store.set_book_next_spine(Some(1));
    seed_spine_five(&mut store, 2048);
    assert!(write_index(
        &root,
        KEY,
        ID,
        10,
        &[record(0, 0, 0, 10)],
        &store,
        false
    ));
    let mut loaded = ReaderStore::new();
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut loaded),
        BookIndexLoadResult::Hit
    );
    assert_eq!(loaded.book_total_pages, 10);
    assert_eq!(loaded.seed.sections.len(), 2);
    assert_eq!(loaded.page_for_spine(5), SEED);

    // A run that does not start at the provisional base is refused.
    let mut store = ReaderStore::new();
    store.set_book_next_spine(Some(1));
    store.seed.first_spine = Some(5);
    store.seed.next_spine = Some(6);
    assert!(store.seed.sections.push(record(2048, 5, SEED + 1, 8)));
    assert!(write_index(
        &root,
        KEY,
        ID,
        10,
        &[record(0, 0, 0, 10)],
        &store,
        false
    ));
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut loaded),
        BookIndexLoadResult::Invalid
    );
    assert!(loaded.seed.first_spine.is_none());
}

#[test]
fn a_provisional_page_pins_its_place_in_the_old_seed_run() {
    use display::font::{FontSize, TypeSettings};
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let mut store = ReaderStore::new();
    store.set_book_next_spine(Some(1));
    seed_spine_five(&mut store, 2048);
    assert!(write_index(
        &root,
        KEY,
        ID,
        10,
        &[record(0, 0, 0, 10)],
        &store,
        false
    ));
    store.set_layout(
        TypeSettings {
            size: FontSize::Large,
            ..TypeSettings::DEFAULT
        },
        false,
    );
    let anchor = |page| reader_cache_files::read_v2_page_anchor(&root, KEY, ID, &store, page).place;
    assert_eq!(
        anchor(SEED + 9),
        Some((5, reader_store::page_fraction(9, 12)))
    );
    // The stretch before the run pins the run's start; past it, nothing.
    assert_eq!(anchor(SEED - 1), Some((5, 0)));
    assert_eq!(anchor(SEED + 12), None);
    // Prefix pages pin as before.
    assert_eq!(anchor(5), Some((0, reader_store::page_fraction(5, 10))));
}

/// An image block (an image token, with the image's height in the record)
/// survives the firmware's section writer and loader, and draws from the
/// loaded store as the image it names. The firmware sink that builds such
/// blocks comes later; here the record's height is set by hand.
#[test]
fn an_image_block_round_trips_through_a_section_file() {
    use display::font::{FontStyle, TypeSettings};
    use proto::text::{TextAlign, TextRole};
    for portrait in [false, true] {
        let disk = fat::Disk::new();
        let manager: VolumeManager<_, _, 8, 8, 1> =
            VolumeManager::new_with_limits(&disk, fat::Clock, 0);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let mut store = ReaderStore::new();
        store.set_layout(TypeSettings::DEFAULT, portrait);
        let page_box = ui::reading::ReadingBlocks::page_box(&store);
        let (width, height) = ui::reading::fit_image(page_box, 640, 900);
        let mut token = [0u8; proto::image::token::IMAGE_TOKEN_MAX_BYTES];
        let token =
            proto::image::token::encode_image_token(7, width, Some("A &amp; B"), &mut token)
                .expect("token fits");
        let (regular, body) = (FontStyle::Regular, TextRole::Body);
        assert!(store.push_line_block(
            "Before the figure",
            regular,
            body,
            TextAlign::Left,
            true,
            0
        ));
        assert!(store.push_line_block(token, regular, body, TextAlign::Center, true, 0));
        store.blocks[1].image_height = height;
        assert!(store.push_line_block("After the figure", regular, body, TextAlign::Left, true, 0));
        reader_layout::rebuild_page_index(&mut store);
        store.set_cached_spine(0);
        assert!(reader_cache_files::write_v2_section_cache(
            &root, KEY, ID, 0, &store
        ));

        let mut loaded = ReaderStore::new();
        loaded.set_layout(TypeSettings::DEFAULT, portrait);
        assert!(matches!(
            reader_cache_files::load_v2_section_cache(
                &root,
                KEY,
                ID,
                0,
                0,
                store.page_count,
                &mut loaded
            ),
            CacheLoadResult::Hit {
                repaginated: false,
                ..
            }
        ));
        assert_eq!(loaded.block_count, 3);
        assert_eq!(loaded.page_count, store.page_count);
        assert_eq!(
            loaded.pages[..loaded.page_count],
            store.pages[..store.page_count]
        );
        for index in 0..3 {
            assert_eq!(loaded.blocks[index], store.blocks[index], "block {index}");
            assert_eq!(loaded.block_text(index), store.block_text(index));
            assert_eq!(
                loaded.block_paragraph_end[index],
                store.block_paragraph_end[index]
            );
        }
        assert_eq!(loaded.blocks[1].image_height, height);

        let mut seen = Vec::new();
        for page in 0..loaded.page_count {
            ui::reading::for_each_drawable_block(&loaded, loaded.pages[page], |block| {
                if let Some(image) = block.image {
                    seen.push((image.slot, image.width, image.height, image.alt.to_string()));
                }
                true
            });
        }
        assert_eq!(seen, [(Some(7), width, height, "A & B".to_string())]);
    }
}

/// A fixture image block: one in five fills most of a page.
fn append_fixture_image(store: &mut ReaderStore, spine: u16, image: usize) {
    let page_box = ui::reading::ReadingBlocks::page_box(store);
    let (src_w, src_h) = if image % 5 == 4 {
        (640, 900)
    } else {
        (300, 90)
    };
    let (width, height) = ui::reading::fit_image(page_box, src_w, src_h);
    let alt = format!("Figure {spine}.{image}");
    assert!(store.push_image_block(height, spine, |out| {
        proto::image::token::encode_image_token(image as u16, width, Some(&alt), out).map(str::len)
    }));
    reader_layout::extend_page_index(store);
}

/// A fixture line holding inline images (a symbol, and on some lines a
/// 64-row initial), with its record marked as the line builder marks it.
fn append_fixture_inline_line(store: &mut ReaderStore, spine: u16, line: usize) {
    use proto::image::token::{encode_inline_image_token, InlineImageToken};
    let mut tokens = [[0u8; proto::image::token::INLINE_IMAGE_TOKEN_BYTES]; 2];
    let [symbol, initial] = &mut tokens;
    let symbol = encode_inline_image_token(
        InlineImageToken {
            slot: 40 + line as u16,
            width: 17,
            height: 12,
        },
        symbol,
    );
    let tall = line % 3 == 0;
    let initial = encode_inline_image_token(
        InlineImageToken {
            slot: 90,
            width: 64,
            height: if tall { 64 } else { 30 },
        },
        initial,
    );
    let text = format!("{initial}ine {line:03} of chapter {spine}, the {symbol} AEBG");
    assert!(store.push_line_block(
        &text,
        display::font::FontStyle::Regular,
        proto::text::TextRole::Body,
        proto::text::TextAlign::Justify,
        line % 7 == 6,
        spine
    ));
    store.mark_last_block_inline_images(if tall { 64 } else { 30 });
    reader_layout::extend_page_index(store);
}

/// A chapter of lines with an image every few lines, and lines holding
/// inline images among them.
fn append_fixture_chapter(store: &mut ReaderStore, spine: u16, block: usize) {
    if block % 9 == 4 {
        append_fixture_image(store, spine, block / 9);
    } else if block % 5 == 2 {
        append_fixture_inline_line(store, spine, block);
    } else {
        append_fixture_line(store, spine, block);
    }
}

/// Every block in reading order: its text, image height, and paragraph
/// flags.
fn block_text(store: &ReaderStore) -> Vec<(String, u16, bool, bool)> {
    (0..store.block_count)
        .map(|index| {
            (
                store.block_text(index).to_string(),
                store.blocks[index].image_height,
                store.block_paragraph_start[index],
                store.block_paragraph_end[index],
            )
        })
        .collect()
}

/// Image blocks and lines holding inline images through the page windows a
/// build flushes, the pages they carry, section files, and a spacing
/// reflow, which must lay them out as a fresh build would.
#[test]
fn image_blocks_keep_their_height_through_windows_carries_files_and_the_reflow() {
    use display::font::{LineSpacing, TypeSettings};
    for portrait in [false, true] {
        let disk = fat::Disk::new();
        let manager: VolumeManager<_, _, 8, 8, 1> =
            VolumeManager::new_with_limits(&disk, fat::Clock, 0);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let mut store = ReaderStore::new();
        let compact = TypeSettings {
            spacing: LineSpacing::Compact,
            ..TypeSettings::DEFAULT
        };
        store.set_layout(compact, portrait);
        let mut records = Vec::new();
        let mut total = 0u32;
        let mut carried_images = 0;
        let mut written = Vec::new();
        for spine in 0..2u16 {
            store.clear_lines();
            for block in 0..120 {
                append_fixture_chapter(&mut store, spine, block);
                let last = block == 119;
                // Cut right after an image too: the window's last page is
                // carried, and an image is on it.
                let image = store.blocks[store.block_count - 1].is_image();
                if store.page_count >= 3 || (image && store.page_count > 1) || last {
                    store.set_cached_spine(spine);
                    assert!(reader_layout::flush_page_window(
                        &mut store,
                        !last,
                        |window| {
                            written.extend(block_text(window));
                            let section = records.len() as u16;
                            if !reader_cache_files::write_v2_section_cache(
                                &root, KEY, ID, section, window,
                            ) {
                                return false;
                            }
                            records.push(BookV2SectionRecord {
                                section,
                                spine,
                                start_page: total,
                                page_count: window.page_count as u16,
                                partial: false,
                            });
                            total += window.page_count as u32;
                            true
                        }
                    ));
                    // The unfinished last page moves to the front of the
                    // next window, its images as they were.
                    carried_images += (0..store.block_count)
                        .filter(|&index| store.blocks[index].is_image())
                        .count();
                }
            }
        }
        assert!(carried_images > 0, "an image rode a carried page");
        assert!(records.len() > 3);
        assert!(write_index(&root, KEY, ID, total, &records, &store, false));

        // The section files hold every block as the windows wrote it.
        let mut loaded = Vec::new();
        for record in &records {
            assert!(matches!(
                reader_cache_files::load_v2_section_cache(
                    &root,
                    KEY,
                    ID,
                    record.section,
                    record.spine,
                    record.page_count as usize,
                    &mut store
                ),
                CacheLoadResult::Hit { .. }
            ));
            loaded.extend(block_text(&store));
        }
        assert_eq!(loaded, written);
        assert!(loaded.iter().filter(|block| block.1 > 0).count() >= 2 * 13);
        let inline_lines = |blocks: &[(String, u16, bool, bool)]| {
            blocks
                .iter()
                .filter(|block| block.1 & proto::cache::BlockRecord::INLINE_IMAGES != 0)
                .count()
        };
        assert!(inline_lines(&loaded) >= 2 * 20, "{}", inline_lines(&loaded));

        // A spacing change reflows them unchanged, into the pages a fresh
        // layout makes.
        let relaxed = TypeSettings {
            spacing: LineSpacing::Relaxed,
            ..TypeSettings::DEFAULT
        };
        store.set_layout(relaxed, portrait);
        assert_eq!(
            reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
            BookIndexLoadResult::SpacingMismatch
        );
        let mut scratch = vec![0u8; 24576];
        assert!(reader_cache_files::reflow_book_spacing(
            &root,
            KEY,
            ID,
            &mut store,
            &mut scratch
        ));
        let mut reflowed = Vec::new();
        let mut pages = Vec::new();
        for record in store.book_sections.iter().collect::<Vec<_>>() {
            assert!(matches!(
                reader_cache_files::load_v2_section_cache(
                    &root,
                    KEY,
                    ID,
                    record.section,
                    record.spine,
                    record.page_count as usize,
                    &mut store
                ),
                CacheLoadResult::Hit {
                    repaginated: false,
                    ..
                }
            ));
            reflowed.extend(block_text(&store));
            pages.extend(page_text(&store));
        }
        // Lines holding images keep their tokens and their height: the
        // reflow carries them unchanged.
        assert_eq!(reflowed, written, "portrait={portrait}");
        assert_eq!(inline_lines(&reflowed), inline_lines(&written));
        let mut expected = Vec::new();
        for spine in 0..2u16 {
            let mut fresh = ReaderStore::new();
            fresh.set_layout(relaxed, portrait);
            for block in 0..120 {
                append_fixture_chapter(&mut fresh, spine, block);
            }
            expected.extend(page_text(&fresh));
        }
        assert_eq!(pages, expected, "portrait={portrait}");
    }
}

/// Rewrites the book's committed index as a v27 index: the legacy
/// `BOOK.BIN` (no generation footer), with the version a v27 writer put.
fn make_index_v27(root: &embedded_sdmmc::Directory<'_, &fat::Disk, fat::Clock, 8, 8, 1>) {
    let x = root.open_dir(proto::cache::CACHE_ROOT_DIR).unwrap();
    let c = x.open_dir(proto::cache::CACHE_V2_DIR).unwrap();
    let b = c.open_dir(KEY).unwrap();
    let mut body = Vec::new();
    for name in ["BOOKA.BIN", "BOOKB.BIN"] {
        if let Ok(file) = b.open_file_in_dir(name, embedded_sdmmc::Mode::ReadOnly) {
            body = vec![0u8; file.length() as usize - 16];
            assert_eq!(file.read(&mut body).unwrap(), body.len());
        }
    }
    assert!(!body.is_empty());
    body[4..6].copy_from_slice(&27u16.to_le_bytes());
    let legacy = b
        .open_file_in_dir("BOOK.BIN", embedded_sdmmc::Mode::ReadWriteCreateOrTruncate)
        .unwrap();
    legacy.write(&body).unwrap();
    legacy.close().unwrap();
    for name in ["BOOKA.BIN", "BOOKB.BIN"] {
        let _ = sd_session::remove_file_reclaiming_clusters(&b, name);
    }
}

#[test]
fn a_v27_index_is_checked_once_then_written_current_or_rebuilt() {
    use display::font::{LineSpacing, TypeSettings};
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let sections = [record(0, 0, 0, 4), record(1, 1, 4, 3)];
    let fresh = |store: &mut ReaderStore| {
        *store = ReaderStore::new();
        store.set_book_labels("A Book", "An Author");
    };
    let mut store = ReaderStore::new();
    fresh(&mut store);
    assert_eq!(
        reader_cache_files::prepare_book_replacement(&root, KEY),
        Some(0)
    );
    assert!(write_index(&root, KEY, ID, 7, &sections, &store, false));
    make_index_v27(&root);
    let load = |store: &mut ReaderStore| {
        fresh(store);
        reader_cache_files::load_v2_book_index(&root, KEY, ID, store)
    };
    let settle = |store: &mut ReaderStore, has_images, spacing_changed| {
        reader_cache_files::settle_index_before_images(
            &root,
            KEY,
            ID,
            store,
            has_images,
            spacing_changed,
            5,
            None,
        )
    };
    let before = BookIndexLoadResult::BeforeImages {
        spacing_changed: false,
    };
    assert_eq!(load(&mut store), before);
    assert_eq!(store.book_total_pages, 7, "loaded like a hit");

    // A book that could not be read is used as it stands, and checked
    // again next time.
    assert_eq!(settle(&mut store, None, false), BookIndexLoadResult::Hit);
    assert_eq!(load(&mut store), before);

    // One with images: its place is pinned and the index dropped, so it
    // rebuilds. The old index stays readable until a new one commits.
    assert_eq!(
        settle(&mut store, Some(true), false),
        BookIndexLoadResult::Invalid
    );
    assert!(store.book_sections.is_empty());
    assert!(store.anchors(ID, 5));
    assert_eq!(load(&mut store), before);

    // A spacing change waits for the check too: with no images the reflow
    // goes ahead (and writes the index as it always does).
    store.set_layout(
        TypeSettings {
            spacing: LineSpacing::Relaxed,
            ..TypeSettings::DEFAULT
        },
        false,
    );
    let spacing = |store: &mut ReaderStore| {
        let settings = store.type_settings();
        *store = ReaderStore::new();
        store.set_layout(settings, false);
        reader_cache_files::load_v2_book_index(&root, KEY, ID, store)
    };
    assert_eq!(
        spacing(&mut store),
        BookIndexLoadResult::BeforeImages {
            spacing_changed: true
        }
    );
    assert_eq!(
        settle(&mut store, Some(false), true),
        BookIndexLoadResult::SpacingMismatch
    );
    assert_eq!(
        spacing(&mut store),
        BookIndexLoadResult::BeforeImages {
            spacing_changed: true
        }
    );
    assert_eq!(
        settle(&mut store, None, true),
        BookIndexLoadResult::Invalid,
        "no reflow on an unchecked index"
    );
    store.set_layout(TypeSettings::DEFAULT, false);

    // No images: written again as the current version, and nothing else
    // changes.
    assert_eq!(load(&mut store), before);
    assert_eq!(
        settle(&mut store, Some(false), false),
        BookIndexLoadResult::Hit
    );
    assert_eq!(load(&mut store), BookIndexLoadResult::Hit);
    assert_eq!(store.book_total_pages, 7);
    assert_eq!(
        store.book_sections.iter().collect::<Vec<_>>(),
        sections.to_vec()
    );
    assert_eq!(store.title.as_str(), "A Book");

    // An unchecked prefix is never built on.
    store.set_book_next_spine(Some(2));
    assert!(write_index(&root, KEY, ID, 7, &sections, &store, false));
    make_index_v27(&root);
    assert_eq!(load(&mut store), before);
    assert_eq!(store.book_next_spine(), Some(2));
    assert_eq!(
        settle(&mut store, None, false),
        BookIndexLoadResult::Invalid
    );
}

/// The one-time check on a v27 index counts JPEGs as well as PNGs, except
/// a lone image named as a cover: nearly every book has one, so a book
/// whose only image is its cover is written as the current version and
/// keeps its sections. A cover and any other image, or one image that is
/// no cover, rebuild. A fresh build still counts the cover, so a relayout
/// shows it.
#[test]
fn a_v27_book_rebuilds_for_images_beyond_a_lone_cover() {
    let cases: [(&[&str], bool); 5] = [
        (&["OEBPS/images/cover.jpg"], false),
        (&["OEBPS/images/Cover.PNG"], false),
        (
            &["OEBPS/images/cover.jpg", "OEBPS/images/plate01.jpg"],
            true,
        ),
        (&["OEBPS/images/plate01.jpg"], true),
        (&["OEBPS/images/cover.svg"], false),
    ];
    for (images, rebuild) in cases {
        let disk = fat::Disk::new();
        let manager: VolumeManager<_, _, 8, 8, 1> =
            VolumeManager::new_with_limits(&disk, fat::Clock, 0);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let sections = [record(0, 0, 0, 4), record(1, 1, 4, 3)];
        let mut store = ReaderStore::new();
        assert_eq!(
            reader_cache_files::prepare_book_replacement(&root, KEY),
            Some(0)
        );
        assert!(write_index(&root, KEY, ID, 7, &sections, &store, false));
        make_index_v27(&root);
        store = ReaderStore::new();
        assert_eq!(
            reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
            BookIndexLoadResult::BeforeImages {
                spacing_changed: false
            }
        );

        // The check the firmware makes on such an index: the ZIP's central
        // directory, through the open.
        let mut files: Vec<(&str, bool, &[u8])> = vec![
            ("OEBPS/cover.xhtml", false, b"<img src='images/cover.jpg'/>"),
            ("OEBPS/ch01.xhtml", false, b"<p>Text</p>"),
        ];
        for name in images {
            files.push((name, false, &[0xff, 0xd8, 0xff, 0xe0]));
        }
        let (bytes, _) = zip::zip(&files);
        let reads = std::cell::RefCell::new(Vec::new());
        let fails = std::cell::Cell::new(None);
        let mut tail = [0u8; 4096];
        let mut zip = proto::epub::ZipStream::new(
            zip::Archive {
                bytes: &bytes,
                reads: &reads,
                fails: &fails,
            },
            &mut tail,
        )
        .unwrap();
        let beyond = book_images::book_has_images_beyond_a_cover(&mut zip, &mut [0u8; 512]);
        assert_eq!(beyond, rebuild, "{images:?}");
        // A fresh build of the same book counts every image, the cover too.
        let svg_only = images == ["OEBPS/images/cover.svg"];
        assert_eq!(
            book_images::book_has_images(&mut zip, &mut [0u8; 512]),
            !svg_only,
            "{images:?}"
        );
        let settled = reader_cache_files::settle_index_before_images(
            &root,
            KEY,
            ID,
            &mut store,
            Some(beyond),
            false,
            5,
            None,
        );
        if rebuild {
            // Pinned to the reader's place and rebuilt; the old index stays
            // readable until the new one commits.
            assert_eq!(settled, BookIndexLoadResult::Invalid, "{images:?}");
            assert!(store.book_sections.is_empty());
            assert!(store.anchors(ID, 5));
            continue;
        }
        assert_eq!(settled, BookIndexLoadResult::Hit, "{images:?}");
        store = ReaderStore::new();
        assert_eq!(
            reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
            BookIndexLoadResult::Hit,
            "written current, with its sections: nothing to rebuild"
        );
        assert_eq!(
            store.book_sections.iter().collect::<Vec<_>>(),
            sections.to_vec()
        );
        assert!(!store.book_has_images());
    }
}

#[test]
fn the_book_index_remembers_whether_the_book_has_images() {
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let sections = [record(0, 0, 0, 4)];
    assert_eq!(
        reader_cache_files::prepare_book_replacement(&root, KEY),
        Some(0)
    );
    for has_images in [true, false, true] {
        let mut store = ReaderStore::new();
        store.set_book_has_images(has_images);
        store.set_book_next_spine(Some(1));
        assert!(write_index(&root, KEY, ID, 4, &sections, &store, false));
        let mut loaded = ReaderStore::new();
        assert_eq!(
            reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut loaded),
            BookIndexLoadResult::Hit
        );
        // A resumed build reads it here instead of walking the ZIP again.
        assert_eq!(loaded.book_has_images(), has_images);
        assert_eq!(loaded.book_next_spine(), Some(1));
        loaded.clear_book_index();
        assert!(!loaded.book_has_images());
    }
}
