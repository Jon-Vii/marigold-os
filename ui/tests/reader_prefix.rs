//! Exercise the firmware's prefix read model, including its ordinary cache
//! lookup path: future pages must never alias the last committed section.
#[allow(dead_code, unused_imports)]
#[path = "../../fw/src/reader_layout.rs"]
mod reader_layout;
#[allow(dead_code)]
#[path = "../../fw/src/reader_store.rs"]
mod reader_store;

use display::font::{LineSpacing, TypeSettings};
use proto::cache::BookV2SectionRecord;
use reader_store::ReaderStore;

fn section(id: u16, spine: u16, start: u32, pages: u16) -> BookV2SectionRecord {
    BookV2SectionRecord {
        section: id,
        spine,
        start_page: start,
        page_count: pages,
        partial: false,
    }
}

#[test]
fn pending_prefix_targets_are_not_clamped_to_cached_tail() {
    let mut store = ReaderStore::new();
    let prefix = [section(320, 3, 0, 4), section(321, 3, 4, 2)];
    store.set_book_index(6, false, &prefix);
    store.set_book_next_spine(Some(4));
    assert_eq!(store.requested_page(5, None), Some(5));
    assert_eq!(store.requested_page(6, None), None);
    assert_eq!(store.requested_page(500, None), None);
    assert_eq!(store.section_for_global_page(5), Some(prefix[1]));
    assert_eq!(store.section_for_global_page(6), None);
    assert_eq!(store.section_for_global_page(500), None);
    assert_eq!(store.requested_page(0, Some(8)), None);
    assert_eq!(store.page_for_spine(8), u32::MAX);
    assert_eq!(store.requested_page(0, Some(3)), Some(0));
}

#[test]
fn appending_a_complete_spine_preserves_existing_pages_and_chapter_counts() {
    let mut store = ReaderStore::new();
    let sections = [
        section(0, 0, 0, 4),
        section(1, 0, 4, 3),
        section(2, 2, 7, 5),
    ];
    store.set_book_index(7, false, &sections[..2]);
    store.set_book_next_spine(Some(2));
    let earlier = store.chapter_page_position(5);
    assert_eq!(earlier, Some((6, 7)));
    assert_eq!(store.requested_page(7, None), None);
    store.set_book_index(12, false, &sections);
    store.set_book_next_spine(Some(3));
    assert_eq!(store.requested_page(7, None), Some(7));
    assert_eq!(store.requested_page(0, Some(2)), Some(7));
    assert_eq!(store.chapter_page_position(5), earlier);
    assert_eq!(store.section_for_global_page(5), Some(sections[1]));
}

#[test]
fn terminal_cache_retains_eof_clamping_without_advertising_a_resumable_tail() {
    let mut store = ReaderStore::new();
    store.set_book_index(4, true, &[section(0, 0, 0, 4)]);
    assert_eq!(store.book_next_spine(), None);
    assert_eq!(store.requested_page(4, None), Some(3));
    assert_eq!(store.requested_page(u32::MAX, None), Some(3));
    assert!(store.section_for_global_page(4).is_some());
    // A terminal, capacity-limited cache can be partial without being resumable.
    assert!(store.book_cache_partial);
}

#[test]
fn progress_cursor_invalidates_chapter_map_and_never_survives_layout_reset() {
    let mut store = ReaderStore::new();
    store.set_book_index(4, false, &[section(0, 0, 0, 4)]);
    store.set_book_next_spine(Some(1));
    store.chapter_page_count = 10;
    store.set_book_next_spine(Some(2));
    assert_eq!(store.chapter_page_count, 0);
    store.set_layout(
        TypeSettings {
            spacing: LineSpacing::Relaxed,
            ..TypeSettings::DEFAULT
        },
        false,
    );
    assert_eq!(store.book_next_spine(), None);
    store.set_book_next_spine(Some(3));
    store.set_book_generated_toc(true);
    store.begin_book_load();
    assert_eq!(store.book_next_spine(), None);
    assert!(!store.book_generated_toc());
    assert_eq!(store.requested_page(0, None), None);
}

#[test]
fn unknown_chapter_sentinel_reaches_the_navigation_event_map() {
    let mut store = ReaderStore::new();
    store.set_book_index(4, false, &[section(0, 1, 0, 4)]);
    store.set_book_next_spine(Some(2));
    assert!(store.push_toc_record("First", 1, 1));
    assert!(store.push_toc_record("Future", 1, 8));
    reader_layout::rebuild_toc_page_targets(&mut store);
    let pages = reader_store::chapter_pages_for_event(&store);
    assert_eq!(pages[0], 0);
    assert_eq!(pages[1], u16::MAX);
}
