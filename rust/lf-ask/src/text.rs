use crate::SNIPPET_CHARS;
use regex::Regex;
use std::{collections::HashSet, sync::LazyLock};

pub const STOPWORD_TEXT: &str = "a an and are as at be been but by can did do does for from had has have he her his how i if in is it its me my of on or our she so than that the their them then there these they this to us was we were what when where which who whom why will with would you your about into over said say tell told";
pub(crate) static WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+(?:'[\p{L}\p{N}]+)*").unwrap());
static STOPWORDS: LazyLock<HashSet<&str>> =
    LazyLock::new(|| STOPWORD_TEXT.split_whitespace().collect());
static PHRASE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]+)""#).unwrap());
static CITATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[(\d{1,2}(?:[\s\x1c-\x1f]*[,;][\s\x1c-\x1f]*\d{1,2})*)\]").unwrap()
});
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]{2,}").unwrap());
static PUNCT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+([.,;:!?])").unwrap());
static DECIMAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{Nd}$").unwrap());
static NONPRINTABLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{C}\p{Z}]$").unwrap());
pub(crate) fn ascii_digits(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii() || !DECIMAL.is_match(&c.to_string()) {
                return c;
            }
            let mut first = c as u32;
            while first > 0
                && char::from_u32(first - 1).is_some_and(|c| DECIMAL.is_match(&c.to_string()))
            {
                first -= 1;
            }
            char::from(b'0' + ((c as u32 - first) % 10) as u8)
        })
        .collect()
}
pub(crate) fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}
pub(crate) fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(python_whitespace).filter(|s| !s.is_empty())
}
pub(crate) fn lines(s: &str) -> impl Iterator<Item = &str> {
    s.split([
        '\n', '\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        '\u{2029}',
    ])
}
pub(crate) fn repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = quote.to_string();
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c != ' ' && NONPRINTABLE.is_match(&c.to_string()) => {
                out.push_str(&if c as u32 <= 0xff {
                    format!("\\x{:02x}", c as u32)
                } else if c as u32 <= 0xffff {
                    format!("\\u{:04x}", c as u32)
                } else {
                    format!("\\U{:08x}", c as u32)
                });
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}
pub fn stem(word: &str) -> String {
    let mut word = word.strip_suffix("'s").unwrap_or(word).replace('\'', "");
    let len = word.chars().count();
    if len > 4 && word.ends_with("ies") {
        word.truncate(word.len() - 3);
        word.push('y');
        return word;
    }
    let suffix = if len > 5 && word.ends_with("ing") {
        3
    } else if len > 4 && word.ends_with("ed") {
        2
    } else {
        0
    };
    if suffix > 0 {
        word.truncate(word.len() - suffix);
        let chars: Vec<_> = word.chars().collect();
        if chars.len() > 2
            && chars[chars.len() - 1] == chars[chars.len() - 2]
            && !"lsz".contains(chars[chars.len() - 1])
        {
            word.pop();
        }
        return word;
    }
    if len > 3 && word.ends_with("sses") {
        word.truncate(word.len() - 2);
    } else if len > 3
        && word.ends_with('s')
        && !["ss", "us", "is"].iter().any(|s| word.ends_with(s))
    {
        word.pop();
    }
    word
}
pub fn tokenize(text: &str, keep_stopwords: bool) -> Vec<String> {
    WORD_RE
        .find_iter(&text.to_lowercase())
        .map(|m| m.as_str())
        .filter(|s| keep_stopwords || !STOPWORDS.contains(s))
        .map(stem)
        .collect()
}
pub fn extract_phrases(question: &str) -> (String, Vec<Vec<String>>) {
    let phrases = PHRASE_RE
        .captures_iter(question)
        .map(|c| tokenize(&c[1], true))
        .filter(|v| !v.is_empty())
        .collect();
    (question.replace('"', " "), phrases)
}
pub fn clean_citations(answer: &str, valid: usize) -> String {
    let replaced = CITATION_RE.replace_all(answer, |c: &regex::Captures<'_>| {
        let keep: Vec<_> = c[1]
            .split([',', ';'])
            .filter_map(|n| {
                ascii_digits(n.trim_matches(python_whitespace))
                    .parse::<usize>()
                    .ok()
            })
            .filter(|n| (1..=valid).contains(n))
            .map(|n| n.to_string())
            .collect();
        if keep.is_empty() {
            String::new()
        } else {
            format!("[{}]", keep.join(", "))
        }
    });
    PUNCT
        .replace_all(&SPACES.replace_all(&replaced, " "), "$1")
        .trim_matches(python_whitespace)
        .into()
}
pub fn snippet(text: &str, terms: &HashSet<String>) -> String {
    let flat = words(text).collect::<Vec<_>>().join(" ");
    let chars: Vec<_> = flat.chars().collect();
    if chars.len() <= SNIPPET_CHARS {
        return flat;
    }
    let lower = flat.to_lowercase();
    let start = WORD_RE
        .find_iter(&lower)
        .find(|m| terms.contains(&stem(m.as_str())))
        .map(|m| lower[..m.start()].chars().count().saturating_sub(60))
        .unwrap_or(0);
    let piece = chars
        .iter()
        .skip(start)
        .take(SNIPPET_CHARS)
        .collect::<String>()
        .trim_matches(python_whitespace)
        .to_string();
    format!(
        "{}{piece}{}",
        if start > 0 { "…" } else { "" },
        if start + SNIPPET_CHARS < chars.len() {
            "…"
        } else {
            ""
        }
    )
}
pub const CHUNK_TARGET_WORDS: usize = 150;
pub const CHUNK_MAX_WORDS: usize = 200;
pub fn chunk_lines(text: &str) -> Vec<String> {
    chunk_lines_with_limits(text, CHUNK_TARGET_WORDS, CHUNK_MAX_WORDS)
}
pub fn chunk_lines_with_limits(text: &str, target: usize, maximum: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut count = 0;
    let flush = |chunks: &mut Vec<String>, current: &mut Vec<String>, count: &mut usize| {
        if !current.is_empty() {
            chunks.push(current.join("\n"));
        }
        current.clear();
        *count = 0;
    };
    for line in lines(text)
        .map(|s| s.trim_matches(python_whitespace))
        .filter(|s| !s.is_empty())
    {
        let words: Vec<_> = words(line).collect();
        if words.len() > maximum {
            flush(&mut chunks, &mut current, &mut count);
            chunks.extend(words.chunks(maximum).map(|w| w.join(" ")));
            continue;
        }
        if !current.is_empty() && count + words.len() > maximum {
            flush(&mut chunks, &mut current, &mut count);
        }
        current.push(line.to_string());
        count += words.len();
        if count >= target {
            flush(&mut chunks, &mut current, &mut count);
        }
    }
    flush(&mut chunks, &mut current, &mut count);
    chunks
}
