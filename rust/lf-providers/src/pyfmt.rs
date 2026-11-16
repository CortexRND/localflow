//! Small helpers that reproduce Python string semantics the ported code relies
//! on (`str.strip`, `str.splitlines`, `repr(str)`, `json.dumps` escaping).

/// Python `str.isspace()` for one character: Rust's `White_Space` set plus the
/// information separators U+001C..U+001F that Python also treats as spaces.
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `str.strip()` with no argument.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// Python `str.splitlines()` (no `keepends`).
pub fn py_splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let is_break = matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !is_break {
            continue;
        }
        lines.push(&s[start..i]);
        let mut end = i + c.len_utf8();
        if c == '\r' {
            if let Some(&(j, '\n')) = iter.peek() {
                iter.next();
                end = j + 1;
            }
        }
        start = end;
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// Python `repr(str)` (printable non-ASCII text is kept as is, like Python 3).
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
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
            c if (c as u32) < 0x20 || (0x7f..=0xa0).contains(&(c as u32)) || c == '\u{ad}' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// A JSON string literal exactly as Python's `json.dumps` (ensure_ascii=True)
/// writes it.
pub fn py_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e && c != '\u{7f}' => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_matches_python() {
        assert_eq!(py_strip("  a b \t\n"), "a b");
        assert_eq!(py_strip("\u{1c}x\u{1f}"), "x");
        assert_eq!(py_strip(""), "");
        assert_eq!(py_strip("\u{a0}\u{3000}"), "");
    }

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(py_splitlines("a\nb\r\nc\rd"), vec!["a", "b", "c", "d"]);
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
        assert_eq!(py_splitlines("\n\nx"), vec!["", "", "x"]);
        assert!(py_splitlines("").is_empty());
        assert_eq!(py_splitlines("a\u{2028}b\u{0c}c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr("x"), "'x'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("a\nb\\"), "'a\\nb\\\\'");
        assert_eq!(py_repr("\u{1}é"), "'\\x01é'");
    }

    #[test]
    fn json_str_matches_python_json_dumps() {
        assert_eq!(py_json_str("abc"), "\"abc\"");
        assert_eq!(py_json_str("a\"b\\c/"), "\"a\\\"b\\\\c/\"");
        assert_eq!(py_json_str("é\u{1}\u{7f}"), "\"\\u00e9\\u0001\u{7f}\"");
        assert_eq!(py_json_str("😀"), "\"\\ud83d\\ude00\"");
        assert_eq!(py_json_str("\u{8}\u{c}\t"), "\"\\b\\f\\t\"");
    }
}
