use crate::book::{BookId, BookMeta, BookSource, CoverStatus};
use heapless::String;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanRoot {
    BooksDir,
    CardRoot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileCandidate<'a> {
    pub root: ScanRoot,
    pub path: &'a str,
    pub byte_size: u32,
}

impl<'a> FileCandidate<'a> {
    pub fn as_book(self, id: BookId) -> Option<BookMeta<'a>> {
        if !is_epub_path(self.path) {
            return None;
        }
        let file_name = self.path.rsplit('/').next().unwrap_or(self.path);
        let title = file_name.strip_suffix(".epub").unwrap_or(file_name);
        Some(BookMeta {
            id,
            title,
            author: "Unknown Author",
            source_path: self.path,
            byte_size: self.byte_size,
            source: BookSource::MicroSd,
            cover_status: CoverStatus::Unknown,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    NoCard,
    UnsupportedFilesystem,
    Io,
    TooManyBooks,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderProgressRecord<'a> {
    pub book_path: &'a str,
    pub book_id: BookId,
    pub spine_index: u16,
    pub screen_index: u32,
    pub text_run_index: u16,
    pub text_byte_offset: u16,
    pub reading_orientation: u8,
    pub refresh_policy: u8,
}

pub trait BookStorage {
    fn scan_epubs(
        &mut self,
        on_candidate: impl FnMut(FileCandidate<'_>) -> Result<(), StorageError>,
    ) -> Result<(), StorageError>;

    fn read_at(&mut self, path: &str, offset: u32, out: &mut [u8]) -> Result<usize, StorageError>;
}

pub trait ProgressStorage {
    fn load_progress<'a>(
        &mut self,
        scratch: &'a mut [u8],
    ) -> Result<Option<ReaderProgressRecord<'a>>, StorageError>;

    fn store_progress(&mut self, record: ReaderProgressRecord<'_>) -> Result<(), StorageError>;
}

pub fn is_epub_path(path: &str) -> bool {
    // Uploads are written with 8.3 names, where the extension truncates
    // to ".epu"; accept both spellings everywhere EPUBs are discovered.
    if path.len() >= 4 {
        let tail = &path.as_bytes()[path.len() - 4..];
        if tail[0] == b'.'
            && tail[1].eq_ignore_ascii_case(&b'e')
            && tail[2].eq_ignore_ascii_case(&b'p')
            && tail[3].eq_ignore_ascii_case(&b'u')
        {
            return true;
        }
    }
    path.as_bytes()
        .windows(5)
        .last()
        .map(|suffix| suffix.eq_ignore_ascii_case(b".epub"))
        .unwrap_or(false)
}

/// Store the catalog's display path in its fixed-size field. The FAT short
/// name remains the open handle; this only provides the user-facing label and
/// a stable cache identity.
pub fn catalog_display_path<const N: usize>(prefix: &str, name: &str, out: &mut String<N>) {
    out.clear();
    push_utf8_prefix(prefix, N, out);

    // Keep the EPUB suffix when a long FAT name needs trimming. The Library's
    // fallback label uses it to remove the extension, while the beginning of
    // the filename remains the most useful part for the reader.
    let suffix = if name.as_bytes().len() >= 5
        && name.as_bytes()[name.len() - 5..].eq_ignore_ascii_case(b".epub")
    {
        &name[name.len() - 5..]
    } else if name.as_bytes().len() >= 4
        && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".epu")
    {
        &name[name.len() - 4..]
    } else {
        ""
    };
    let stem = &name[..name.len() - suffix.len()];
    let stem_capacity = N.saturating_sub(out.len() + suffix.len());
    push_utf8_prefix(stem, out.len() + stem_capacity, out);
    let _ = out.push_str(suffix);
}

fn push_utf8_prefix<const N: usize>(text: &str, end: usize, out: &mut String<N>) {
    for ch in text.chars() {
        if out.len() + ch.len_utf8() > end {
            break;
        }
        let _ = out.push(ch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_epub_suffix_case_insensitively() {
        assert!(is_epub_path("/books/Alice.EPUB"));
        assert!(is_epub_path("book.epub"));
        assert!(!is_epub_path("book.epub.tmp"));
    }

    #[test]
    fn file_candidate_becomes_minimal_book_meta() {
        let candidate = FileCandidate {
            root: ScanRoot::BooksDir,
            path: "/books/algernon.epub",
            byte_size: 42,
        };

        let book = candidate.as_book(BookId(3)).expect("epub candidate");

        assert_eq!(book.title, "algernon");
        assert_eq!(book.source, BookSource::MicroSd);
        assert_eq!(book.byte_size, 42);
    }

    #[test]
    fn long_epub_names_do_not_collapse_to_the_root_path() {
        for name in [
            "L'Istituto per la Regolazione degli Orologi - Ahmet Hamdi Tanpinar_748.epub",
            "The Weird_ A Compendium of Stra - Jeff Vandermeer; Ann Vandermeer.epub",
        ] {
            let mut path = String::<64>::new();
            catalog_display_path("/", name, &mut path);

            assert_ne!(path.as_str(), "/");
            assert!(path.ends_with(".epub"));
            assert!(path.len() <= 64);
        }
    }
}
