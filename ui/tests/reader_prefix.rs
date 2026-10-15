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
use reader_store::{BookLoadStatus, ReaderStore};

#[test]
fn a_prefix_that_holds_the_requested_page_is_ready_before_the_rest() {
    let mut store = ReaderStore::new();
    let sections = [section(0, 0, 0, 7), section(1, 1, 7, 5)];
    store.set_book_index(7, false, &sections[..1]);
    store.set_book_next_spine(Some(1));
    store.set_current_section_range(0, 7);
    store.page_count = 7;
    // The first spine holds the page: show it, and prepare the rest while
    // the reader is idle.
    assert_eq!(store.requested_page(0, None), Some(0));
    store.finish_book_load(0, 0, BookLoadStatus::Ready);
    assert_eq!(store.reader_status(), BookLoadStatus::Ready);
    assert!(store.covers_global_page(0, 0));
    // Pages past the prefix still need preparation.
    assert_eq!(store.requested_page(7, None), None);
    assert!(!store.has_prepared_page(0, 7));

    store.set_book_index(12, false, &sections);
    store.set_book_next_spine(None);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);
    assert_eq!(store.reader_status(), BookLoadStatus::Ready);
    assert!(store.covers_global_page(0, 0));
    // Reading through the later section now requires cache reads, not parsing.
    for page in 0..12 {
        assert!(store.section_for_global_page(page).is_some());
    }
}

#[test]
fn interrupted_preparation_does_not_hide_errors_or_block_terminal_partial_books() {
    let mut store = ReaderStore::new();
    store.set_book_index(7, false, &[section(0, 0, 0, 7)]);
    store.set_book_next_spine(Some(1));
    store.finish_book_load(0, 0, BookLoadStatus::Error);
    assert_eq!(store.reader_status(), BookLoadStatus::Error);
    store.set_book_index(7, true, &[section(0, 0, 0, 7)]);
    store.set_book_next_spine(None);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);
    assert_eq!(store.reader_status(), BookLoadStatus::Ready);
}

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
fn prepared_sections_do_not_need_a_loading_plate_when_the_ram_window_changes() {
    let mut store = ReaderStore::new();
    store.set_book_index(12, false, &[section(0, 0, 0, 7), section(1, 1, 7, 5)]);
    store.set_current_section_range(0, 7);
    store.page_count = 7;
    store.finish_book_load(0, 0, BookLoadStatus::Ready);
    assert!(store.covers_global_page(0, 6));
    assert!(!store.covers_global_page(0, 7));
    assert!(store.has_prepared_page(0, 7));
    assert!(!store.has_prepared_page(1, 7));
    assert!(!store.has_prepared_page(0, 12));
    // A book still being prepared serves the pages it has.
    store.set_book_next_spine(Some(2));
    store.finish_book_load(0, 0, BookLoadStatus::Ready);
    assert!(store.has_prepared_page(0, 7));
    assert!(!store.has_prepared_page(0, 12));
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

#[test]
fn page_fractions_map_an_unchanged_pagination_back_exactly() {
    for pages in [1u32, 2, 3, 7, 20, 137, 1000, 32_767] {
        for offset in [0, pages / 3, pages / 2, pages - 1] {
            let fraction = reader_store::page_fraction(offset, pages);
            assert_eq!(
                reader_store::page_at_fraction(fraction, pages),
                offset,
                "offset {offset} of {pages}"
            );
        }
    }
}

const ANCHOR_SOURCE: (u32, u32) = (77, 9_000);

fn anchored_store() -> ReaderStore {
    let mut store = ReaderStore::new();
    store.set_active_entry(
        0,
        "Book",
        "BOOK.EPU",
        true,
        ANCHOR_SOURCE.1,
        ANCHOR_SOURCE.0,
        None,
    );
    store
}

#[test]
fn a_relaid_book_keeps_the_place_rather_than_the_page_number() {
    // Old layout: spine 1 runs pages 10..30 across two sections.
    let old = [
        section(0, 0, 0, 10),
        section(1, 1, 10, 12),
        section(2, 1, 22, 8),
        section(3, 2, 30, 5),
    ];
    let mut store = anchored_store();
    store.set_book_index(35, false, &old);
    store.anchor_resident_page(ANCHOR_SOURCE, 20);

    // Larger type: spine 1 now runs pages 14..44. Page 20 sat just past the
    // middle of the spine (10.5 of 20 pages), so the reader lands at 15.75
    // of 30, page 29, not on page 20 near the spine's start.
    let new = [
        section(0, 0, 0, 14),
        section(1, 1, 14, 16),
        section(2, 1, 30, 14),
        section(3, 2, 44, 7),
    ];
    store.set_book_index(51, false, &new);
    assert_eq!(store.requested_page(20, None), Some(29));
    // Only the pinned request translates; spine jumps and other pages don't.
    assert_eq!(store.requested_page(21, None), Some(21));
    assert_eq!(store.requested_page(20, Some(2)), Some(44));

    // Another book's request for the same number stays a raw page.
    store.set_active_entry(1, "Other", "OTHER.EPU", true, 1, 2, None);
    assert_eq!(store.requested_page(20, None), Some(20));
    store.set_active_entry(
        0,
        "Book",
        "BOOK.EPU",
        true,
        ANCHOR_SOURCE.1,
        ANCHOR_SOURCE.0,
        None,
    );

    // A spent pin leaves page numbers alone.
    store.set_page_anchor(None);
    assert_eq!(store.requested_page(20, None), Some(20));
}

#[test]
fn a_pin_waits_for_its_spine_and_survives_the_rebuild_steps() {
    let mut store = anchored_store();
    store.set_book_index(35, false, &[section(0, 0, 0, 10), section(1, 1, 10, 20)]);
    store.anchor_resident_page(ANCHOR_SOURCE, 15);

    // The rebuild has laid out spine 0 only: the place is not known yet.
    store.set_book_index(14, false, &[section(0, 0, 0, 14)]);
    store.set_book_next_spine(Some(1));
    assert_eq!(store.requested_page(15, None), None);

    // A later step re-pins the same request; the first pin must stand, as
    // the resident sections no longer hold the old pagination.
    store.anchor_resident_page(ANCHOR_SOURCE, 15);
    assert!(store.anchors(ANCHOR_SOURCE, 15));
    store.set_book_index(44, false, &[section(0, 0, 0, 14), section(1, 1, 14, 30)]);
    store.set_book_next_spine(None);
    // 5.5 of 20 pages through spine 1 is 8.25 of 30: page 14 + 8.
    assert_eq!(store.requested_page(15, None), Some(22));
}

#[test]
fn a_spine_that_laid_out_empty_lands_on_the_following_text() {
    let mut store = anchored_store();
    store.set_book_index(
        12,
        false,
        &[
            section(0, 0, 0, 4),
            section(1, 1, 4, 4),
            section(2, 2, 8, 4),
        ],
    );
    store.anchor_resident_page(ANCHOR_SOURCE, 5);
    store.set_book_index(10, false, &[section(0, 0, 0, 6), section(1, 2, 6, 4)]);
    assert_eq!(store.requested_page(5, None), Some(6));
}

#[test]
fn a_first_open_lands_where_the_text_starts() {
    // Cover, title, and foreword (spines 0-2, five pages) before the text.
    let sections = [
        section(0, 0, 0, 1),
        section(1, 1, 1, 1),
        section(2, 2, 2, 3),
        section(3, 3, 5, 20),
        section(4, 4, 25, 15),
    ];
    let mut store = anchored_store();
    store.set_book_index(40, false, &sections);
    store.set_text_start_spine(Some(3));
    assert_eq!(
        store.requested_page(0, None),
        Some(0),
        "only a first open seeks"
    );

    store.seek_text_start(ANCHOR_SOURCE);
    assert_eq!(store.requested_page(0, None), Some(5));
    // Pages other than the opening request, and spine jumps, are untouched.
    assert_eq!(store.requested_page(7, None), Some(7));
    assert_eq!(store.requested_page(0, Some(1)), Some(1));

    // Another book's first open does not borrow this book's start.
    store.set_active_entry(1, "Other", "OTHER.EPU", true, 1, 2, None);
    assert_eq!(store.requested_page(0, None), Some(0));
    store.set_active_entry(
        0,
        "Book",
        "BOOK.EPU",
        true,
        ANCHOR_SOURCE.1,
        ANCHOR_SOURCE.0,
        None,
    );

    // Once the load completes, page 0 is the cover again.
    store.end_reader_request();
    assert_eq!(store.requested_page(0, None), Some(0));
}

#[test]
fn a_text_start_waits_for_its_spine_and_ignores_a_deep_marker() {
    let mut store = anchored_store();
    store.seek_text_start(ANCHOR_SOURCE);
    store.set_book_index(5, false, &[section(0, 0, 0, 5)]);
    store.set_book_next_spine(Some(1));
    store.set_text_start_spine(Some(1));
    assert_eq!(store.requested_page(0, None), None, "not laid out yet");

    // A marker past the first quarter of the book is not trusted.
    store.set_book_index(40, false, &[section(0, 0, 0, 12), section(1, 1, 12, 28)]);
    store.set_book_next_spine(None);
    store.set_text_start_spine(Some(1));
    assert_eq!(store.requested_page(0, None), Some(0));
}

#[test]
fn the_section_table_derives_ids_and_start_pages() {
    let mut table: reader_store::SectionTable = reader_store::SectionTable::new();
    assert!(table.push(section(1024, 0, 0, 5)));
    assert!(table.push(BookV2SectionRecord {
        partial: true,
        ..section(1025, 0, 5, 3)
    }));
    assert!(table.push(section(1026, 2, 8, 7)));
    // A gap in ids or pages is not this book's next section.
    assert!(!table.push(section(1028, 3, 15, 1)));
    assert!(!table.push(section(1027, 3, 16, 1)));
    assert_eq!(table.len(), 3);
    let records: Vec<_> = table.iter().collect();
    assert_eq!(
        records,
        [
            section(1024, 0, 0, 5),
            BookV2SectionRecord {
                partial: true,
                ..section(1025, 0, 5, 3)
            },
            section(1026, 2, 8, 7),
        ]
    );
    assert_eq!(table.last(), Some(section(1026, 2, 8, 7)));
}

#[test]
fn a_book_of_many_short_items_fits_well_past_the_old_section_cap() {
    // An essay collection with 900 one- and two-page spine items: every item
    // needs its own section, so the old 320-section table cut it short.
    let mut sections = Vec::new();
    let mut start = 0;
    for spine in 0..900u16 {
        let pages = 1 + spine % 2;
        sections.push(section(spine, spine, start, pages));
        start += u32::from(pages);
    }
    let mut store = ReaderStore::new();
    store.set_book_index(start, false, &sections);
    assert_eq!(store.book_sections.len(), 900);
    assert_eq!(store.page_for_spine(899), sections[899].start_page);
    assert_eq!(
        store.section_for_global_page(start - 1),
        Some(sections[899])
    );
    assert_eq!(
        store.chapter_page_position(sections[451].start_page + 1),
        Some((2, 2))
    );

    let mut table: reader_store::SectionTable = reader_store::SectionTable::new();
    for record in sections.iter().cycle().take(2000) {
        let next = BookV2SectionRecord {
            section: table.len() as u16,
            start_page: table
                .last()
                .map_or(0, |last| last.start_page + u32::from(last.page_count)),
            ..*record
        };
        if !table.push(next) {
            break;
        }
    }
    assert!(table.is_full());
    assert_eq!(table.len(), reader_store::MAX_BOOK_SECTIONS);
}

#[test]
fn a_line_is_hyphenated_only_once_moving_the_word_would_leave_it_loose() {
    use ui::reading::{worth_hyphenating, MAX_HYPHEN_RUN};
    let space = 6;
    // Ten gaps absorbing 20 px is 2 px each: move the word, no hyphen.
    assert!(!worth_hyphenating(20, 10, space, 0));
    // 40 px over ten gaps is 4 px each, over half a space: hyphenate.
    assert!(worth_hyphenating(40, 10, space, 0));
    // A narrow portrait line with four gaps loosens much sooner.
    assert!(worth_hyphenating(16, 4, space, 0));
    // After a ladder of hyphens it takes a doubled gap to add another.
    assert!(!worth_hyphenating(40, 10, space, MAX_HYPHEN_RUN));
    assert!(worth_hyphenating(70, 10, space, MAX_HYPHEN_RUN));
    // A lone word on its line cannot stretch, so it always tries.
    assert!(worth_hyphenating(1, 0, space, MAX_HYPHEN_RUN));
}

#[test]
fn justification_stops_short_of_gaping_word_spaces() {
    use ui::reading::{justify_extra, READER_WRAP_SAFETY};
    let space = 6;
    let right = 400 + READER_WRAP_SAFETY;
    // 30 px over six gaps (5 each, under 1.5 spaces) stretches.
    assert_eq!(justify_extra(0, 370, right, 6, space), Some(30));
    // The same 30 px over three gaps (10 each) would gape: natural spacing.
    assert_eq!(justify_extra(0, 370, right, 3, space), None);
    // Lines under two thirds of the measure never stretch.
    assert_eq!(justify_extra(0, 250, right, 40, space), None);
}

const SEED: u32 = reader_store::PROVISIONAL_PAGE_BASE;

/// A prefix through spine 1 (pages 0..20), and spine 5 laid out ahead of
/// it in a seed run: 12 provisional pages over two sections.
fn seeded_store() -> ReaderStore {
    let mut store = anchored_store();
    store.set_book_index(20, false, &[section(0, 0, 0, 10), section(1, 1, 10, 10)]);
    store.set_book_next_spine(Some(2));
    store.seed.first_spine = Some(5);
    store.seed.next_spine = Some(6);
    assert!(store.seed.sections.push(section(2048, 5, SEED, 8)));
    assert!(store.seed.sections.push(section(2049, 5, SEED + 8, 4)));
    store
}

#[test]
fn a_spine_laid_out_ahead_reads_provisionally_until_the_prefix_reaches_it() {
    let mut store = seeded_store();
    assert_eq!(store.requested_page(0, Some(5)), Some(SEED));
    assert_eq!(store.requested_page(SEED + 9, None), Some(SEED + 9));
    assert_eq!(
        store.section_for_global_page(SEED + 9).map(|s| s.section),
        Some(2049)
    );
    assert_eq!(store.chapter_page_position(SEED + 9), Some((10, 12)));
    assert_eq!(store.page_for_spine(5), SEED);
    assert_eq!(store.page_for_spine(3), u32::MAX);
    // Past the run, or in the gap before it: still to be prepared, by the
    // run and by the prefix respectively.
    assert_eq!(store.requested_page(SEED + 12, None), None);
    assert_eq!(store.request_spine(SEED + 12, None), Some(6));
    assert_eq!(store.requested_page(SEED - 1, None), None);
    assert_eq!(store.request_spine(SEED - 1, None), None);
    assert_eq!(store.requested_page(0, Some(3)), None);
    // Real pages are untouched.
    assert_eq!(store.requested_page(15, None), Some(15));
    assert_eq!(store.settled_page(15), 15);

    // Once the prefix has laid out everything before the run, the pages
    // just before it are real, though the run's own still wait for it.
    store.set_book_index(
        40,
        false,
        &[
            section(0, 0, 0, 10),
            section(1, 1, 10, 10),
            section(2, 3, 20, 20),
        ],
    );
    store.set_book_next_spine(Some(5));
    assert_eq!(store.requested_page(SEED - 1, None), Some(39));
    assert_eq!(store.requested_page(SEED + 9, None), Some(SEED + 9));

    // The prefix lays out spines 2..5 (spine 5 the same as the run): the
    // provisional pages take their real numbers, before and inside.
    store.set_book_index(
        52,
        false,
        &[
            section(0, 0, 0, 10),
            section(1, 1, 10, 10),
            section(2, 3, 20, 20),
            section(3, 5, 40, 8),
            section(4, 5, 48, 4),
        ],
    );
    store.set_book_next_spine(Some(6));
    assert_eq!(store.settled_page(SEED + 9), 49);
    assert_eq!(store.requested_page(SEED + 9, None), Some(49));
    assert_eq!(store.settled_page(SEED - 1), 39);
    assert_eq!(store.chapter_page_position(SEED + 9), Some((10, 12)));
    assert_eq!(store.page_for_spine(5), 40);
    assert_eq!(
        store.section_for_global_page(SEED + 9).map(|s| s.section),
        Some(4)
    );
}

#[test]
fn without_a_seed_run_high_pages_are_plain_pages() {
    let mut store = ReaderStore::new();
    store.set_book_index(4, false, &[section(0, 0, 0, 4)]);
    assert!(!store.is_provisional(SEED + 1));
    assert_eq!(store.settled_page(SEED + 1), SEED + 1);
    assert_eq!(store.requested_page(SEED + 1, None), Some(3));
}

#[test]
fn a_place_pinned_in_a_seed_run_lands_in_the_relaid_run() {
    let mut store = seeded_store();
    store.anchor_resident_page(ANCHOR_SOURCE, SEED + 9);
    // Larger type: the prefix and run are laid out again from scratch.
    store.set_book_index(14, false, &[section(0, 0, 0, 14)]);
    store.set_book_next_spine(Some(1));
    store.seed.clear();
    assert_eq!(store.requested_page(SEED + 9, None), None);
    assert_eq!(store.request_spine(SEED + 9, None), Some(5));
    // Spine 5 is seeded again, now 16 pages: 9.5 of 12 is 12.67 of 16.
    store.seed.first_spine = Some(5);
    store.seed.next_spine = Some(6);
    assert!(store.seed.sections.push(section(2048, 5, SEED, 10)));
    assert!(store.seed.sections.push(section(2049, 5, SEED + 10, 6)));
    assert_eq!(store.requested_page(SEED + 9, None), Some(SEED + 12));
}

#[test]
fn a_seed_run_that_reached_the_end_clamps_like_a_book() {
    let mut store = seeded_store();
    store.seed.next_spine = None;
    assert_eq!(store.requested_page(SEED + 40, None), Some(SEED + 11));
}

#[test]
fn unknown_and_provisional_chapter_pages_go_unnumbered() {
    let mut store = seeded_store();
    assert!(store.push_toc_record("Two", 1, 1));
    assert!(store.push_toc_record("Three", 1, 3));
    assert!(store.push_toc_record("Five", 1, 5));
    reader_layout::rebuild_toc_page_targets(&mut store);
    let pages: Vec<u32> = (0..3).map(|i| store.toc_item(i).unwrap().page).collect();
    assert_eq!(pages, [11, 0, 0]);
}
