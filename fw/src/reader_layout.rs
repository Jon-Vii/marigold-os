//! Firmware side of the reader page plan: the store-backed fast paths
//! (cached page records, section-relative page mapping) and TOC page
//! targets over the bounded [`ReaderStore`]. Heights, pagination walks,
//! and page-body drawing live in [`ui::reading`] behind the
//! `ReadingBlocks` trait so firmware and host tools render one way.

use crate::reader_store::ReaderStore;
pub(crate) use display::font::{style_marker_code, STYLE_MARKER};
use proto::cache::PageRecord;
pub(crate) use ui::reading::{
    first_styled_line_style, paragraph_indent, reader_layout_config, READER_WRAP_SAFETY,
};
use ui::reading::{page_record_at, paginate_block_pages, PageCursor};

pub(crate) struct ReaderPagePlan {
    page_count: u32,
    page: PageRecord,
}

impl ReaderPagePlan {
    pub(crate) fn new(sd_library: &ReaderStore, requested_page: u32) -> Self {
        let page_count = reader_page_count(sd_library);
        let requested_page = sd_library.local_page_for_global(requested_page.min(page_count - 1));
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

pub(crate) fn rebuild_toc_page_targets(library: &mut ReaderStore) {
    for toc_index in 0..library.toc_count {
        let spine_index = library.toc[toc_index].spine_index;
        if spine_index < 0 {
            library.toc_page[toc_index] = 0;
            continue;
        }
        let spine = spine_index as u16;
        let page = library
            .book_sections
            .iter()
            .take(library.book_section_count)
            .find(|section| section.spine == spine)
            .map(|section| section.start_page as usize)
            .or_else(|| {
                library
                    .page_spine
                    .iter()
                    .take(library.page_count)
                    .position(|page_spine| *page_spine == spine)
            })
            .unwrap_or(0);
        library.toc_page[toc_index] = page.min(u16::MAX as usize) as u16;
    }
}
