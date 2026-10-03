#![allow(dead_code, unused_imports)]
extern crate self as esp_println;
extern crate self as hal_ext;
#[path = "../../hal-ext/src/nvm.rs"]
pub mod nvm;
pub use std::println;
#[path = "support/fat.rs"]
mod fat;
#[path = "../../fw/src/reader_cache_files.rs"]
mod reader_cache_files;
#[path = "../../fw/src/reader_layout.rs"]
mod reader_layout;
#[path = "../../fw/src/reader_store.rs"]
mod reader_store;
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
use reader_store::ReaderStore;
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
    assert!(reader_cache_files::write_v2_book_index(
        &root, KEY, ID, 1, &sections, &store, false
    ));
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
        BookIndexLoadResult::Hit
    );
    assert_eq!(
        reader_cache_files::prepare_book_replacement(&root, KEY),
        Some(320)
    );
    sections[0].section = 320;
    sections[0].page_count = 2;
    assert!(reader_cache_files::write_v2_book_index(
        &root, KEY, ID, 2, &sections, &store, false
    ));
    assert_eq!(
        reader_cache_files::load_v2_book_index(&root, KEY, ID, &mut store),
        BookIndexLoadResult::Hit
    );
    assert_eq!(store.book_total_pages, 2);
    assert_eq!(store.book_sections[0].section, 320);
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
    assert!(reader_cache_files::write_v2_book_index(
        &root,
        KEY,
        ID,
        1,
        &[section],
        &store,
        false
    ));
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
            let _ = reader_cache_files::write_v2_book_index(
                &root,
                KEY,
                ID,
                2,
                &[section],
                &store,
                false,
            );
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
                    let _ = reader_cache_files::write_v2_book_index(
                        &root,
                        KEY,
                        ID,
                        2,
                        &[section],
                        &store,
                        false,
                    );
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
        assert!(reader_cache_files::write_v2_book_index(
            &root, KEY, ID, total, &records, &store, false
        ));
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
            let sections = store.book_sections[..store.book_section_count].to_vec();
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
    assert!(reader_cache_files::write_v2_book_index(
        &root,
        KEY,
        ID,
        2,
        &[section],
        &store,
        false
    ));
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
    let header = reader_cache_files::read_cache_header(&root, KEY).unwrap();
    assert_eq!(
        header.source_hash, ID.0,
        "sweep must not trust the corrupted newer identity"
    );
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
        Some(320)
    );
    store.set_book_next_spine(Some(1));
    store.set_book_generated_toc(true);
    let section = BookV2SectionRecord {
        section: 320,
        spine: 0,
        start_page: 0,
        page_count: 1,
        partial: false,
    };
    assert!(reader_cache_files::write_v2_book_index(
        &root,
        KEY,
        ID,
        1,
        &[section],
        &store,
        false
    ));
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
    assert!(reader_cache_files::write_v2_book_index(
        &root,
        KEY,
        ID,
        1,
        &[section],
        &store,
        false
    ));
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
    assert!(reader_cache_files::write_v2_book_index(
        &root,
        KEY,
        ID,
        2,
        &[section],
        &store,
        false
    ));
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
    assert!(reader_cache_files::write_v2_book_index(
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
        assert!(reader_cache_files::write_v2_book_index(
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
        let sections = store.book_sections[..store.book_section_count].to_vec();
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
    assert!(reader_cache_files::write_v2_book_index(
        &root,
        KEY,
        ID,
        1,
        &[section],
        &store,
        false
    ));
    let second_id = (ID.0 + 1, ID.1 + 1);
    store.set_book_labels("Second source", "Author");
    assert!(reader_cache_files::write_v2_book_index(
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
    ui::reading::draw_reading_page_body(&mut fb, &store, store.pages[0]);
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
