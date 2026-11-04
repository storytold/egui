//! Word-boundary lookup for line-breaking scripts that are written without spaces.
//!
//! Thai, Lao, Khmer and Myanmar are written as unbroken runs of syllables:
//! Unicode line breaking ([UAX #14] class SA) needs dictionary analysis to find
//! where one word ends and the next begins. There are two ways to get word
//! boundaries:
//!
//! * the bundled dictionaries behind the `sea_dictionary` feature, or
//! * an application-installed [`LineBreaker`] via
//!   [`crate::text::Fonts::set_line_breaker`].
//!
//! [UAX #14]: https://www.unicode.org/reports/tr14/#SA

use alloc::vec::Vec;

/// Characters from the script blocks that need dictionary analysis to find
/// word boundaries: Thai, Lao, Khmer, Myanmar (UAX #14 class SA).
pub(crate) fn is_sea_script(c: char) -> bool {
    matches!(
        c as u32,
        0x0E00..=0x0E7F // Thai
            | 0x0E80..=0x0EFF // Lao
            | 0x1780..=0x17FF // Khmer
            | 0x1000..=0x109F // Myanmar
            | 0xAA60..=0xAA7F // Myanmar Extended-A
            | 0xA9E0..=0xA9FF // Myanmar Extended-B
            | 0x19E0..=0x19FF // Khmer Symbols
    )
}

/// Characters that join their base character's run: combining marks
/// (categories Mn/Mc/Me), zero-width joiners, and Thai/Lao SARA AM (category
/// Lo, so not covered by the mark test, but glued to its base all the same).
///
/// The standalone spacing vowel signs (Thai U+0E30/U+0E32, Lao U+0EB0/U+0EB2,
/// also Lo) deliberately stay OUT: they are word-start-worthy letters, not
/// joiners.
pub(crate) fn is_sea_extend(c: char) -> bool {
    matches!(c, '\u{200C}' | '\u{200D}') // ZWNJ, ZWJ
        || matches!(c, '\u{0E33}' | '\u{0EB3}') // Thai/Lao SARA AM
        || super::unicode::is_combining_mark(c)
}

/// Closing punctuation of the Southeast-Asian scripts: kinsoku, so it must
/// never start a row. It joins the preceding word instead.
pub(crate) fn is_sea_closing_punctuation(c: char) -> bool {
    matches!(
        c as u32,
        0x0E2F | 0x0E46 | 0x0E4F | 0x0E5A | 0x0E5B // Thai
            | 0x0EAF | 0x0EC6 // Lao
            | 0x17D4..=0x17D7 | 0x17D9 | 0x17DA // Khmer
            | 0x104A..=0x104C | 0x104F // Myanmar
    )
}

/// Splits `text` into runs of (byte range, is-SEA-script), extending
/// `char_indices`-style. Never splits a char.
///
/// A mark/joiner char (see [`is_sea_extend`]) inherits the kind of the run it
/// follows — so coeng U+17D2 (both a combining mark and in the Khmer block)
/// stays with its base — and a leading mark/joiner (malformed input) starts
/// its own non-SEA run.
pub(crate) fn sea_script_runs(text: &str) -> Vec<(core::ops::Range<usize>, bool)> {
    let mut runs = Vec::new();
    let mut current: Option<bool> = None;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        let is_sea = if is_sea_extend(c) {
            current.unwrap_or(false)
        } else {
            is_sea_script(c)
        };
        if current != Some(is_sea) {
            if let Some(previous) = current {
                runs.push((start..i, previous));
            }
            start = i;
            current = Some(is_sea);
        }
    }
    if let Some(previous) = current {
        runs.push((start..text.len(), previous));
    }
    runs
}

/// Maximum-matching word breaker over the bundled dictionaries.
///
/// Splits each Southeast-Asian run by the longest dictionary word at every
/// position; a position that starts no known word advances one grapheme cluster
/// (base + combining marks), so marks and coeng subscripts stay with their
/// base. Closing punctuation joins the preceding word (kinsoku), so it can
/// never start a row.
///
/// Text outside the Southeast-Asian scripts is delegated to
/// [`SimpleLineBreaker`].
#[derive(Default)]
pub struct SeaLineBreaker;

impl SeaLineBreaker {
    /// Byte ranges of words in `text`, ascending and non-overlapping.
    ///
    /// Ranges start and end on `char` boundaries. Ranges outside the scripts
    /// the breaker handles are simply not reported.
    pub fn word_boundaries(&self, text: &str) -> Vec<core::ops::Range<usize>> {
        let mut words = Vec::new();
        for (range, is_sea) in sea_script_runs(text) {
            if !is_sea {
                continue;
            }
            // `range` is char-boundary aligned by construction, so slicing is safe.
            let run = &text[range.clone()];
            let run_start = range.start;
            let mut pos = 0;
            while pos < run.len() {
                let word_len =
                    if let Some(len) = super::line_breaker_dict::longest_word(&run[pos..]) {
                        len
                    } else {
                        // No dictionary word starts here: advance one grapheme
                        // unit (first char + following joiners) — a grapheme
                        // cluster by construction, so marks and coeng
                        // subscripts stay with their base.
                        let Some(first) = run[pos..].chars().next() else {
                            break;
                        };
                        let mut end = pos + first.len_utf8();
                        while let Some(next) = run[end..].chars().next()
                            && is_sea_extend(next)
                        {
                            end += next.len_utf8();
                        }
                        end - pos
                    };
                let mut end = pos + word_len;
                // Kinsoku: closing punctuation joins the preceding word, so
                // the boundary lands after the punctuation.
                while let Some(next) = run[end..].chars().next()
                    && is_sea_closing_punctuation(next)
                {
                    end += next.len_utf8();
                }
                words.push(run_start + pos..run_start + end);
                pos = end;
            }
        }
        words
    }
}

impl super::LineBreaker for SeaLineBreaker {
    fn line_breaks(&self, text: &str, language: Option<&str>, out: &mut Vec<super::LineBreak>) {
        // The generic rules still apply (whitespace, CJK, hyphens, …):
        super::SimpleLineBreaker.line_breaks(text, language, out);
        // A row may also end at a dictionary word end:
        for range in self.word_boundaries(text) {
            out.push(super::LineBreak::soft(range.end));
        }
    }
}

#[cfg(all(test, feature = "sea_dictionary"))]
mod tests {
    use alloc::{vec, vec::Vec};
    use unicode_segmentation::UnicodeSegmentation as _;

    use super::super::LineBreaker as _;
    use super::*;

    /// Assert the structural contract of [`SeaLineBreaker::word_boundaries`] on
    /// the given text: ascending, non-overlapping, non-empty, char-boundary
    /// aligned, and every boundary a grapheme boundary.
    fn assert_ranges_wellformed(text: &str, ranges: &[core::ops::Range<usize>]) {
        let mut previous_end = 0;
        for range in ranges {
            assert!(
                previous_end <= range.start && range.start < range.end,
                "ranges must be ascending, non-overlapping and non-empty"
            );
            assert!(
                text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
                "range {range:?} of {text:?} is not char-boundary aligned"
            );
            // `grapheme_indices` yields the start of every cluster but never
            // `text.len()`, which is a valid boundary (end of text).
            let boundaries = |needle: usize| {
                core::iter::once(text.len())
                    .chain(text.grapheme_indices(true).map(|(i, _)| i))
                    .any(|i| i == needle)
            };
            assert!(
                boundaries(range.start) && boundaries(range.end),
                "range {range:?} of {text:?} splits a grapheme cluster"
            );
            previous_end = range.end;
        }
    }

    fn words(text: &str) -> Vec<&str> {
        let ranges = SeaLineBreaker.word_boundaries(text);
        assert_ranges_wellformed(text, &ranges);
        ranges.iter().map(|r| &text[r.start..r.end]).collect()
    }

    #[test]
    fn sea_word_boundaries() {
        assert_eq!(words("สวัสดีครับ"), vec!["สวัสดี", "ครับ"]);
        assert_eq!(words("ພາສາລາວສະບາຍດີ"), vec!["ພາສາ", "ລາວ", "ສະບາຍດີ"]);
        // The `ម្ព` coeng cluster stays whole.
        assert_eq!(words("ភាសាខ្មែរកម្ពុជា"), vec!["ភាសាខ្មែរ", "កម្ពុជា"]);
        assert_eq!(words("မြန်မာနိုင်ငံသည်"), vec!["မြန်မာနိုင်ငံ", "သည်"]);
        // The issue's repro sentence.
        assert_eq!(
            words("ภาษาไทยใช้การเขียนต่อเนื่องโดยไม่คั่นคำด้วยช่องว่าง"),
            vec![
                "ภาษา",
                "ไทย",
                "ใช้",
                "การ",
                "เขียน",
                "ต่อ",
                "เนื่อง",
                "โดย",
                "ไม่",
                "คั่น",
                "คำ",
                "ด้วย",
                "ช่อง",
                "ว่าง",
            ]
        );
    }

    #[test]
    fn closing_punctuation_joins_the_preceding_word() {
        let text = "កម្ពុជាមានប្រជាជនច្រើន។ខ្មែរជាភាសា។";
        let ranges = SeaLineBreaker.word_boundaries(text);
        assert_ranges_wellformed(text, &ranges);
        for range in &ranges {
            assert_ne!(
                &text[range.start..range.end],
                "។",
                "no word range may consist only of closing punctuation"
            );
        }
        let ends: Vec<&str> = ranges
            .iter()
            .filter(|r| text[r.start..r.end].ends_with('។'))
            .map(|r| &text[r.start..r.end])
            .collect();
        assert_eq!(ends, vec!["ច្រើន។", "ភាសា។"]);
    }

    #[test]
    fn latin_and_cjk_text_is_untouched() {
        assert_eq!(words("hello world"), Vec::<&str>::new());
        assert_eq!(words("今日は良い天気ですね"), Vec::<&str>::new());
    }

    /// The trait adapter reports every word end as a soft break, on top of the
    /// generic rules.
    #[test]
    fn line_breaks_end_at_word_ends() {
        let text = "สวัสดีครับ";
        let mut breaks = Vec::new();
        SeaLineBreaker.line_breaks(text, Some("th"), &mut breaks);
        let offsets: Vec<usize> = breaks.iter().map(|b| b.offset).collect();
        assert!(
            offsets.contains(&"สวัสดี".len()),
            "the first word ends at its byte length: {offsets:?}"
        );
        assert!(
            offsets.contains(&text.len()),
            "the last word ends at the end of the text: {offsets:?}"
        );
    }

    /// Hostile input: the breaker never slices at a non-char boundary and
    /// never reports an empty or out-of-bounds range.
    #[test]
    fn hostile_input_is_safe() {
        for text in ["", "\u{0E33}", "\u{0E01}\u{0E33}\u{0E01}", "\u{FFFD}", "\u{200D}"] {
            let ranges = SeaLineBreaker.word_boundaries(text);
            assert_ranges_wellformed(text, &ranges);
        }
    }
}
