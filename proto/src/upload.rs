//! Allocation-free naming helpers for browser-to-SD book uploads.

use core::fmt::Write;
use heapless::String;

pub const UPLOAD_FILENAME_BYTES: usize = 64;
pub type UploadFilename = String<UPLOAD_FILENAME_BYTES>;
pub type UploadShortName = String<12>;

const EPUB_SUFFIX: &str = ".epub";
const MAX_DECODED_BASENAME_BYTES: usize = 256;
const FNV_OFFSET: u32 = 0x811C_9DC5;
const FNV_PRIME: u32 = 0x0100_0193;

/// Turn the percent-encoded browser filename into a portable VFAT long name.
///
/// Path components and the supplied extension are discarded, FAT-invalid
/// characters are replaced, and the result always ends in lowercase `.epub`.
pub fn wireless_epub_filename(client_name: &[u8]) -> UploadFilename {
    let mut basename = [0u8; MAX_DECODED_BASENAME_BYTES];
    let mut basename_len = 0;
    let mut at = 0;
    while at < client_name.len() {
        let byte = if client_name[at] == b'%' && at + 2 < client_name.len() {
            match (
                hex_nibble(client_name[at + 1]),
                hex_nibble(client_name[at + 2]),
            ) {
                (Some(high), Some(low)) => {
                    at += 2;
                    (high << 4) | low
                }
                _ => client_name[at],
            }
        } else {
            client_name[at]
        };
        at += 1;

        if byte == b'/' || byte == b'\\' {
            basename_len = 0;
        } else if basename_len < basename.len() {
            basename[basename_len] = byte;
            basename_len += 1;
        }
    }

    let decoded = match core::str::from_utf8(&basename[..basename_len]) {
        Ok(text) => text,
        Err(error) => core::str::from_utf8(&basename[..error.valid_up_to()]).unwrap_or(""),
    };
    let decoded = decoded.trim_matches(|ch| ch == ' ' || ch == '.');
    let stem = decoded
        .rfind('.')
        .map(|extension_at| &decoded[..extension_at])
        .unwrap_or(decoded)
        .trim_matches(|ch| ch == ' ' || ch == '.');

    let mut out = UploadFilename::new();
    for ch in stem.chars() {
        let ch = if ch.is_control()
            || matches!(ch, '"' | '*' | '/' | ':' | '<' | '>' | '?' | '\\' | '|')
        {
            '_'
        } else {
            ch
        };
        if out.len() + ch.len_utf8() + EPUB_SUFFIX.len() > out.capacity() {
            break;
        }
        let _ = out.push(ch);
    }
    while out.ends_with([' ', '.']) {
        out.pop();
    }
    if out.is_empty() {
        let _ = out.push_str("Book");
    }
    let _ = out.push_str(EPUB_SUFFIX);
    out
}

/// Build a deterministic, legal 8.3 alias for a long upload filename.
///
/// `probe` is incremented only if that alias is already occupied by another
/// directory entry. The `.EPU` extension keeps the file readable by older
/// MarigoldOS releases if its long-name records are ever damaged.
pub fn upload_short_alias(long_name: &str, probe: u16) -> UploadShortName {
    let mut hash = FNV_OFFSET;
    for byte in long_name
        .as_bytes()
        .iter()
        .copied()
        .chain(probe.to_le_bytes())
    {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    let mut out = UploadShortName::new();
    write!(out, "{hash:08X}.EPU").expect("8.3 alias always fits");
    out
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_readable_epub_long_name() {
        assert_eq!(
            wireless_epub_filename(b"The%20Left%20Hand%20of%20Darkness.epub"),
            "The Left Hand of Darkness.epub"
        );
        assert_eq!(
            wireless_epub_filename("Märchen 😀.EPUB".as_bytes()),
            "Märchen 😀.epub"
        );
    }

    #[test]
    fn removes_paths_and_sanitizes_fat_characters() {
        assert_eq!(
            wireless_epub_filename(b"..%2Funsafe%3Abook%3F.epub"),
            "unsafe_book_.epub"
        );
        assert_eq!(
            wireless_epub_filename(b"C%3A%5Cfakepath%5CNovel.zip"),
            "Novel.epub"
        );
        assert_eq!(wireless_epub_filename(b"..."), "Book.epub");
    }

    #[test]
    fn truncates_on_a_utf8_boundary_and_keeps_the_suffix() {
        let name =
            wireless_epub_filename("😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀.epub".as_bytes());
        assert!(name.len() <= UPLOAD_FILENAME_BYTES);
        assert!(name.ends_with(EPUB_SUFFIX));
        assert!(core::str::from_utf8(name.as_bytes()).is_ok());
    }

    #[test]
    fn aliases_are_legal_deterministic_and_probeable() {
        let first = upload_short_alias("A Book.epub", 0);
        assert_eq!(first, upload_short_alias("A Book.epub", 0));
        assert_ne!(first, upload_short_alias("A Book.epub", 1));
        assert_eq!(first.len(), 12);
        assert!(first.ends_with(".EPU"));
        assert!(first[..8].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
