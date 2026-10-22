//! Firmware side of the reader page plan: the store-backed fast paths
//! (cached page records, section-relative page mapping) and TOC page
//! targets over the bounded [`ReaderStore`]. Heights, pagination walks,
//! and page-body drawing live in [`ui::reading`] behind the
//! `ReadingBlocks` trait so firmware and host tools render one way.

use crate::reader_store::ReaderStore;
pub(crate) use display::font::{style_marker_code, STYLE_MARKER};
use proto::cache::PageRecord;
pub(crate) use ui::reading::{
    effective_align, line_opening_style, paragraph_indent, reader_layout_config, READER_WRAP_SAFETY,
};
use ui::reading::{page_record_at, paginate_block_pages, PageCursor};

pub(crate) struct ReaderPagePlan {
    page_count: u32,
    page: PageRecord,
}

impl ReaderPagePlan {
    pub(crate) fn new(sd_library: &ReaderStore, requested_page: u32) -> Self {
        let page_count = reader_page_count(sd_library);
        let requested_page = sd_library.settled_page(requested_page);
        // A provisional page lies past the prefix's count by design.
        let requested_page = if sd_library.is_provisional(requested_page) {
            requested_page
        } else {
            requested_page.min(page_count - 1)
        };
        let requested_page = sd_library.local_page_for_global(requested_page);
        let page = reader_page_at(sd_library, requested_page);
        Self { page_count, page }
    }

    pub(crate) fn page_count(&self) -> u32 {
        self.page_count
    }

    pub(crate) fn page(&self) -> PageRecord {
        self.page
    }
}

pub(crate) fn reader_page_count(sd_library: &ReaderStore) -> u32 {
    if sd_library.book_total_pages > 0 {
        return sd_library.book_total_pages;
    }
    if sd_library.page_count > 0 {
        return sd_library.page_count as u32;
    }
    paginate_block_pages(sd_library).max(1) as u32
}

pub(crate) fn reader_page_at(sd_library: &ReaderStore, page_index: usize) -> PageRecord {
    if page_index < sd_library.page_count {
        return sd_library.pages[page_index];
    }
    page_record_at(sd_library, page_index)
}

pub(crate) fn rebuild_page_index(library: &mut ReaderStore) {
    library.page_count = 0;
    extend_page_index(library);
}

/// Update after appending blocks or marking the final block's paragraph end.
/// Completed pages cannot change under these mutations. Revisit the trailing
/// page because its available space (including its final gap) can change.
/// Layout changes, removals, and edits to earlier blocks require a full rebuild.
pub(crate) fn extend_page_index(library: &mut ReaderStore) {
    let first_block = if library.page_count > 0 {
        library.page_count -= 1;
        usize::from(library.pages[library.page_count].first_block)
    } else {
        0
    };
    let mut cursor = PageCursor::from_page_start(first_block);
    while library.page_count < library.pages.len() {
        let Some(page) = cursor.next_page(library) else {
            break;
        };
        let index = library.page_count;
        library.pages[index] = page;
        library.page_spine[index] = library.block_spine[usize::from(page.first_block)];
        library.page_count += 1;
    }
}

/// Write a section ending on a complete page, then retain its unfinished tail.
/// Both XHTML construction and cached-line reflow use the same cut/carry rule.
/// On write failure the original window remains intact for error handling.
pub(crate) fn flush_page_window(
    library: &mut ReaderStore,
    carry_incomplete: bool,
    write: impl FnOnce(&ReaderStore) -> bool,
) -> bool {
    let blocks = library.block_count;
    let text = library.text_len;
    let pages = library.page_count;
    let cut = if carry_incomplete && pages > 1 {
        let first = usize::from(library.pages[pages - 1].first_block);
        (first > 0 && first < blocks).then_some(first)
    } else {
        None
    };
    if let Some(first) = cut {
        library.block_count = first;
        library.page_count = pages - 1;
        library.text_len = library.blocks[first].text_offset as usize;
    }
    let ok = write(library);
    library.block_count = blocks;
    library.text_len = text;
    library.page_count = pages;
    if ok {
        if let Some(first) = cut {
            library.carry_last_page(first);
            rebuild_page_index(library);
        } else {
            library.clear_lines();
        }
    }
    ok
}

pub(crate) fn rebuild_toc_page_targets(library: &mut ReaderStore) {
    for toc_index in 0..library.toc_count {
        let spine_index = library.toc[toc_index].spine_index;
        if spine_index < 0 {
            library.toc_page[toc_index] = 0;
            continue;
        }
        let spine = spine_index as u16;
        let page = library
            .known_page_for_spine(spine)
            .map(|page| page as usize)
            .or_else(|| {
                library
                    .page_spine
                    .iter()
                    .take(library.page_count)
                    .position(|page_spine| *page_spine == spine)
            })
            .unwrap_or_else(|| {
                if library.book_next_spine().is_some() {
                    u16::MAX as usize
                } else {
                    0
                }
            });
        library.toc_page[toc_index] = page.min(u16::MAX as usize) as u16;
    }
}
