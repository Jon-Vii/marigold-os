//! The text an image block stores in a section's text blob.
//!
//! A section keeps every block's text in one UTF-8 blob, and an image block
//! (a [`crate::cache::BlockRecord`] with a non-zero `image_height`) uses its
//! share for a token naming the image:
//!
//! ```text
//! U+FFFC  slot (4 hex digits)  width (4 hex digits)  alt text
//! "\u{FFFC}" "002a" "01f4" "A cat asleep on a chair"
//! ```
//!
//! - `slot` is the image's entry in the book's image index
//!   ([`super::index`]), which never changes once the index is built.
//! - `width` is the image's fitted width in the layout the section was built
//!   for. Its height is the record's `image_height`.
//! - The alt text is the `alt` attribute cleaned the way the alt-text
//!   placeholder always was (entities decoded, whitespace collapsed), then
//!   trimmed, stripped of control characters, and cut to
//!   [`IMAGE_ALT_MAX_BYTES`] on a character boundary. It may be empty.
//!
//! The token satisfies every rule the spacing reflow checks a stored line
//! against, so image blocks travel through it unchanged: it is never empty,
//! at most [`IMAGE_TOKEN_MAX_BYTES`] (well under the 768-byte line limit),
//! valid UTF-8, and starts and ends with something other than ASCII
//! whitespace (U+FFFC, then either a hex digit or the trimmed alt).
//!
//! Style markers: text lines carry `ESC` + digit markers where a styled run
//! starts, and a reader of block text switches fonts on them. A token is
//! not styled text and never contains one: it does not start with a marker,
//! and cleaning drops every control character from the alt, `ESC` included.
//! Nothing reads a block's text as a token unless its record says it is an
//! image block, and the image drawer shows the alt in italic by convention,
//! as the placeholder always was.

use crate::epub::decode_html_entity;

/// First character of every token: Unicode's object replacement character,
/// "a placeholder for an object that is not text".
pub const IMAGE_TOKEN_MARK: char = '\u{FFFC}';
/// The mark and the two four-digit hex fields.
pub const IMAGE_TOKEN_HEAD_BYTES: usize = IMAGE_TOKEN_MARK.len_utf8() + 8;
/// Longest alt text a token keeps: the alt-text placeholder's bound.
pub const IMAGE_ALT_MAX_BYTES: usize = 160;
/// Longest token.
pub const IMAGE_TOKEN_MAX_BYTES: usize = IMAGE_TOKEN_HEAD_BYTES + IMAGE_ALT_MAX_BYTES;

/// A decoded token. `alt` borrows from the block text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageToken<'a> {
    pub slot: u16,
    pub width: u16,
    pub alt: &'a str,
}

/// Writes the token for an image into `out` and returns it. `alt` is the
/// raw attribute value as the parser hands it to
/// [`crate::epub::XhtmlBlockSink::push_image`]; `None` stores no alt.
///
/// The alt is cleaned straight into `out`, so the caller needs no other
/// buffer. It is cut short when `out` is shorter than
/// [`IMAGE_TOKEN_MAX_BYTES`]. Returns `None` only when `out` cannot hold
/// the [`IMAGE_TOKEN_HEAD_BYTES`].
pub fn encode_image_token<'o>(
    slot: u16,
    width: u16,
    alt: Option<&str>,
    out: &'o mut [u8],
) -> Option<&'o str> {
    let limit = out.len().min(IMAGE_TOKEN_MAX_BYTES);
    let head = out.get_mut(..IMAGE_TOKEN_HEAD_BYTES)?;
    let mark = IMAGE_TOKEN_MARK.encode_utf8(&mut head[..]).len();
    write_hex(&mut head[mark..mark + 4], slot);
    write_hex(&mut head[mark + 4..], width);
    let mut len = IMAGE_TOKEN_HEAD_BYTES;
    let mut pending_space = false;
    for ch in decoded_chars(alt.unwrap_or("")) {
        if is_html_space(ch) {
            // Collapse runs and trim: a space is written only before the
            // next visible character, and never first.
            pending_space = len > IMAGE_TOKEN_HEAD_BYTES;
            continue;
        }
        if ch.is_control() {
            continue;
        }
        let needed = ch.len_utf8() + usize::from(pending_space);
        if len + needed > limit {
            break;
        }
        if pending_space {
            out[len] = b' ';
            len += 1;
            pending_space = false;
        }
        len += ch.encode_utf8(&mut out[len..]).len();
    }
    core::str::from_utf8(&out[..len]).ok()
}

/// Reads an image block's text back. `None` when it is not a token.
pub fn decode_image_token(text: &str) -> Option<ImageToken<'_>> {
    let rest = text.strip_prefix(IMAGE_TOKEN_MARK)?;
    let digits = rest.as_bytes().get(..8)?;
    let slot = read_hex(&digits[..4])?;
    let width = read_hex(&digits[4..])?;
    // Eight ASCII digits, so byte 8 is a character boundary.
    Some(ImageToken {
        slot,
        width,
        alt: &rest[8..],
    })
}

/// `raw` with HTML entities decoded, one character at a time.
fn decoded_chars(raw: &str) -> impl Iterator<Item = char> + '_ {
    let mut cursor = 0usize;
    core::iter::from_fn(move || {
        let rest = raw.get(cursor..)?;
        if let Some(decoded) = decode_html_entity(rest) {
            cursor += rest.find(';').map_or(1, |end| end + 1);
            return Some(decoded);
        }
        let ch = rest.chars().next()?;
        cursor += ch.len_utf8();
        Some(ch)
    })
}

fn is_html_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{0C}')
}

fn write_hex(out: &mut [u8], value: u16) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in out.iter_mut().enumerate().take(4) {
        *byte = DIGITS[usize::from(value >> (12 - 4 * index) & 0xf)];
    }
}

fn read_hex(digits: &[u8]) -> Option<u16> {
    digits.iter().try_fold(0u16, |value, &digit| {
        let nibble = (digit as char).to_digit(16)?;
        Some(value << 4 | nibble as u16)
    })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::string::String;

    /// The stored-line rules the firmware's spacing reflow enforces
    /// (`reflow_book_spacing`): every token must pass them.
    fn passes_reflow_rules(text: &str) -> bool {
        !text.is_empty()
            && text.len() <= 768
            && text.trim_matches(|ch: char| ch.is_ascii_whitespace()) == text
            && !text.contains('\u{1b}')
    }

    fn token(slot: u16, width: u16, alt: Option<&str>) -> String {
        let mut out = [0u8; IMAGE_TOKEN_MAX_BYTES];
        encode_image_token(slot, width, alt, &mut out)
            .expect("token fits")
            .into()
    }

    #[test]
    fn tokens_round_trip() {
        let text = token(0x2a, 500, Some("A cat"));
        assert_eq!(text, "\u{FFFC}002a01f4A cat");
        assert_eq!(
            decode_image_token(&text),
            Some(ImageToken {
                slot: 42,
                width: 500,
                alt: "A cat",
            })
        );
        let bare = token(u16::MAX, 0, None);
        assert_eq!(bare, "\u{FFFC}ffff0000");
        assert_eq!(decode_image_token(&bare).map(|t| t.alt), Some(""));
    }

    #[test]
    fn alt_is_cleaned_like_the_placeholder_then_trimmed() {
        let text = token(1, 2, Some("  Caf&eacute;\t &amp;\n\n tea &#x2014; ok  "));
        assert_eq!(decode_image_token(&text).unwrap().alt, "Café & tea — ok");
        // Blank and empty alts store nothing.
        assert_eq!(
            decode_image_token(&token(1, 2, Some("  \n "))).unwrap().alt,
            ""
        );
        assert_eq!(decode_image_token(&token(1, 2, Some(""))).unwrap().alt, "");
    }

    #[test]
    fn alt_cannot_smuggle_style_markers_or_controls() {
        let text = token(3, 4, Some("a\u{1b}1b&#27;2c\u{7f}d\u{85}e"));
        assert_eq!(decode_image_token(&text).unwrap().alt, "a1b2cde");
    }

    #[test]
    fn long_alts_cut_on_a_character_boundary() {
        let alt: String = "é".repeat(200);
        let text = token(5, 6, Some(&alt));
        let kept = decode_image_token(&text).unwrap().alt;
        assert_eq!(kept.len(), IMAGE_ALT_MAX_BYTES);
        assert!(kept.chars().all(|ch| ch == 'é'));

        // A cut never leaves a trailing space.
        let spaced: String = "ab ".repeat(100);
        let text = token(5, 6, Some(&spaced));
        let kept = decode_image_token(&text).unwrap().alt;
        assert!(kept.len() <= IMAGE_ALT_MAX_BYTES && !kept.ends_with(' '));

        // A short buffer cuts the alt, and one under the head refuses.
        let mut small = [0u8; IMAGE_TOKEN_HEAD_BYTES + 3];
        let text = encode_image_token(7, 8, Some("abcdef"), &mut small).unwrap();
        assert_eq!(decode_image_token(text).unwrap().alt, "abc");
        let mut tiny = [0u8; IMAGE_TOKEN_HEAD_BYTES - 1];
        assert_eq!(encode_image_token(7, 8, None, &mut tiny), None);
    }

    #[test]
    fn decode_refuses_text_that_is_not_a_token() {
        for text in [
            "",
            "plain prose",
            "\u{1b}1italic",
            "\u{FFFC}",
            "\u{FFFC}0001",
            "\u{FFFC}00010g00",
            "\u{FFFC}0001 002alt",
        ] {
            assert_eq!(decode_image_token(text), None, "{text:?}");
        }
        // Upper-case digits are read too.
        assert_eq!(
            decode_image_token("\u{FFFC}00FF0A00").map(|t| (t.slot, t.width)),
            Some((255, 2560))
        );
    }

    #[test]
    fn every_token_passes_the_reflow_rules() {
        let pieces = [
            " ", "\t", "\n", "a", "é", "&amp;", "&nbsp;", "&#32;", "\u{1b}", "\u{a0}", "&bogus",
            "世界", "\u{2003}", "x y",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..5_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let mut alt = String::new();
            for step in 0..(seed % 90) {
                alt.push_str(pieces[((seed >> (step % 50)) % pieces.len() as u64) as usize]);
            }
            let slot = (seed >> 20) as u16;
            let width = (seed >> 40) as u16;
            for alt in [Some(alt.as_str()), None] {
                let text = token(slot, width, alt);
                assert!(passes_reflow_rules(&text), "{text:?}");
                assert!(text.len() <= IMAGE_TOKEN_MAX_BYTES);
                let decoded = decode_image_token(&text).expect("decodes");
                assert_eq!((decoded.slot, decoded.width), (slot, width));
            }
        }
    }
}
