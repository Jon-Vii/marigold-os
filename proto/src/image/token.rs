//! The text that names an image in a section's text blob: an image block's
//! token, and the tokens a line of text carries for its inline images.
//!
//! A section keeps every block's text in one UTF-8 blob, and an image block
//! (a [`crate::cache::BlockRecord`] whose [`is_image`] holds) uses its share
//! for a token naming the image:
//!
//! [`is_image`]: crate::cache::BlockRecord::is_image
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
//!
//! # Inline images
//!
//! An image inside a line of text is a token of its own in the line, with
//! no alt (an inline image that cannot be drawn shows an outline):
//!
//! ```text
//! U+FFFC  slot (4 hex digits)  width (4 hex digits)  height (4 hex digits)
//! "Complete the \u{FFFC}002a0011000c AEBG"
//! ```
//!
//! The width and height are the image's fitted size in the layout the
//! section was built for. Only a line whose record says it holds inline
//! images ([`crate::cache::BlockRecord::has_inline_images`]) is read for
//! these tokens, so the same characters in a book's own text are never
//! taken for one; a sink that writes tokens drops U+FFFC from the book's
//! text besides. A U+FFFC that is not followed by twelve hex digits is
//! text. A token holds no space, no style marker and no control character,
//! so the stored-line rules hold for a line that carries tokens as they do
//! for any other.

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

/// Bytes of an inline image token: the mark and three four-digit hex fields.
pub const INLINE_IMAGE_TOKEN_BYTES: usize = IMAGE_TOKEN_MARK.len_utf8() + 12;

/// [`IMAGE_TOKEN_MARK`] as UTF-8.
const MARK_BYTES: [u8; 3] = [0xEF, 0xBF, 0xBC];
const _: () = assert!(IMAGE_TOKEN_MARK.len_utf8() == MARK_BYTES.len());

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

/// An image inside a line of text, as its token names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineImageToken {
    pub slot: u16,
    pub width: u16,
    pub height: u16,
}

/// Writes the token for an inline image into `out` and returns it.
pub fn encode_inline_image_token(
    token: InlineImageToken,
    out: &mut [u8; INLINE_IMAGE_TOKEN_BYTES],
) -> &str {
    out[..MARK_BYTES.len()].copy_from_slice(&MARK_BYTES);
    let fields = [token.slot, token.width, token.height];
    for (index, value) in fields.into_iter().enumerate() {
        let at = MARK_BYTES.len() + 4 * index;
        write_hex(&mut out[at..at + 4], value);
    }
    // The mark, then ASCII digits.
    core::str::from_utf8(&out[..]).unwrap_or("")
}

/// The inline token that `bytes` starts with, if they start with a whole
/// one. Works on raw bytes, so a caller can scan a line read a piece at a
/// time without checking each piece is UTF-8.
pub fn inline_image_token_at(bytes: &[u8]) -> Option<InlineImageToken> {
    let token = bytes.get(..INLINE_IMAGE_TOKEN_BYTES)?;
    if token[..MARK_BYTES.len()] != MARK_BYTES {
        return None;
    }
    let field = |index: usize| {
        let at = MARK_BYTES.len() + 4 * index;
        read_hex(&token[at..at + 4])
    };
    Some(InlineImageToken {
        slot: field(0)?,
        width: field(1)?,
        height: field(2)?,
    })
}

/// A piece of a line that holds inline images: a run of text (style
/// markers and all), or an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlinePiece<'a> {
    Text(&'a str),
    Image(InlineImageToken),
}

/// Splits a line that holds inline images into runs of text and images, in
/// order. Text runs are never empty.
pub fn inline_pieces(line: &str) -> InlinePieces<'_> {
    InlinePieces { line, cursor: 0 }
}

/// The iterator [`inline_pieces`] returns.
#[derive(Clone)]
pub struct InlinePieces<'a> {
    line: &'a str,
    cursor: usize,
}

impl<'a> Iterator for InlinePieces<'a> {
    type Item = InlinePiece<'a>;

    fn next(&mut self) -> Option<InlinePiece<'a>> {
        let rest = self
            .line
            .get(self.cursor..)
            .filter(|rest| !rest.is_empty())?;
        if let Some(token) = inline_image_token_at(rest.as_bytes()) {
            self.cursor += INLINE_IMAGE_TOKEN_BYTES;
            return Some(InlinePiece::Image(token));
        }
        // Text runs to the next whole token. The mark is three bytes that
        // only ever begin a character, so every place found is a
        // character boundary.
        let bytes = rest.as_bytes();
        let mut end = rest.len();
        let mut search = 1;
        while let Some(found) = find_mark(&bytes[search..]) {
            let at = search + found;
            if inline_image_token_at(&bytes[at..]).is_some() {
                end = at;
                break;
            }
            search = at + MARK_BYTES.len();
        }
        self.cursor += end;
        Some(InlinePiece::Text(&rest[..end]))
    }
}

/// Where the next mark starts in `bytes`.
fn find_mark(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(MARK_BYTES.len())
        .position(|window| window == MARK_BYTES)
}

/// The inline images in a line, in order.
pub fn inline_image_tokens(line: &str) -> impl Iterator<Item = InlineImageToken> + '_ {
    inline_pieces(line).filter_map(|piece| match piece {
        InlinePiece::Image(token) => Some(token),
        InlinePiece::Text(_) => None,
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
    use std::format;
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

    fn inline(slot: u16, width: u16, height: u16) -> String {
        let mut out = [0u8; INLINE_IMAGE_TOKEN_BYTES];
        encode_inline_image_token(
            InlineImageToken {
                slot,
                width,
                height,
            },
            &mut out,
        )
        .into()
    }

    #[test]
    fn inline_tokens_round_trip_and_split_their_line() {
        let symbol = InlineImageToken {
            slot: 0x2a,
            width: 17,
            height: 12,
        };
        let text = inline(0x2a, 17, 12);
        assert_eq!(text, "\u{FFFC}002a0011000c");
        assert_eq!(text.len(), INLINE_IMAGE_TOKEN_BYTES);
        assert_eq!(inline_image_token_at(text.as_bytes()), Some(symbol));
        assert!(passes_reflow_rules(&text));

        // Hex digits after a token are the next word's, not the token's.
        let line = format!("Complete the {text}AEBG. Then {text}");
        let pieces: std::vec::Vec<_> = inline_pieces(&line).collect();
        assert_eq!(
            pieces,
            [
                InlinePiece::Text("Complete the "),
                InlinePiece::Image(symbol),
                InlinePiece::Text("AEBG. Then "),
                InlinePiece::Image(symbol),
            ]
        );
        assert_eq!(inline_image_tokens(&line).count(), 2);
        let line = format!("{text}{}", inline(1, 64, 64));
        assert_eq!(
            inline_pieces(&line).collect::<std::vec::Vec<_>>(),
            [
                InlinePiece::Image(symbol),
                InlinePiece::Image(InlineImageToken {
                    slot: 1,
                    width: 64,
                    height: 64,
                }),
            ]
        );
    }

    #[test]
    fn a_mark_without_a_whole_token_is_text() {
        let whole = inline(3, 4, 5);
        for line in [
            "\u{FFFC}",
            "a \u{FFFC}0003 b",
            "\u{FFFC}00030004000g",
            "\u{FFFC}\u{FFFC}",
            "plain \u{1b}2styled",
        ] {
            let pieces: std::vec::Vec<_> = inline_pieces(line).collect();
            assert_eq!(pieces, [InlinePiece::Text(line)], "{line:?}");
        }
        // A broken token, then a whole one: the broken one stays text.
        let line = format!("x\u{FFFC}12{whole}y");
        let pieces: std::vec::Vec<_> = inline_pieces(&line).collect();
        assert_eq!(
            pieces,
            [
                InlinePiece::Text("x\u{FFFC}12"),
                InlinePiece::Image(InlineImageToken {
                    slot: 3,
                    width: 4,
                    height: 5,
                }),
                InlinePiece::Text("y"),
            ]
        );
        assert_eq!(inline_pieces("").count(), 0);
        // Cut anywhere, a token reads only when whole.
        for cut in 0..whole.len() {
            assert_eq!(inline_image_token_at(&whole.as_bytes()[..cut]), None);
        }
    }

    #[test]
    fn inline_pieces_cover_every_line_in_order() {
        let pieces = ["a", "é ", "\u{FFFC}", "0", "\u{1b}1", " ", "世"];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..3_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let mut line = String::new();
            for step in 0..(seed % 24) {
                let pick = (seed >> (step % 40)) % (pieces.len() as u64 + 1);
                match pieces.get(pick as usize) {
                    Some(piece) => line.push_str(piece),
                    None => line.push_str(&inline((seed >> 8) as u16, 9, 7)),
                }
            }
            // Joined back up, the pieces are the line.
            let mut joined = String::new();
            for piece in inline_pieces(&line) {
                match piece {
                    InlinePiece::Text(text) => {
                        assert!(!text.is_empty());
                        joined.push_str(text);
                    }
                    InlinePiece::Image(token) => {
                        joined.push_str(&inline(token.slot, token.width, token.height))
                    }
                }
            }
            assert_eq!(joined, line);
        }
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
