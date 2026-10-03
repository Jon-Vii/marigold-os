use proto::book::{BookId, BookMeta, BookSource, ChapterMeta, CoverStatus};

pub const ACTIVE_BOOK_ID: BookId = BookId(1);

pub const BOOKS: [BookMeta<'static>; 1] = [BookMeta {
    id: ACTIVE_BOOK_ID,
    title: "About This Reader",
    author: "",
    source_path: "/built-in/guide",
    byte_size: 0,
    source: BookSource::BuiltIn,
    cover_status: CoverStatus::Missing,
}];

pub const CHAPTERS: [ChapterMeta<'static>; 3] = [
    ChapterMeta {
        title: "Reading",
        spine_index: 0,
        source_href: "guide/reading.xhtml",
    },
    ChapterMeta {
        title: "Adding Books",
        spine_index: 1,
        source_href: "guide/adding-books.xhtml",
    },
    ChapterMeta {
        title: "Wi-Fi Setup",
        spine_index: 2,
        source_href: "guide/wifi-setup.xhtml",
    },
];

pub fn active_book(book_id: u32) -> BookMeta<'static> {
    BOOKS
        .iter()
        .copied()
        .find(|book| book.id.0 == book_id)
        .unwrap_or(BOOKS[0])
}

pub fn book_at(index: usize) -> Option<BookMeta<'static>> {
    BOOKS.get(index).copied()
}

pub const fn book_count() -> u8 {
    BOOKS.len() as u8
}

pub fn chapter_at(index: usize) -> Option<ChapterMeta<'static>> {
    CHAPTERS.get(index).copied()
}

pub const fn chapter_count() -> u8 {
    CHAPTERS.len() as u8
}
