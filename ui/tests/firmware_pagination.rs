//! Exercise the firmware's bounded page index on the host, rather than a
//! second pagination implementation that only resembles the board path.
#[allow(dead_code, unused_imports)]
#[path = "../../fw/src/reader_layout.rs"]
mod reader_layout;
#[allow(dead_code)]
#[path = "../../fw/src/reader_store.rs"]
mod reader_store;

use display::font::{FontFamily, FontSize, FontWeight, LineSpacing, TypeSettings};
use proto::cache::BlockRecord;
use proto::text::{FontStyle, TextAlign, TextRole};
use reader_store::ReaderStore;
use ui::reading::{line_advance, page_record_at, paginate_block_pages, paragraph_gap};

fn append_block(store: &mut ReaderStore, role: TextRole) {
    let index = store.block_count;
    store.blocks[index] = BlockRecord {
        text_offset: 0,
        text_len: 0,
        line_count: 1,
        role,
        style: FontStyle::Regular,
        align: TextAlign::Left,
    };
    store.block_paragraph_end[index] = true;
    store.block_count += 1;
}

#[test]
fn firmware_keeps_a_heading_whose_ink_fits_at_the_page_edge() {
    let mut store = ReaderStore::new();
    let bounds = store.page_box();
    let body = line_advance(store.type_settings(), TextRole::Body);
    let heading = line_advance(store.type_settings(), TextRole::Heading1);
    let preceding = (bounds.bottom - bounds.top - heading) / body;
    for _ in 0..preceding {
        append_block(&mut store, TextRole::Body);
    }
    append_block(&mut store, TextRole::Heading1);

    assert_eq!(paginate_block_pages(&store), 1);
    reader_layout::rebuild_page_index(&mut store);
    assert_eq!(store.page_count, 1, "trailing spacing is not page ink");
    assert_eq!(store.pages[0], page_record_at(&store, 0));
}

#[test]
fn firmware_keeps_blockquote_ink_but_discards_its_trailing_page_gap() {
    for portrait in [false, true] {
        let mut store = ReaderStore::new();
        store.set_layout(TypeSettings::DEFAULT, portrait);
        let bounds = store.page_box();
        let advance = line_advance(store.type_settings(), TextRole::Body);
        let gap = paragraph_gap(TextRole::BlockQuote);
        let room = bounds.bottom - bounds.top;
        // Find a realistic mixture of quote/body lines that leaves room for
        // the final quote's ink, but not its trailing paragraph spacing.
        let (quotes, bodies) = (0..16)
            .flat_map(|quotes| (0..32).map(move |bodies| (quotes, bodies)))
            .find(|(quotes, bodies)| {
                let ink = quotes * (advance + gap) + bodies * advance + advance;
                ink <= room && ink + gap > room
            })
            .expect("edge-fitting fixture for each panel and orientation");
        for _ in 0..quotes {
            append_block(&mut store, TextRole::BlockQuote);
        }
        for _ in 0..bodies {
            append_block(&mut store, TextRole::Body);
        }
        append_block(&mut store, TextRole::BlockQuote);
        reader_layout::rebuild_page_index(&mut store);
        assert_eq!(store.page_count, 1, "portrait={portrait}");
        assert_eq!(usize::from(store.pages[0].block_count), store.block_count);
        assert_eq!(store.pages[0], page_record_at(&store, 0));
    }
}

#[test]
fn firmware_index_preserves_blocks_and_spines_across_layouts() {
    let mut store = ReaderStore::new();
    for index in 0..120 {
        let role = if index % 7 == 0 {
            TextRole::BlockQuote
        } else {
            TextRole::Body
        };
        append_block(&mut store, role);
        store.block_spine[index] = (index / 23) as u16;
        store.block_page_break_before[index] = index % 23 == 0;
    }
    for portrait in [false, true] {
        for size in [FontSize::Small, FontSize::Medium, FontSize::Large] {
            for spacing in [
                LineSpacing::Compact,
                LineSpacing::Normal,
                LineSpacing::Relaxed,
            ] {
                store.set_layout(
                    TypeSettings {
                        size,
                        spacing,
                        weight: FontWeight::Normal,
                        family: FontFamily::Literata,
                    },
                    portrait,
                );
                reader_layout::rebuild_page_index(&mut store);
                assert_eq!(store.page_count, paginate_block_pages(&store));
                let mut next_block = 0;
                for index in 0..store.page_count {
                    let page = store.pages[index];
                    assert_eq!(usize::from(page.first_block), next_block);
                    assert!(page.block_count > 0);
                    assert_eq!(store.page_spine[index], store.block_spine[next_block]);
                    assert_eq!(page, page_record_at(&store, index));
                    next_block += usize::from(page.block_count);
                    assert!(!store.block_page_break_before
                        [usize::from(page.first_block) + 1..next_block]
                        .iter()
                        .any(|forced| *forced));
                }
                assert_eq!(next_block, store.block_count);
                assert_eq!(
                    page_record_at(&store, usize::MAX),
                    store.pages[store.page_count - 1]
                );
            }
        }
    }
}

#[test]
fn firmware_index_is_bounded_and_empty_rebuild_clears_old_pages() {
    let mut store = ReaderStore::new();
    for index in 0..store.block_capacity() {
        append_block(&mut store, TextRole::Body);
        store.block_page_break_before[index] = true;
        store.block_spine[index] = index as u16;
    }
    reader_layout::rebuild_page_index(&mut store);
    assert_eq!(store.page_count, store.page_capacity());
    for index in 0..store.page_count {
        assert_eq!(store.pages[index].first_block, index as u16);
        assert_eq!(store.pages[index].block_count, 1);
        assert_eq!(store.page_spine[index], index as u16);
    }
    store.block_count = 0;
    reader_layout::rebuild_page_index(&mut store);
    assert_eq!(store.page_count, 0);
    assert_eq!(paginate_block_pages(&store), 1);
    assert_eq!(page_record_at(&store, 0).block_count, 0);
}

#[test]
fn incremental_index_matches_full_walk_during_append_and_section_carry() {
    let mut store = ReaderStore::new();
    for portrait in [false, true] {
        for spacing in [
            LineSpacing::Compact,
            LineSpacing::Normal,
            LineSpacing::Relaxed,
        ] {
            store.clear_lines();
            store.set_layout(
                TypeSettings {
                    spacing,
                    ..TypeSettings::DEFAULT
                },
                portrait,
            );
            for step in 0..700 {
                let index = store.block_count;
                append_block(
                    &mut store,
                    if step % 7 == 0 {
                        TextRole::BlockQuote
                    } else {
                        TextRole::Body
                    },
                );
                store.block_paragraph_end[index] = false;
                store.block_spine[index] = (step / 23) as u16;
                store.block_page_break_before[index] = step % 23 == 0;
                reader_layout::extend_page_index(&mut store);
                assert_incremental_index(&store);
                // A later parser fragment can end a paragraph without adding
                // a block. Its new gap must affect the next appended line.
                if step % 3 == 0 {
                    store.mark_last_block_paragraph_end();
                    reader_layout::extend_page_index(&mut store);
                    assert_incremental_index(&store);
                }
                if store.page_count >= 12 {
                    let cut = usize::from(store.pages[store.page_count - 1].first_block);
                    store.carry_last_page(cut);
                    reader_layout::rebuild_page_index(&mut store);
                    assert_incremental_index(&store);
                }
            }
        }
    }
}

fn assert_incremental_index(store: &ReaderStore) {
    assert_eq!(
        store.page_count,
        paginate_block_pages(store).min(store.page_capacity())
    );
    for index in 0..store.page_count {
        assert_eq!(store.pages[index], page_record_at(store, index));
        assert_eq!(
            store.page_spine[index],
            store.block_spine[usize::from(store.pages[index].first_block)]
        );
    }
}

#[test]
fn incremental_index_respects_capacity_and_clear() {
    let mut store = ReaderStore::new();
    for index in 0..store.block_capacity() {
        append_block(&mut store, TextRole::Heading1);
        store.block_page_break_before[index] = true;
        reader_layout::extend_page_index(&mut store);
        assert_incremental_index(&store);
    }
    store.clear_lines();
    reader_layout::extend_page_index(&mut store);
    assert_eq!(store.page_count, 0);
    append_block(&mut store, TextRole::Body);
    reader_layout::extend_page_index(&mut store);
    assert_incremental_index(&store);
}

/// Run explicitly in release mode; timings are observations, never CI gates.
#[test]
#[ignore = "host timing probe; run in release mode with --ignored --nocapture"]
fn benchmark_cache_pagination() {
    use std::{hint::black_box, time::Instant};
    const BLOCKS: usize = 320;
    const ROUNDS: usize = 2000;
    let mut store = ReaderStore::new();
    for _ in 0..BLOCKS {
        append_block(&mut store, TextRole::Body);
    }
    for batch in [1, 4, 16, BLOCKS] {
        for incremental in [false, true] {
            let mut samples = [0.0; 5];
            for sample in &mut samples {
                let start = Instant::now();
                for _ in 0..ROUNDS {
                    store.page_count = 0;
                    for count in (batch..=BLOCKS).step_by(batch) {
                        store.block_count = count;
                        if incremental {
                            reader_layout::extend_page_index(black_box(&mut store));
                        } else {
                            reader_layout::rebuild_page_index(black_box(&mut store));
                        }
                        black_box(&store.pages[..store.page_count]);
                    }
                }
                *sample = start.elapsed().as_secs_f64() * 1_000_000.0 / ROUNDS as f64;
            }
            samples.sort_by(f64::total_cmp);
            assert_incremental_index(&store);
            println!(
                "batch={batch} incremental={incremental} median_us_per_section={:.3}",
                samples[2]
            );
        }
    }
}
