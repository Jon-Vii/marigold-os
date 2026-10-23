//! Where an image lives in the book's ZIP, and the hash the image index
//! files it under.
//!
//! An `<img src>` is a URL relative to its chapter. [`resolve_image_path`]
//! turns it into the ZIP entry name it refers to, writing into a buffer the
//! caller lends (the parser's callbacks run at the deepest point of a book
//! build, where nothing may be copied onto the stack), and [`path_hash`]
//! hashes that name. The index hashes central-directory names the same way,
//! so a lookup compares the two without either string being kept.

use crate::epub::decode_html_entity;

/// Why an image source names no entry in the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImagePathError {
    /// Empty, or names a directory rather than a file.
    NoFile,
    /// An absolute URL (`http:`, `data:`, ...): nothing in the book.
    External,
    /// `..` climbs above the root of the book.
    Escapes,
    /// The resolved path does not fit the buffer.
    TooLong,
    /// Percent-escapes decode to bytes that are not UTF-8.
    NotUtf8,
    /// An escaped separator, `%2F` or `%5C`: a `/` inside a name would
    /// slip past the folding of `.` and `..`, and no book names a file so.
    EscapedSeparator,
}

/// Resolves `src`, as written in the chapter at ZIP path `chapter`, to the
/// ZIP entry name it refers to, written into `out`.
///
/// - The `?query` and `#fragment` are dropped.
/// - Entities are decoded (`src` is a raw attribute value) and
///   `%XX` escapes are decoded; a `%` not followed by two hex digits stays
///   as it is. An escaped `/` or `\` is an error.
/// - A relative `src` starts from the chapter's directory, and a leading
///   `/` from the root of the ZIP.
/// - `.` and empty segments drop out, and `..` removes the segment before
///   it. A `..` with nothing left to remove is an error, never clamped:
///   such a path names nothing in the book.
pub fn resolve_image_path<'o>(
    chapter: &str,
    src: &str,
    out: &'o mut [u8],
) -> Result<&'o str, ImagePathError> {
    let src = src.trim_matches(is_html_space);
    if has_scheme(src) {
        return Err(ImagePathError::External);
    }
    let mut path = Folder {
        out,
        len: 0,
        segment: 0,
    };
    let src = match src.strip_prefix('/') {
        Some(rooted) => rooted,
        None => {
            let directory = chapter.rfind('/').map_or("", |end| &chapter[..end]);
            for segment in directory.split('/') {
                path.start_segment()?;
                for &byte in segment.as_bytes() {
                    path.push(byte)?;
                }
                path.end_segment()?;
            }
            src
        }
    };
    path.start_segment()?;
    let bytes = src.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let (unit, advance) = next_unit(src, cursor);
        cursor += advance;
        match unit {
            Unit::Markup(b'#' | b'?') => break,
            Unit::Markup(b'/') => {
                path.end_segment()?;
                path.start_segment()?;
            }
            Unit::Escaped(b'/' | b'\\') => return Err(ImagePathError::EscapedSeparator),
            Unit::Markup(byte) | Unit::Escaped(byte) => path.push(byte)?,
            Unit::Decoded(ch) => {
                let mut utf8 = [0u8; 4];
                for &byte in ch.encode_utf8(&mut utf8).as_bytes() {
                    path.push(byte)?;
                }
            }
        }
    }
    // The last segment must be a name: `dir/`, `.` and `..` are folders.
    if !path.segment_is_name() {
        return Err(ImagePathError::NoFile);
    }
    let Folder { out, len, .. } = path;
    let out: &'o [u8] = out;
    core::str::from_utf8(&out[..len]).map_err(|_| ImagePathError::NotUtf8)
}

/// The stable 64-bit hash an image index files a ZIP path under: FNV-1a
/// over its UTF-8 bytes. Stored on the card, so it must never change.
pub fn path_hash(path: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    path.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

/// Segments built in place in the caller's buffer: `a/b/c`, no leading or
/// trailing `/`. A segment is written first and judged once it ends.
struct Folder<'o> {
    out: &'o mut [u8],
    len: usize,
    /// Where the current segment's bytes start.
    segment: usize,
}

impl Folder<'_> {
    fn push(&mut self, byte: u8) -> Result<(), ImagePathError> {
        let slot = self.out.get_mut(self.len).ok_or(ImagePathError::TooLong)?;
        *slot = byte;
        self.len += 1;
        Ok(())
    }

    fn start_segment(&mut self) -> Result<(), ImagePathError> {
        if self.len > 0 {
            self.push(b'/')?;
        }
        self.segment = self.len;
        Ok(())
    }

    fn current(&self) -> &[u8] {
        &self.out[self.segment..self.len]
    }

    fn segment_is_name(&self) -> bool {
        !matches!(self.current(), b"" | b"." | b"..")
    }

    /// Takes the current segment back out, with the `/` before it.
    fn drop_segment(&mut self) {
        self.len = self.segment.saturating_sub(1);
    }

    fn end_segment(&mut self) -> Result<(), ImagePathError> {
        match self.current() {
            b"" | b"." => self.drop_segment(),
            b".." => {
                if self.segment == 0 {
                    return Err(ImagePathError::Escapes);
                }
                self.drop_segment();
                // And the segment before it.
                self.len = self.out[..self.len]
                    .iter()
                    .rposition(|&byte| byte == b'/')
                    .unwrap_or(0);
            }
            _ => {}
        }
        Ok(())
    }
}

/// One step through a raw `src`.
enum Unit {
    /// A byte as written, which may delimit (`/`, `?`, `#`).
    Markup(u8),
    /// A byte from a `%XX` escape: always part of a name.
    Escaped(u8),
    /// A character an entity stood for.
    Decoded(char),
}

/// The unit of `src` at `cursor`, and how many bytes it takes.
fn next_unit(src: &str, cursor: usize) -> (Unit, usize) {
    let bytes = src.as_bytes();
    let byte = bytes[cursor];
    if byte == b'%' {
        let high = bytes.get(cursor + 1).and_then(|&digit| hex(digit));
        let low = bytes.get(cursor + 2).and_then(|&digit| hex(digit));
        if let (Some(high), Some(low)) = (high, low) {
            return (Unit::Escaped(high << 4 | low), 3);
        }
    }
    if byte == b'&' {
        if let Some(decoded) = decode_html_entity(&src[cursor..]) {
            let advance = src[cursor..].find(';').map_or(1, |end| end + 1);
            // An entity for a delimiter still delimits, as it would after
            // the XML parser a full reader runs first.
            return match u8::try_from(decoded) {
                Ok(ascii @ (b'/' | b'#' | b'?')) => (Unit::Markup(ascii), advance),
                _ => (Unit::Decoded(decoded), advance),
            };
        }
    }
    (Unit::Markup(byte), 1)
}

fn hex(digit: u8) -> Option<u8> {
    (digit as char).to_digit(16).map(|value| value as u8)
}

/// Whether `src` starts with a URL scheme (`letter *(letter / digit / + -
/// .) :`) before any `/`, `?` or `#`.
fn has_scheme(src: &str) -> bool {
    let Some(colon) = src.find(':') else {
        return false;
    };
    let scheme = &src[..colon];
    scheme.starts_with(|ch: char| ch.is_ascii_alphabetic())
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn is_html_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{0C}')
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::string::{String, ToString};

    fn resolve(chapter: &str, src: &str) -> Result<String, ImagePathError> {
        let mut out = [0u8; 64];
        resolve_image_path(chapter, src, &mut out).map(ToString::to_string)
    }

    fn s(text: &str) -> String {
        text.into()
    }

    #[test]
    fn relative_sources_start_from_the_chapter_directory() {
        let chapter = "OEBPS/text/ch01.xhtml";
        assert_eq!(resolve(chapter, "fig.png"), Ok(s("OEBPS/text/fig.png")));
        assert_eq!(
            resolve(chapter, "./img/fig.png"),
            Ok(s("OEBPS/text/img/fig.png"))
        );
        assert_eq!(
            resolve(chapter, "../images/fig.png"),
            Ok(s("OEBPS/images/fig.png"))
        );
        assert_eq!(resolve(chapter, "../../cover.png"), Ok(s("cover.png")));
        assert_eq!(
            resolve(chapter, "a/./b/../../c//d.png"),
            Ok(s("OEBPS/text/c/d.png"))
        );
        // A chapter at the root of the ZIP.
        assert_eq!(resolve("ch.xhtml", "images/a.png"), Ok(s("images/a.png")));
        // A messy chapter path folds too.
        assert_eq!(
            resolve("OEBPS/./x/../text/ch.xhtml", "a.png"),
            Ok(s("OEBPS/text/a.png"))
        );
    }

    #[test]
    fn a_leading_slash_starts_from_the_root() {
        assert_eq!(
            resolve("OEBPS/text/ch.xhtml", "/OEBPS/images/a.png"),
            Ok(s("OEBPS/images/a.png"))
        );
        assert_eq!(resolve("OEBPS/text/ch.xhtml", "//a.png"), Ok(s("a.png")));
    }

    #[test]
    fn query_and_fragment_are_dropped() {
        let chapter = "OEBPS/ch.xhtml";
        assert_eq!(resolve(chapter, "a.png#frag"), Ok(s("OEBPS/a.png")));
        assert_eq!(resolve(chapter, "a.png?v=2#frag"), Ok(s("OEBPS/a.png")));
        assert_eq!(resolve(chapter, "a.png#x?y"), Ok(s("OEBPS/a.png")));
        assert_eq!(resolve(chapter, " a.png \n"), Ok(s("OEBPS/a.png")));
    }

    #[test]
    fn escapes_and_entities_decode() {
        let chapter = "OEBPS/ch.xhtml";
        assert_eq!(resolve(chapter, "my%20fig.png"), Ok(s("OEBPS/my fig.png")));
        assert_eq!(resolve(chapter, "caf%C3%A9.png"), Ok(s("OEBPS/café.png")));
        assert_eq!(resolve(chapter, "café.png"), Ok(s("OEBPS/café.png")));
        assert_eq!(resolve(chapter, "Q&amp;A.png"), Ok(s("OEBPS/Q&A.png")));
        // An escaped `#` is part of the name, an entity one is a fragment.
        assert_eq!(resolve(chapter, "no%23.png"), Ok(s("OEBPS/no#.png")));
        assert_eq!(resolve(chapter, "a.png&#35;x"), Ok(s("OEBPS/a.png")));
        // A stray percent stays.
        assert_eq!(resolve(chapter, "100%.png"), Ok(s("OEBPS/100%.png")));
        assert_eq!(resolve(chapter, "%zz.png"), Ok(s("OEBPS/%zz.png")));
        // Escapes that do not form UTF-8 name nothing.
        assert_eq!(resolve(chapter, "%C3.png"), Err(ImagePathError::NotUtf8));
    }

    #[test]
    fn escaped_separators_are_refused() {
        let chapter = "OEBPS/text/ch.xhtml";
        for src in [
            "..%2Fx.png",
            "..%2fx.png",
            "a%2F..%2F..%2F..%2Fx.png",
            "images%2Fa.png",
            "..%5Cx.png",
            "images%5ca.png",
            "/%2F..%2Fx.png",
        ] {
            assert_eq!(
                resolve(chapter, src),
                Err(ImagePathError::EscapedSeparator),
                "{src:?}"
            );
        }
        assert_eq!(
            resolve("ch.xhtml", "..%2Fx.png"),
            Err(ImagePathError::EscapedSeparator)
        );
        // Escaped dots still fold, and never past the root.
        assert_eq!(resolve(chapter, "%2E%2E/a.png"), Ok(s("OEBPS/a.png")));
        assert_eq!(
            resolve("ch.xhtml", "%2e%2e/a.png"),
            Err(ImagePathError::Escapes)
        );
        // A separator after the fragment never counts.
        assert_eq!(resolve(chapter, "a.png#%2F"), Ok(s("OEBPS/text/a.png")));
    }

    #[test]
    fn climbing_out_of_the_book_is_an_error() {
        assert_eq!(
            resolve("ch.xhtml", "../a.png"),
            Err(ImagePathError::Escapes)
        );
        assert_eq!(
            resolve("OEBPS/ch.xhtml", "../../a.png"),
            Err(ImagePathError::Escapes)
        );
        assert_eq!(
            resolve("OEBPS/ch.xhtml", "/../a.png"),
            Err(ImagePathError::Escapes)
        );
        assert_eq!(
            resolve("OEBPS/ch.xhtml", "a/../../../b.png"),
            Err(ImagePathError::Escapes)
        );
        assert_eq!(
            resolve("../ch.xhtml", "a.png"),
            Err(ImagePathError::Escapes)
        );
    }

    #[test]
    fn sources_that_name_no_file_are_refused() {
        let chapter = "OEBPS/ch.xhtml";
        for src in ["", "   ", "#top", "?q", "dir/", ".", "..", "a/.."] {
            assert_eq!(
                resolve(chapter, src),
                Err(ImagePathError::NoFile),
                "{src:?}"
            );
        }
        for src in [
            "http://example.com/a.png",
            "data:image/png;base64,AAAA",
            "HTTPS:x",
        ] {
            assert_eq!(
                resolve(chapter, src),
                Err(ImagePathError::External),
                "{src:?}"
            );
        }
        // A colon after a slash is part of a name, not a scheme.
        assert_eq!(resolve(chapter, "a/b:c.png"), Ok(s("OEBPS/a/b:c.png")));
    }

    #[test]
    fn a_short_buffer_is_an_error() {
        let mut out = [0u8; 8];
        assert_eq!(
            resolve_image_path("OEBPS/ch.xhtml", "a.png", &mut out),
            Err(ImagePathError::TooLong)
        );
        let mut out = [0u8; 11];
        assert_eq!(
            resolve_image_path("OEBPS/ch.xhtml", "a.png", &mut out),
            Ok("OEBPS/a.png")
        );
    }

    #[test]
    fn the_path_hash_is_fnv1a_64() {
        // Published FNV-1a test vectors: these values are on cards.
        assert_eq!(path_hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(path_hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(path_hash("foobar"), 0x8594_4171_f739_67e8);
        assert_ne!(path_hash("OEBPS/a.png"), path_hash("OEBPS/b.png"));
    }
}
