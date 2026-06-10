//! Firmware side of the reader page plan: page records, pagination, and
//! TOC page targets over the bounded [`ReaderStore`]. The shared layout
//! constants, measurement, wrapping, and styled-line drawing live in
//! [`ui::reading`] so firmware, cache building, and host preview tooling
//! cannot drift apart.

use crate::reader_store::ReaderStore;
use display::font::{literata, BitmapFont, FontStyle};
pub(crate) use display::font::{style_marker_code, STYLE_MARKER};
use proto::cache::PageRecord;
pub(crate) use ui::reading::{
    draw_centered_wrapped_literata, draw_justified_wrapped_literata, draw_styled_line,
    draw_wrapped_literata, first_styled_line_style, line_advance_for, reader_x_for,
    styled_text_ink_width, wrapped_block_height, READER_LAYOUT_CONFIG, READER_LEFT_X,
    READER_PAGE_BOTTOM, READER_PAGE_TOP, READER_RIGHT_X, READER_WRAP_SAFETY,
};

pub(crate) struct ReaderPagePlan {
    page_count: u32,
    page: PageRecord,
}

pub(crate) struct ReaderDrawableBlock<'a> {
    pub(crate) record: proto::cache::BlockRecord,
    pub(crate) text: &'a str,
    pub(crate) y: i16,
    pub(crate) advance: i16,
    pub(crate) style: FontStyle,
    pub(crate) font: &'static BitmapFont,
}

impl ReaderPagePlan {
    pub(crate) fn new(sd_library: &ReaderStore, requested_page: u32) -> Self {
        let page_count = reader_page_count(sd_library, READER_PAGE_TOP, READER_PAGE_BOTTOM);
        let requested_page = sd_library.local_page_for_global(requested_page.min(page_count - 1));
        let page = reader_page_at(
            sd_library,
            requested_page,
            READER_PAGE_TOP,
            READER_PAGE_BOTTOM,
        );
        Self { page_count, page }
    }

    pub(crate) fn page_count(&self) -> u32 {
        self.page_count
    }

    pub(crate) fn for_each_block(
        &self,
        sd_library: &ReaderStore,
        mut visit: impl FnMut(ReaderDrawableBlock<'_>) -> bool,
    ) {
        let mut y = READER_PAGE_TOP;
        for offset in 0..self.page.block_count as usize {
            let index = self.page.first_block as usize + offset;
            let Some(record) = sd_library.block_record(index) else {
                break;
            };
            let text = sd_library.block_text(index);
            let advance = line_advance_for(record.role);
            let style = sd_library.block_style(index);
            let block_height = sd_block_height(sd_library, index);
            if y + block_height > READER_PAGE_BOTTOM && y > READER_PAGE_TOP {
                break;
            }
            if !visit(ReaderDrawableBlock {
                record,
                text,
                y: y + advance,
                advance,
                style,
                font: literata(style),
            }) {
                break;
            }
            y += block_height;
        }
    }
}

pub(crate) fn reader_page_count(sd_library: &ReaderStore, page_top: i16, page_bottom: i16) -> u32 {
    if sd_library.book_total_pages > 0 {
        return sd_library.book_total_pages;
    }
    if sd_library.page_count > 0 {
        return sd_library.page_count as u32;
    }
    paginate_sd_reader(sd_library, page_top, page_bottom).max(1) as u32
}

pub(crate) fn reader_page_at(
    sd_library: &ReaderStore,
    page_index: usize,
    page_top: i16,
    page_bottom: i16,
) -> PageRecord {
    if page_index < sd_library.page_count {
        return sd_library.pages[page_index];
    }
    let mut current = 0usize;
    let mut first_block = 0usize;
    let mut block_count = 0usize;
    let mut y = page_top;

    for index in 0..sd_library.block_count {
        let block_height = sd_block_height(sd_library, index);
        let new_page = (y + block_height > page_bottom
            || sd_library.block_page_break_before[index])
            && y > page_top;
        if new_page {
            if current == page_index {
                return PageRecord {
                    first_block: first_block as u16,
                    block_count: block_count as u16,
                };
            }
            current += 1;
            first_block = index;
            block_count = 0;
            y = page_top;
        }
        block_count += 1;
        y += block_height;
    }

    PageRecord {
        first_block: first_block as u16,
        block_count: block_count as u16,
    }
}

pub(crate) fn rebuild_page_index(library: &mut ReaderStore, page_top: i16, page_bottom: i16) {
    library.page_count = 0;
    if library.block_count == 0 {
        return;
    }

    let mut first_block = 0usize;
    let mut block_count = 0usize;
    let mut y = page_top;

    for index in 0..library.block_count {
        let block_height = sd_block_height(library, index);
        let new_page = (y + block_height > page_bottom || library.block_page_break_before[index])
            && y > page_top;
        if new_page {
            push_sd_page_record(library, first_block, block_count);
            first_block = index;
            block_count = 0;
            y = page_top;
        }
        block_count += 1;
        y += block_height;
    }

    push_sd_page_record(library, first_block, block_count);
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

fn push_sd_page_record(library: &mut ReaderStore, first_block: usize, block_count: usize) {
    if block_count == 0 || library.page_count >= library.pages.len() {
        return;
    }
    let page_index = library.page_count;
    library.pages[library.page_count] = PageRecord {
        first_block: first_block as u16,
        block_count: block_count as u16,
    };
    library.page_spine[page_index] = library.block_spine.get(first_block).copied().unwrap_or(0);
    library.page_count += 1;
}

fn paginate_sd_reader(sd_library: &ReaderStore, page_top: i16, page_bottom: i16) -> usize {
    let mut pages = 1u32;
    let mut y = page_top;

    for index in 0..sd_library.block_count {
        if sd_library.block_page_break_before[index] && y > page_top {
            pages = pages.saturating_add(1);
            y = page_top;
        }
        let block_height = sd_block_height(sd_library, index);

        if y + block_height > page_bottom && y > page_top {
            pages = pages.saturating_add(1);
            y = page_top;
        }
        y += block_height;
    }

    pages.max(1) as usize
}

fn sd_block_height(sd_library: &ReaderStore, index: usize) -> i16 {
    let Some(record) = sd_library.blocks.get(index) else {
        return 0;
    };
    let advance = line_advance_for(record.role);
    let height = if record.line_count == 1 {
        advance
    } else {
        wrapped_block_height(
            literata(sd_library.block_styles[index]),
            sd_library.block_text(index),
            record.role,
            record.align,
            advance,
        )
    };
    height + paragraph_gap_after(sd_library, index)
}

pub(crate) fn paragraph_gap_after(sd_library: &ReaderStore, index: usize) -> i16 {
    if sd_library
        .block_paragraph_end
        .get(index)
        .copied()
        .unwrap_or(true)
    {
        ui::reading::paragraph_gap(sd_library.blocks[index].role)
    } else {
        0
    }
}
