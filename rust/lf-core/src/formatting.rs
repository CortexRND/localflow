//! Convert spoken formatting commands and apply deterministic writing styles
//! (port of `localflow/formatting.py`).

use std::sync::LazyLock;

use crate::symbols::pycompat::re::{self, Regex};
use crate::symbols::pycompat::{casefold, chars, is_space, is_word, isupper, lower, strip};

const DETERMINERS: [&str; 18] = [
    "a", "an", "the", "my", "your", "our", "his", "her", "their", "its", "this", "that", "any",
    "one", "every", "each", "no", "another",
];

/// `_PUNCTUATION` sorted by length descending (stable), i.e. the alternation
/// order of `_SPOKEN_PUNCTUATION`.
const PUNCTUATION: [(&str, &str); 8] = [
    ("exclamation point", "!"),
    ("exclamation mark", "!"),
    ("question mark", "?"),
    ("full stop", "."),
    ("semicolon", ";"),
    ("period", "."),
    ("comma", ","),
    ("colon", ":"),
];

const STYLE_NAMES: [&str; 4] = ["formal", "casual", "very_casual", "excited"];

/// `[ \t]*[,;]*[ \t]*(?<!\w)(?P<command>new paragraph|new line)\b[.,!?;:]*[ \t]*` IGNORECASE
static BREAK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        re::seq(vec![
            re::star(re::set(" \t")),
            re::star(re::set(",;")),
            re::star(re::set(" \t")),
            re::not_after(is_word),
            re::group(
                1,
                re::alt(vec![re::lit_ci("new paragraph"), re::lit_ci("new line")]),
            ),
            re::boundary(),
            re::star(re::set(".,!?;:")),
            re::star(re::set(" \t")),
        ]),
        1,
    )
});

/// `[ \t]*,?[ \t]*(?<!\w)(?P<command>...)\b(?=(?:[ \t]*[.,!?])|[\r\n]|[ \t]*$)[ \t]*[.,!?]*`
static SPOKEN_PUNCTUATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        re::seq(vec![
            re::star(re::set(" \t")),
            re::opt(re::ch(',')),
            re::star(re::set(" \t")),
            re::not_after(is_word),
            re::group(
                1,
                re::alt(PUNCTUATION.iter().map(|(k, _)| re::lit_ci(k)).collect()),
            ),
            re::boundary(),
            re::ahead(re::alt(vec![
                re::seq(vec![re::star(re::set(" \t")), re::set(".,!?")]),
                re::set("\r\n"),
                re::seq(vec![re::star(re::set(" \t")), re::dollar()]),
            ])),
            re::star(re::set(" \t")),
            re::star(re::set(".,!?")),
        ]),
        1,
    )
});

/// `\bscratch\s+that\b[.,!?;:]*` IGNORECASE
static SCRATCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        re::seq(vec![
            re::boundary(),
            re::lit_ci("scratch"),
            re::plus(re::space()),
            re::lit_ci("that"),
            re::boundary(),
            re::star(re::set(".,!?;:")),
        ]),
        0,
    )
});

/// `([.?!]|\n+)([ \t]*)([a-z])`
static CAPITALIZE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        re::seq(vec![
            re::group(1, re::alt(vec![re::set(".?!"), re::plus(re::ch('\n'))])),
            re::group(2, re::star(re::set(" \t"))),
            re::group(3, re::pred(|c| c.is_ascii_lowercase())),
        ]),
        3,
    )
});

/// `\w+`
static WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(re::plus(re::word()), 0));

fn slice(text: &[char], start: usize, end: usize) -> String {
    let end = end.min(text.len());
    let start = start.min(end);
    text[start..end].iter().collect()
}

/// Which alternative an IGNORECASE match came from. Python looks the matched
/// text up by `casefold()`, which raises KeyError for spellings only
/// IGNORECASE equates (e.g. "exclamatıon point"); we use the alternative.
fn punctuation_for(matched: &str) -> &'static str {
    let folded = casefold(matched);
    if let Some((_, v)) = PUNCTUATION.iter().find(|(k, _)| *k == folded) {
        return v;
    }
    let text = chars(matched);
    PUNCTUATION
        .iter()
        .find(|(k, _)| Regex::new(re::lit_ci(k), 0).fullmatch(&text))
        .map(|(_, v)| *v)
        .unwrap_or("")
}

/// Turn "new line", "new paragraph", spoken punctuation and "scratch that"
/// into text edits, then capitalize sentence starts.
pub fn apply_spoken_formatting(text: &str) -> String {
    let original = chars(text);
    let text = BREAK.sub_chars(&original, |m| {
        let (cmd_start, _) = m.span(1).unwrap_or((m.start, m.start));
        let prefix = slice(&original, 0, cmd_start);
        let prefix = prefix.trim_end_matches([' ', '\t', ',', ';']);
        // re.search(r"(\w+)$", prefix): `$` also matches before a final "\n".
        let prefix = prefix.strip_suffix('\n').unwrap_or(prefix);
        let prefix_chars = chars(prefix);
        let previous_start = prefix_chars
            .iter()
            .rposition(|&c| !is_word(c))
            .map(|i| i + 1)
            .unwrap_or(0);
        let previous = slice(&prefix_chars, previous_start, prefix_chars.len());
        let rest = &original[m.end..];
        let skip = rest.iter().take_while(|&&c| is_space(c)).count();
        let following: String = rest[skip..].iter().take_while(|&&c| is_word(c)).collect();
        if !previous.is_empty() && DETERMINERS.contains(&casefold(&previous).as_str()) {
            return m.text(&original);
        }
        if !following.is_empty() && casefold(&following) == "of" {
            return m.text(&original);
        }
        let command = m.group(&original, 1).unwrap_or_default();
        if casefold(&command) == "new paragraph" {
            "\n\n".to_string()
        } else {
            "\n".to_string()
        }
    });
    let text = SPOKEN_PUNCTUATION.sub(&text, |m, t| {
        punctuation_for(&m.group(t, 1).unwrap_or_default()).to_string()
    });

    let base = chars(&text);
    let mut current = base.clone();
    let matches = SCRATCH.find_iter(&base);
    let marks = ".,!?;:";
    for found in matches.iter().rev() {
        // Positions are from the pre-loop text (as in Python), applied to the
        // progressively edited text.
        let prefix_full = slice(&current, 0, found.start);
        let prefix = prefix_full.trim_end_matches([' ', '\t', ',', '.', '!', '?', ';', ':']);
        let prefix_chars = chars(prefix);
        let suffix = slice(&current, found.end, current.len());
        let before = prefix.trim_end_matches([' ', '\t']);
        let group: String = found.text(&base);
        let prior_boundary =
            before.is_empty() || before.ends_with(|c| (marks.to_string() + "\n").contains(c));
        let next_boundary = suffix.is_empty()
            || group.ends_with(|c| marks.contains(c))
            || suffix.starts_with(|c| (marks.to_string() + "\n").contains(c));
        if !prior_boundary && !next_boundary {
            continue;
        }
        let boundary: isize = prefix_chars
            .iter()
            .rposition(|c| ".?!\n".contains(*c))
            .map(|i| i as isize)
            .unwrap_or(-1);
        let mut suffix = suffix;
        let boundary_is_newline = boundary >= 0 && current.get(boundary as usize) == Some(&'\n');
        if suffix.starts_with('\n') && (boundary < 0 || boundary_is_newline) {
            suffix = suffix.trim_start_matches([' ', '\t']).to_string();
            suffix = suffix.strip_prefix('\n').unwrap_or(&suffix).to_string();
        }
        let keep = slice(&current, 0, (boundary + 1) as usize);
        current = chars(&(keep + &suffix));
    }
    let text: String = current.iter().collect();
    let text = text.trim_matches([' ', '\t']);
    CAPITALIZE.sub(text, |m, t| {
        let mut out = m.group(t, 1).unwrap_or_default();
        out.push_str(&m.group(t, 2).unwrap_or_default());
        out.push_str(&m.group(t, 3).unwrap_or_default().to_uppercase());
        out
    })
}

/// Apply a deterministic writing style ("formal", "casual", "very_casual",
/// "excited"); unknown styles leave the text unchanged.
pub fn apply_style<S: AsRef<str>>(text: &str, style: &str, vocabulary: &[S]) -> String {
    if !STYLE_NAMES.contains(&style) {
        return text.to_string();
    }
    let mut text: Vec<char> = chars(text);
    if style == "casual" || style == "very_casual" {
        // re.search(r"\.(\s*)$", text): the leftmost "." followed only by
        // whitespace (`$` may also sit before a final newline, which \s*
        // consumes anyway).
        let tail = text.iter().rev().take_while(|&&c| is_space(c)).count();
        let dot = text.len() - tail;
        if dot > 0 && text[dot - 1] == '.' {
            let start = dot - 1;
            let has_newline = text.contains(&'\n');
            let earlier = text[..start].iter().any(|c| ".!?".contains(*c));
            if style == "very_casual" || (!has_newline && !earlier) {
                text.remove(start);
            }
        }
    }
    if style == "very_casual" {
        if let Some(first) = WORDS.search(&text) {
            let word = first.text(&text);
            let protected: Vec<String> = vocabulary
                .iter()
                .map(|v| strip(v.as_ref()).to_string())
                .filter(|v| !v.is_empty())
                .map(|v| casefold(&v))
                .collect();
            let folded = casefold(&word);
            if folded != "i" && !isupper(&word) && !protected.contains(&folded) {
                let lowered = lower(&text[first.start].to_string());
                let mut out: String = text[..first.start].iter().collect();
                out.push_str(&lowered);
                out.extend(&text[first.start + 1..]);
                return out;
            }
        }
    }
    text.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoken_formatting_table() {
        let cases = [
            ("send it, period.", "send it."),
            ("send it full stop. next thought", "send it. Next thought"),
            ("the trial period ends soon", "the trial period ends soon"),
            (
                "the question mark is visible",
                "the question mark is visible",
            ),
            ("is it ready question mark", "is it ready?"),
            ("a new line b", "a new line b"),
            ("hello new line world", "hello\nWorld"),
            (
                "I need a new line of credit.",
                "I need a new line of credit.",
            ),
            ("my new line of work", "my new line of work"),
            ("renew line today", "renew line today"),
            ("hello, new paragraph. two", "hello\n\nTwo"),
            ("Call me. New line. Thanks", "Call me.\nThanks"),
            ("Hi, new paragraph, thanks", "Hi\n\nThanks"),
            ("scratch that. keep this", "keep this"),
            ("I went home. Scratch that. I stayed in.", "I stayed in."),
            ("A. B. Scratch that.", "A."),
            ("Scratch that.", ""),
            ("Do not scratch that itch.", "Do not scratch that itch."),
            (
                "I went home scratch that I stayed",
                "I went home scratch that I stayed",
            ),
            ("remove this scratch that\nlast line", "last line"),
            (
                "first clause. remove this, scratch that. keep this",
                "first clause. Keep this",
            ),
            (
                "first line\nremove this scratch that\nlast line",
                "first line\nLast line",
            ),
        ];
        for (source, expected) in cases {
            assert_eq!(apply_spoken_formatting(source), expected, "{source:?}");
        }
    }

    #[test]
    fn spoken_symbols_absorb_whisper_punctuation_and_capitalize() {
        assert_eq!(apply_spoken_formatting("send it comma."), "send it,");
        assert_eq!(
            apply_spoken_formatting("send it exclamation point! next"),
            "send it! Next"
        );
        assert_eq!(apply_spoken_formatting("new line."), "\n");
    }

    #[test]
    fn spoken_formatting_empty_and_unicode() {
        assert_eq!(apply_spoken_formatting(""), "");
        assert_eq!(apply_spoken_formatting("  \t "), "");
        // Python IGNORECASE: dotless ı matches "i" in the command words.
        assert_eq!(
            apply_spoken_formatting("hello new lıne world"),
            "hello\nWorld"
        );
        assert_eq!(
            apply_spoken_formatting("is it ready questıon mark"),
            "is it ready?"
        );
        // é is a word character, so "café period" punctuates but "écomma" doesn't.
        assert_eq!(apply_spoken_formatting("café period"), "café.");
        assert_eq!(apply_spoken_formatting("écomma"), "écomma");
        assert_eq!(apply_spoken_formatting("done. ärger"), "done. ärger");
        // Python applies the pre-loop match positions to the edited text, so
        // the earlier match then truncates everything (hand-traced: "").
        assert_eq!(
            apply_spoken_formatting("multi scratch that. scratch that. ok"),
            ""
        );
        assert_eq!(
            apply_spoken_formatting("one. two scratch that. three"),
            "one. Three"
        );
    }

    #[test]
    fn styles() {
        let empty: [&str; 0] = [];
        let cases: [(&str, &str, &[&str], &str); 9] = [
            ("Hello.", "formal", &[], "Hello."),
            ("Hello.", "casual", &[], "Hello"),
            ("Hello. Again.", "casual", &[], "Hello. Again."),
            ("Hello.\nAgain.", "casual", &[], "Hello.\nAgain."),
            ("Hello world.", "very_casual", &[], "hello world"),
            ("I agree.", "very_casual", &[], "I agree"),
            ("NASA agrees.", "very_casual", &[], "NASA agrees"),
            ("Ada agrees.", "very_casual", &["Ada"], "Ada agrees"),
            ("Hello!", "excited", &[], "Hello!"),
        ];
        for (text, style, vocabulary, expected) in cases {
            assert_eq!(
                apply_style(text, style, vocabulary),
                expected,
                "{text:?} {style}"
            );
        }
        assert_eq!(
            apply_style("HeLLo world.", "very_casual", &empty),
            "heLLo world"
        );
        assert_eq!(apply_style("Hello.  \n", "casual", &empty), "Hello.  \n"); // newline in text
        assert_eq!(apply_style("Hello.  ", "casual", &empty), "Hello  ");
        assert_eq!(apply_style("", "very_casual", &empty), "");
        assert_eq!(apply_style("...", "very_casual", &empty), "..");
        assert_eq!(apply_style("Hello.", "unknown", &empty), "Hello.");
        assert_eq!(
            apply_style("İstanbul.", "very_casual", &empty),
            "i\u{307}stanbul"
        );
        assert_eq!(
            apply_style("Ada agrees.", "very_casual", &["  ada  "]),
            "Ada agrees"
        );
    }
}
