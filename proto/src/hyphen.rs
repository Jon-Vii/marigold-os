//! Where a word may break at the end of a line.
//!
//! Three sources, in the order a typesetter trusts them: soft hyphens the
//! book itself placed (U+00AD), the dashes already in a compound
//! ("well-known"), and, for a language with patterns, Liang's algorithm over
//! the TeX hyphenation patterns. A word with soft hyphens breaks only there:
//! the publisher has said where. This module only says where a break is
//! acceptable; the line builder decides whether one is worth taking.

#[path = "hyphen_en_us_generated.rs"]
mod en_us;

use heapless::Vec;

/// Breaks reported for one word, at most.
pub const MAX_WORD_BREAKS: usize = 24;
/// Letters kept before and after a pattern break (TeX's US English
/// `lefthyphenmin`/`righthyphenmin`).
const LEFT_MIN: usize = 2;
const RIGHT_MIN: usize = 3;
/// Shorter words are never split by pattern; longer ones are skipped too.
const MIN_PATTERN_WORD: usize = 5;
const MAX_PATTERN_WORD: usize = 48;
/// Letters either side of a soft hyphen or dash break.
const MIN_PART: usize = 2;
const SOFT_HYPHEN: char = '\u{00AD}';

/// Which patterns, if any, a book's text is hyphenated with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hyphenation {
    /// Only the book's own soft hyphens and dashes.
    #[default]
    None,
    EnglishUs,
}

impl Hyphenation {
    /// Patterns for a `dc:language` tag: US English for any `en` variant.
    pub fn for_language(tag: &str) -> Self {
        let primary = tag.trim().split(['-', '_']).next().unwrap_or("");
        if primary.eq_ignore_ascii_case("en") || primary.eq_ignore_ascii_case("eng") {
            Self::EnglishUs
        } else {
            Self::None
        }
    }
}

/// One acceptable break in a word: the line keeps `word[..head_end]`
/// (followed by a drawn hyphen when `hyphen`), and the next line resumes at
/// `word[tail_start..]`. The two differ only around a soft hyphen, which is
/// never drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordBreak {
    pub head_end: usize,
    pub tail_start: usize,
    pub hyphen: bool,
}

fn is_dash(ch: char) -> bool {
    matches!(ch, '-' | '\u{2010}' | '\u{2013}' | '\u{2014}')
}

fn letters_in(text: &str) -> usize {
    text.chars().filter(|ch| ch.is_alphanumeric()).count()
}

/// Every acceptable break in `word` (one whitespace-free token, punctuation
/// included) whose head is at most `max_head` bytes, in order. A line end
/// with little room passes a small `max_head`, and the pattern work past it
/// is skipped.
pub fn word_breaks(
    word: &str,
    hyphenation: Hyphenation,
    max_head: usize,
    out: &mut Vec<WordBreak, MAX_WORD_BREAKS>,
) {
    out.clear();
    let has_soft_hyphen = word.contains(SOFT_HYPHEN);
    for (index, ch) in word.char_indices() {
        if ch != SOFT_HYPHEN && !is_dash(ch) {
            continue;
        }
        let after = index + ch.len_utf8();
        if letters_in(&word[..index]) < MIN_PART || letters_in(&word[after..]) < MIN_PART {
            continue;
        }
        let soft = ch == SOFT_HYPHEN;
        if (if soft { index } else { after }) > max_head {
            break;
        }
        let _ = out.push(WordBreak {
            head_end: if soft { index } else { after },
            tail_start: after,
            hyphen: soft,
        });
    }
    if !has_soft_hyphen && hyphenation == Hyphenation::EnglishUs {
        let mut segment_start = 0;
        for (index, ch) in word.char_indices().filter(|(_, ch)| is_dash(*ch)) {
            pattern_breaks(word, segment_start, index, max_head, out);
            segment_start = index + ch.len_utf8();
        }
        pattern_breaks(word, segment_start, word.len(), max_head, out);
    }
    // Dash breaks come first, then each part's pattern breaks: a short
    // insertion sort puts them in order without the library sort's frame,
    // which would sit on the EPUB build's stack.
    for index in 1..out.len() {
        let mut at = index;
        while at > 0 && out[at - 1].head_end > out[at].head_end {
            out.swap(at - 1, at);
            at -= 1;
        }
    }
}

/// Pattern breaks inside `word[start..end]`, one dash-free part: its
/// letters, past any leading and trailing punctuation, must all be ASCII
/// and not all capitals (an acronym).
fn pattern_breaks(
    word: &str,
    start: usize,
    end: usize,
    max_head: usize,
    out: &mut Vec<WordBreak, MAX_WORD_BREAKS>,
) {
    let part = &word[start..end];
    let core = part.trim_matches(|ch: char| !ch.is_alphanumeric());
    let core_start = start + (core.as_ptr() as usize - part.as_ptr() as usize);
    let letters = core.as_bytes();
    if letters.len() < MIN_PATTERN_WORD
        || letters.len() > MAX_PATTERN_WORD
        || !letters.iter().all(u8::is_ascii_alphabetic)
        || letters.iter().all(u8::is_ascii_uppercase)
    {
        return;
    }
    let mut lower = [0u8; MAX_PATTERN_WORD];
    for (out, letter) in lower.iter_mut().zip(letters) {
        *out = letter.to_ascii_lowercase();
    }
    let lower = &lower[..letters.len()];
    // The most letters of this part a head can keep.
    let last_after = (letters.len() - RIGHT_MIN).min(max_head.saturating_sub(core_start));
    if last_after < LEFT_MIN {
        return;
    }
    let mut points = [0u8; MAX_PATTERN_WORD + 3];
    if !exception_points(lower, &mut points) {
        liang_points(lower, last_after, &mut points);
    }
    for after in LEFT_MIN..=last_after {
        if points[after + 1] % 2 == 1 {
            let at = core_start + after;
            let _ = out.push(WordBreak {
                head_end: at,
                tail_start: at,
                hyphen: true,
            });
        }
    }
}

/// Liang's values for `letters` (lowercase): `points[k + 1]` odd means a
/// break after the first `k` letters, settled for every `k` up to
/// `last_after`. Every pattern matching inside the framed word is found by
/// walking the trie from each starting letter; patterns starting past
/// `last_after + 1` cannot reach those points and are skipped.
fn liang_points(letters: &[u8], last_after: usize, points: &mut [u8; MAX_PATTERN_WORD + 3]) {
    let mut framed = [b'.'; MAX_PATTERN_WORD + 2];
    framed[1..=letters.len()].copy_from_slice(letters);
    let framed = &framed[..letters.len() + 2];
    for start in 0..framed.len().min(last_after + 2) {
        let walk = &framed[start..];
        // The first two levels by table, the rest by child scan.
        let Some(first) = letter_index(walk[0]) else {
            continue;
        };
        let mut node = usize::from(en_us::FIRST_LETTER[first]);
        if node == 0 {
            continue;
        }
        apply_values(node, 0, &mut points[start..]);
        let Some(second) = walk.get(1).and_then(|letter| letter_index(*letter)) else {
            continue;
        };
        node = usize::from(en_us::FIRST_TWO_LETTERS[first * 27 + second]);
        if node == 0 {
            continue;
        }
        apply_values(node, 1, &mut points[start..]);
        for (depth, letter) in walk.iter().enumerate().skip(2) {
            let Some(child) = child_with(node, *letter) else {
                break;
            };
            node = child;
            apply_values(node, depth, &mut points[start..]);
        }
    }
}

/// `.` is 0, `a`..`z` 1..=26: the tables' row and column order.
fn letter_index(letter: u8) -> Option<usize> {
    match letter {
        b'.' => Some(0),
        b'a'..=b'z' => Some(usize::from(letter - b'a') + 1),
        _ => None,
    }
}

/// Raises `points` to the values of the pattern ending at `node`, `depth`
/// letters below where the walk started, if a pattern ends there.
fn apply_values(node: usize, depth: usize, points: &mut [u8]) {
    let values_at = en_us::NODES[node * 3 + 1];
    if values_at == en_us::NO_VALUES {
        return;
    }
    let values_at = usize::from(values_at);
    let values = &en_us::VALUES[values_at..values_at + depth + 2];
    for (slot, value) in points.iter_mut().zip(values) {
        *slot = (*slot).max(value - b'0');
    }
}

/// The child of trie `node` for `letter`; children are few and contiguous.
fn child_with(node: usize, letter: u8) -> Option<usize> {
    let first = usize::from(en_us::NODES[node * 3]);
    let count = usize::from(en_us::NODES[node * 3 + 2] & 0xFF);
    (first..first + count).find(|child| (en_us::NODES[child * 3 + 2] >> 8) as u8 == letter)
}

/// The exception list's breaks for `letters`, in the same form as
/// `liang_points`. False when the word is not listed.
fn exception_points(letters: &[u8], points: &mut [u8; MAX_PATTERN_WORD + 3]) -> bool {
    for entry in en_us::EXCEPTIONS.split(|byte| *byte == b' ') {
        if entry.len() - entry.iter().filter(|byte| **byte == b'-').count() != letters.len()
            || !entry
                .iter()
                .filter(|byte| **byte != b'-')
                .eq(letters.iter())
        {
            continue;
        }
        let mut count = 0;
        for byte in entry {
            if *byte == b'-' {
                points[count + 1] = 1;
            } else {
                count += 1;
            }
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hyphenated(word: &str, hyphenation: Hyphenation) -> heapless::String<96> {
        let mut breaks = Vec::new();
        word_breaks(word, hyphenation, usize::MAX, &mut breaks);
        let mut out = heapless::String::new();
        let mut at = 0;
        for item in &breaks {
            out.push_str(&word[at..item.head_end]).unwrap();
            out.push(if item.hyphen { '=' } else { '|' }).unwrap();
            at = item.tail_start;
        }
        out.push_str(&word[at..]).unwrap();
        out
    }

    #[test]
    fn english_patterns_break_where_a_dictionary_would() {
        let en = Hyphenation::EnglishUs;
        assert_eq!(hyphenated("hyphenation", en), "hy=phen=ation");
        // At least three letters follow a break, as TeX does: not "comput-er".
        assert_eq!(hyphenated("computer", en), "com=puter");
        assert_eq!(hyphenated("extraordinary", en), "ex=tra=or=di=nary");
        assert_eq!(hyphenated("Typesetting", en), "Type=set=ting");
    }

    #[test]
    fn exceptions_override_patterns() {
        let en = Hyphenation::EnglishUs;
        assert_eq!(hyphenated("associate", en), "as=so=ciate");
        assert_eq!(hyphenated("project", en), "project");
        assert_eq!(hyphenated("table", en), "ta=ble");
    }

    #[test]
    fn short_words_acronyms_and_mixed_tokens_stay_whole() {
        let en = Hyphenation::EnglishUs;
        for word in [
            "the",
            "word",
            "UNESCO",
            "x86-64",
            "café",
            "don\u{2019}t",
            "http://example.org/a",
        ] {
            assert!(
                !hyphenated(word, en).contains('='),
                "{word} -> {}",
                hyphenated(word, en)
            );
        }
    }

    #[test]
    fn punctuation_around_a_word_is_kept_out_of_the_break() {
        let en = Hyphenation::EnglishUs;
        assert_eq!(
            hyphenated("\u{201C}hyphenation,\u{201D}", en),
            "\u{201C}hy=phen=ation,\u{201D}"
        );
    }

    #[test]
    fn compounds_break_after_their_dash_and_inside_their_parts() {
        let en = Hyphenation::EnglishUs;
        assert_eq!(hyphenated("well-known", en), "well-|known");
        assert_eq!(hyphenated("self-explanatory", en), "self-|ex=plana=tory");
        assert_eq!(
            hyphenated("word\u{2014}another", en),
            "word\u{2014}|an=other"
        );
        // Too little either side of the dash.
        assert_eq!(hyphenated("a-team", Hyphenation::None), "a-team");
    }

    #[test]
    fn soft_hyphens_are_the_only_breaks_when_present() {
        let en = Hyphenation::EnglishUs;
        assert_eq!(hyphenated("hyph\u{AD}enation", en), "hyph=enation");
        let mut breaks = Vec::new();
        word_breaks("hyph\u{AD}enation", en, usize::MAX, &mut breaks);
        assert_eq!(
            breaks.as_slice(),
            [WordBreak {
                head_end: 4,
                tail_start: 6,
                hyphen: true
            }]
        );
    }

    #[test]
    fn a_head_limit_keeps_only_the_breaks_that_could_fit() {
        let mut breaks = Vec::new();
        word_breaks("hyphenation", Hyphenation::EnglishUs, 6, &mut breaks);
        let heads: Vec<usize, 8> = breaks.iter().map(|item| item.head_end).collect();
        assert_eq!(heads.as_slice(), [2, 6]);
        word_breaks("hyphenation", Hyphenation::EnglishUs, 1, &mut breaks);
        assert!(breaks.is_empty());
    }

    #[test]
    fn books_without_patterns_keep_only_their_own_breaks() {
        assert_eq!(hyphenated("hyphenation", Hyphenation::None), "hyphenation");
        assert_eq!(hyphenated("well-known", Hyphenation::None), "well-|known");
        assert_eq!(Hyphenation::for_language("en-GB"), Hyphenation::EnglishUs);
        assert_eq!(Hyphenation::for_language(" EN "), Hyphenation::EnglishUs);
        assert_eq!(Hyphenation::for_language("eng"), Hyphenation::EnglishUs);
        assert_eq!(Hyphenation::for_language("sv"), Hyphenation::None);
        assert_eq!(Hyphenation::for_language(""), Hyphenation::None);
    }
}
