//! Fake shelf for the web emulator: a few small public-domain books parsed
//! from a light line markup into the same block records the firmware caches,
//! so `ui::reading` paginates and draws them exactly as it would on the card.
//! The first entry is the default Continue book a fresh visit opens to.

use display::font::{FontStyle, TypeSettings};
use proto::cache::{BlockRecord, PageRecord};
use proto::text::{TextAlign, TextRole};
use ui::reading::{
    block_first_line_indent, block_height, block_ink_height, body_font, wrapped_line_count,
    ReadingBlocks, READER_LEFT_X, READER_PAGE_BOTTOM, READER_PAGE_TOP, READER_RIGHT_X,
    reader_x_for,
};

pub struct BookSource {
    pub title: &'static str,
    pub author: &'static str,
    pub text: &'static str,
}

pub const SHELF: [BookSource; 3] = [
    BookSource {
        title: "Alice's Adventures in Wonderland",
        author: "Lewis Carroll",
        text: include_str!("../books/alice.txt"),
    },
    BookSource {
        title: "A Christmas Carol",
        author: "Charles Dickens",
        text: include_str!("../books/carol.txt"),
    },
    BookSource {
        title: "Aesop's Fables",
        author: "Townsend translation",
        text: include_str!("../books/aesop.txt"),
    },
];

struct Block {
    record: BlockRecord,
    text: String,
    style: FontStyle,
    page_break_before: bool,
    paragraph_end: bool,
}

pub struct Chapter {
    pub title: String,
    /// First page of the chapter under the current pagination.
    pub start_page: u16,
}

/// A whole fake book, resident and paginated under one set of type
/// settings. Rebuilt when the reader's type settings change, mirroring the
/// firmware's cache-invalidate-on-layout-config path.
pub struct BookStore {
    blocks: Vec<Block>,
    settings: TypeSettings,
    pub pages: Vec<PageRecord>,
    pub chapters: Vec<Chapter>,
}

impl ReadingBlocks for BookStore {
    fn block_count(&self) -> usize {
        self.blocks.len()
    }

    fn block(&self, index: usize) -> Option<BlockRecord> {
        self.blocks.get(index).map(|block| block.record)
    }

    fn block_text(&self, index: usize) -> &str {
        self.blocks
            .get(index)
            .map(|block| block.text.as_str())
            .unwrap_or("")
    }

    fn block_style(&self, index: usize) -> FontStyle {
        self.blocks
            .get(index)
            .map(|block| block.style)
            .unwrap_or(FontStyle::Regular)
    }

    fn page_break_before(&self, index: usize) -> bool {
        self.blocks
            .get(index)
            .map(|block| block.page_break_before)
            .unwrap_or(false)
    }

    fn paragraph_end(&self, index: usize) -> bool {
        self.blocks
            .get(index)
            .map(|block| block.paragraph_end)
            .unwrap_or(true)
    }

    fn type_settings(&self) -> TypeSettings {
        self.settings
    }
}

impl BookStore {
    pub fn build(source: &BookSource, settings: TypeSettings) -> Self {
        let mut store = Self {
            blocks: Vec::new(),
            settings,
            pages: Vec::new(),
            chapters: Vec::new(),
        };
        let mut chapter_blocks: Vec<usize> = Vec::new();
        parse(source.text, &mut store, &mut chapter_blocks);
        store.finish_line_counts();
        store.paginate(&chapter_blocks);
        store
    }

    pub fn page_count(&self) -> u32 {
        self.pages.len().max(1) as u32
    }

    pub fn page(&self, index: u32) -> PageRecord {
        let clamped = (index as usize).min(self.pages.len().saturating_sub(1));
        self.pages
            .get(clamped)
            .copied()
            .unwrap_or(PageRecord {
                first_block: 0,
                block_count: 0,
            })
    }

    pub fn chapter_for_page(&self, page: u32) -> u16 {
        let mut current = 0u16;
        for (index, chapter) in self.chapters.iter().enumerate() {
            if u32::from(chapter.start_page) <= page {
                current = index as u16;
            } else {
                break;
            }
        }
        current
    }

    /// Page-within-chapter position for the reader footer: (current, total).
    pub fn chapter_page_position(&self, page: u32) -> (u32, u32) {
        let chapter = self.chapter_for_page(page) as usize;
        let start = u32::from(self.chapters.get(chapter).map(|c| c.start_page).unwrap_or(0));
        let end = self
            .chapters
            .get(chapter + 1)
            .map(|c| u32::from(c.start_page))
            .unwrap_or_else(|| self.page_count());
        let total = end.saturating_sub(start).max(1);
        (page.saturating_sub(start) + 1, total)
    }

    /// Compute the wrap-dependent line counts the parser could not know yet.
    fn finish_line_counts(&mut self) {
        for index in 0..self.blocks.len() {
            let record = self.blocks[index].record;
            let indent = block_first_line_indent(self, index);
            let font = body_font(self.settings, self.blocks[index].style);
            let max_width = READER_RIGHT_X
                - if record.align == TextAlign::Center {
                    READER_LEFT_X
                } else {
                    reader_x_for(record.role)
                };
            let lines =
                wrapped_line_count(font, &self.blocks[index].text, max_width, indent).max(1);
            self.blocks[index].record.line_count = lines.min(u8::MAX as u16) as u8;
        }
    }

    /// The firmware's page walk (`reader_layout::rebuild_page_index`):
    /// ink-height fit against the page bottom, forced breaks honored, and
    /// the chapter list resolved to the page its heading lands on.
    fn paginate(&mut self, chapter_blocks: &[usize]) {
        self.pages.clear();
        if self.blocks.is_empty() {
            return;
        }
        let mut first_block = 0usize;
        let mut block_count = 0usize;
        let mut y = READER_PAGE_TOP;
        let mut chapter_cursor = 0usize;

        for index in 0..self.blocks.len() {
            let height = block_height(self, index);
            let new_page = (y + block_ink_height(self, index) > READER_PAGE_BOTTOM
                || self.blocks[index].page_break_before)
                && y > READER_PAGE_TOP;
            if new_page {
                self.pages.push(PageRecord {
                    first_block: first_block as u16,
                    block_count: block_count as u16,
                });
                first_block = index;
                block_count = 0;
                y = READER_PAGE_TOP;
            }
            if chapter_cursor < chapter_blocks.len() && chapter_blocks[chapter_cursor] == index {
                self.chapters[chapter_cursor].start_page = self.pages.len() as u16;
                chapter_cursor += 1;
            }
            block_count += 1;
            y += height;
        }
        self.pages.push(PageRecord {
            first_block: first_block as u16,
            block_count: block_count as u16,
        });
    }
}

fn record(role: TextRole, align: TextAlign) -> BlockRecord {
    BlockRecord {
        text_offset: 0,
        text_len: 0,
        line_count: 1,
        role,
        style: proto::text::FontStyle::Regular,
        align,
    }
}

/// Line markup: `# ` chapter heading, `> ` italic quote, `~ ` centered
/// verse line, `***` separator, blank-line-separated justified paragraphs.
fn parse(text: &str, store: &mut BookStore, chapter_blocks: &mut Vec<usize>) {
    let mut paragraph = String::new();
    let mut verse_run: Vec<String> = Vec::new();

    let flush_paragraph = |store: &mut BookStore, paragraph: &mut String| {
        if paragraph.is_empty() {
            return;
        }
        store.blocks.push(Block {
            record: record(TextRole::Body, TextAlign::Justify),
            text: core::mem::take(paragraph),
            style: FontStyle::Regular,
            page_break_before: false,
            paragraph_end: true,
        });
    };
    let flush_verse = |store: &mut BookStore, verse_run: &mut Vec<String>| {
        let count = verse_run.len();
        for (index, line) in verse_run.drain(..).enumerate() {
            store.blocks.push(Block {
                record: record(TextRole::Body, TextAlign::Center),
                text: line,
                style: FontStyle::Regular,
                page_break_before: false,
                paragraph_end: index + 1 == count,
            });
        }
        if count > 0 {
            // Stanza gap: Body carries no paragraph gap, so a blank
            // centered line stands in for the stanza break.
            store.blocks.push(Block {
                record: record(TextRole::Body, TextAlign::Center),
                text: " ".into(),
                style: FontStyle::Regular,
                page_break_before: false,
                paragraph_end: true,
            });
        }
    };

    for line in text.lines() {
        let line = line.trim_end();
        if let Some(title) = line.strip_prefix("# ") {
            flush_paragraph(store, &mut paragraph);
            flush_verse(store, &mut verse_run);
            chapter_blocks.push(store.blocks.len());
            store.chapters.push(Chapter {
                title: title.to_string(),
                start_page: 0,
            });
            store.blocks.push(Block {
                record: record(TextRole::Heading1, TextAlign::Center),
                text: title.to_string(),
                style: FontStyle::Bold,
                page_break_before: !store.blocks.is_empty(),
                paragraph_end: true,
            });
        } else if let Some(quote) = line.strip_prefix("> ") {
            flush_paragraph(store, &mut paragraph);
            flush_verse(store, &mut verse_run);
            store.blocks.push(Block {
                record: record(TextRole::BlockQuote, TextAlign::Left),
                text: quote.to_string(),
                style: FontStyle::Italic,
                page_break_before: false,
                paragraph_end: true,
            });
        } else if let Some(verse) = line.strip_prefix("~ ") {
            flush_paragraph(store, &mut paragraph);
            verse_run.push(verse.to_string());
        } else if line == "***" {
            flush_paragraph(store, &mut paragraph);
            flush_verse(store, &mut verse_run);
            store.blocks.push(Block {
                record: record(TextRole::Body, TextAlign::Center),
                text: "* * *".into(),
                style: FontStyle::Regular,
                page_break_before: false,
                paragraph_end: true,
            });
        } else if line.is_empty() {
            flush_paragraph(store, &mut paragraph);
            flush_verse(store, &mut verse_run);
        } else {
            if !paragraph.is_empty() {
                paragraph.push(' ');
            }
            paragraph.push_str(line);
        }
    }
    flush_paragraph(store, &mut paragraph);
    flush_verse(store, &mut verse_run);
}
