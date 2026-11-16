//! Greedy word wrap with an injected measure (port of `localflow/text_wrap.py`).
//!
//! Slicing is by Unicode scalar value, like Python `str` slicing.

use crate::tokens::Weight;

/// Injectable text-width measurement: `(text, font size, weight) -> width`.
/// AppKit callers pass a real font measurement, tests pass a fake one.
pub type TextMeasure<'a> = &'a dyn Fn(&str, f64, Weight) -> f64;

/// Greedy word wrap of `text` into lines no wider than `width`.
///
/// `measure_line(&str) -> width`. A single word wider than `width` is broken
/// by characters, like a wrapping label does.
pub fn wrap_lines(text: &str, width: f64, measure_line: &dyn Fn(&str) -> f64) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        for word in paragraph.split(' ') {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if measure_line(&candidate) <= width {
                current = candidate;
                continue;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let mut rest: Vec<char> = word.chars().collect();
            while rest.len() > 1 && measure_line(&rest.iter().collect::<String>()) > width {
                let mut cut = rest.len() - 1;
                while cut > 1 && measure_line(&rest[..cut].iter().collect::<String>()) > width {
                    cut -= 1;
                }
                lines.push(rest[..cut].iter().collect());
                rest.drain(..cut);
            }
            current = rest.into_iter().collect();
        }
        lines.push(current);
    }
    lines
}

/// [`wrap_lines`] with a three-argument measure bound to a font size/weight.
pub fn wrap_text(
    text: &str,
    width: f64,
    measure: TextMeasure<'_>,
    size: f64,
    weight: Weight,
) -> Vec<String> {
    wrap_lines(text, width, &|s: &str| measure(s, size, weight))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> f64 {
        s.chars().count() as f64 * 10.0
    }

    #[test]
    fn wraps_words_greedily() {
        assert_eq!(wrap_lines("aa bb cc", 50.0, &m), vec!["aa bb", "cc"]);
    }

    #[test]
    fn empty_text_is_one_empty_line() {
        assert_eq!(wrap_lines("", 50.0, &m), vec![""]);
    }

    #[test]
    fn paragraphs_are_kept() {
        assert_eq!(wrap_lines("a\n\nb", 50.0, &m), vec!["a", "", "b"]);
    }

    #[test]
    fn long_word_is_broken_by_characters() {
        assert_eq!(wrap_lines("abcdefgh", 30.0, &m), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn slices_by_unicode_scalar() {
        assert_eq!(
            wrap_lines("日本語日本語", 30.0, &m),
            vec!["日本語", "日本語"]
        );
        assert_eq!(wrap_lines("héllo", 30.0, &m), vec!["hél", "lo"]);
    }

    #[test]
    fn narrow_width_keeps_single_chars() {
        assert_eq!(wrap_lines("abc", 1.0, &m), vec!["a", "b", "c"]);
    }

    #[test]
    fn text_measure_closure_gets_size_and_weight() {
        let f = |s: &str, size: f64, w: Weight| {
            s.chars().count() as f64 * size * if w == Weight::Bold { 2.0 } else { 1.0 }
        };
        assert_eq!(
            wrap_text("aa bb", 40.0, &f, 10.0, Weight::Bold),
            vec!["aa", "bb"]
        );
        assert_eq!(
            wrap_text("aa bb", 50.0, &f, 10.0, Weight::Regular),
            vec!["aa bb"]
        );
    }
}
