//! Python-compatible text helpers shared by the storage/text modules.
//!
//! Mounted from `symbols.rs` (`lf_core::symbols::pycompat`); not declared in
//! lib.rs. Everything here mirrors a specific CPython behaviour the ported
//! modules rely on: `str.strip/split/lower/casefold/isupper`, the `re` module's
//! Unicode `\w`/`\s`/`\b` classes and IGNORECASE matching, `repr(str)`,
//! `json.dumps` formatting, and the sidecar `.lock` file convention.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde_json::Value;

// ------------------------------------------------------------ char classes ---

static WORD_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\A[\p{L}\p{N}_]\z").expect("word class"));
static ND_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\A\p{Nd}\z").expect("digit class"));
static NONPRINTABLE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\A[\p{Cc}\p{Cf}\p{Co}\p{Cn}\p{Zl}\p{Zp}\p{Zs}]\z").expect("printable class")
});

fn char_matches(re: &regex::Regex, c: char) -> bool {
    let mut buf = [0u8; 4];
    re.is_match(c.encode_utf8(&mut buf))
}

/// Python `re` `\w` for str patterns: `ch.isalnum() or ch == "_"`, i.e.
/// Unicode letters (L*) and numbers (N*) plus underscore. Combining marks are
/// *not* word characters (unlike Rust regex's `\w`).
pub fn is_word(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric() || c == '_';
    }
    char_matches(&WORD_RE, c)
}

/// Python `[^\W_]`: a word character other than underscore (`str.isalnum`).
pub fn is_alnum(c: char) -> bool {
    c != '_' && is_word(c)
}

/// Python `str.isspace` / `re` `\s`: Unicode White_Space plus the ASCII
/// information separators U+001C..U+001F.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `re` `\d` (Unicode decimal digits, category Nd).
pub fn is_decimal(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    char_matches(&ND_RE, c)
}

/// Value of a Unicode decimal digit (`int()` semantics). Nd characters come
/// in contiguous runs of whole 0..9 blocks.
pub fn decimal_value(c: char) -> u32 {
    if c.is_ascii_digit() {
        return c as u32 - '0' as u32;
    }
    let mut start = c as u32;
    while start > 0 {
        match char::from_u32(start - 1) {
            Some(prev) if is_decimal(prev) => start -= 1,
            _ => break,
        }
    }
    (c as u32 - start) % 10
}

/// `int()` of a run of `\d` characters; None on overflow.
pub fn parse_decimal(chars: &[char]) -> Option<usize> {
    let mut value: usize = 0;
    for &c in chars {
        value = value
            .checked_mul(10)?
            .checked_add(decimal_value(c) as usize)?;
    }
    Some(value)
}

/// CPython `_PyUnicode_ToLowercase`: the first code point of the full
/// lowercase mapping (so U+0130 -> 'i').
pub fn simple_lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// CPython `_PyUnicode_ToUppercase`: first code point of the full uppercase
/// mapping (so 'ß' -> 'S').
pub fn simple_upper(c: char) -> char {
    c.to_uppercase().next().unwrap_or(c)
}

/// `_sre.unicode_iscased`.
pub fn is_cased(c: char) -> bool {
    c != simple_lower(c) || c != simple_upper(c)
}

/// CPython `Lib/re/_casefix.py` `_EXTRA_CASES`: lowercase characters that
/// IGNORECASE also treats as equal (they share an uppercase form).
fn extra_cases(lo: char) -> &'static [char] {
    match lo as u32 {
        0x0069 => &['\u{0131}'],
        0x0073 => &['\u{017f}'],
        0x00b5 => &['\u{03bc}'],
        0x0131 => &['\u{0069}'],
        0x017f => &['\u{0073}'],
        0x0345 => &['\u{03b9}', '\u{1fbe}'],
        0x0390 => &['\u{1fd3}'],
        0x03b0 => &['\u{1fe3}'],
        0x03b2 => &['\u{03d0}'],
        0x03b5 => &['\u{03f5}'],
        0x03b8 => &['\u{03d1}'],
        0x03b9 => &['\u{0345}', '\u{1fbe}'],
        0x03ba => &['\u{03f0}'],
        0x03bc => &['\u{00b5}'],
        0x03c0 => &['\u{03d6}'],
        0x03c1 => &['\u{03f1}'],
        0x03c2 => &['\u{03c3}'],
        0x03c3 => &['\u{03c2}'],
        0x03c6 => &['\u{03d5}'],
        0x03d0 => &['\u{03b2}'],
        0x03d1 => &['\u{03b8}'],
        0x03d5 => &['\u{03c6}'],
        0x03d6 => &['\u{03c0}'],
        0x03f0 => &['\u{03ba}'],
        0x03f1 => &['\u{03c1}'],
        0x03f5 => &['\u{03b5}'],
        0x0432 => &['\u{1c80}'],
        0x0434 => &['\u{1c81}'],
        0x043e => &['\u{1c82}'],
        0x0441 => &['\u{1c83}'],
        0x0442 => &['\u{1c84}', '\u{1c85}'],
        0x044a => &['\u{1c86}'],
        0x0463 => &['\u{1c87}'],
        0x1c80 => &['\u{0432}'],
        0x1c81 => &['\u{0434}'],
        0x1c82 => &['\u{043e}'],
        0x1c83 => &['\u{0441}'],
        0x1c84 => &['\u{0442}', '\u{1c85}'],
        0x1c85 => &['\u{0442}', '\u{1c84}'],
        0x1c86 => &['\u{044a}'],
        0x1c87 => &['\u{0463}'],
        0x1c88 => &['\u{a64b}'],
        0x1e61 => &['\u{1e9b}'],
        0x1e9b => &['\u{1e61}'],
        0x1fbe => &['\u{0345}', '\u{03b9}'],
        0x1fd3 => &['\u{0390}'],
        0x1fe3 => &['\u{03b0}'],
        0xa64b => &['\u{1c88}'],
        0xfb05 => &['\u{fb06}'],
        0xfb06 => &['\u{fb05}'],
        _ => &[],
    }
}

/// Does text char `t` match pattern literal `p` under Python `re.IGNORECASE`?
pub fn ignorecase_eq(p: char, t: char) -> bool {
    if !is_cased(p) {
        return p == t;
    }
    let lo = simple_lower(p);
    let tl = simple_lower(t);
    tl == lo || extra_cases(lo).contains(&tl)
}

const TITLECASE: &[(u32, u32)] = &[
    (0x01c5, 0x01c5),
    (0x01c8, 0x01c8),
    (0x01cb, 0x01cb),
    (0x01f2, 0x01f2),
    (0x1f88, 0x1f8f),
    (0x1f98, 0x1f9f),
    (0x1fa8, 0x1faf),
    (0x1fbc, 0x1fbc),
    (0x1fcc, 0x1fcc),
    (0x1ffc, 0x1ffc),
];

fn is_titlecase(c: char) -> bool {
    let v = c as u32;
    TITLECASE.iter().any(|&(a, b)| (a..=b).contains(&v))
}

// ------------------------------------------------------------ str methods ---

/// Python `str.casefold()` (full case folding).
pub fn casefold(s: &str) -> String {
    caseless::default_case_fold_str(s)
}

/// Python `str.lower()` (full mapping with final-sigma handling).
pub fn lower(s: &str) -> String {
    s.to_lowercase()
}

/// Python `str.isupper()`.
pub fn isupper(s: &str) -> bool {
    let mut cased = false;
    for c in s.chars() {
        if c.is_lowercase() || is_titlecase(c) {
            return false;
        }
        if c.is_uppercase() {
            cased = true;
        }
    }
    cased
}

/// Python `str.strip()` (whitespace).
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Python `str.strip(chars)`.
pub fn strip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_matches(|c| chars.contains(c))
}

/// Python `str.rstrip(chars)`.
pub fn rstrip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_end_matches(|c| chars.contains(c))
}

/// Python `str.split()` without arguments.
pub fn split_ws(s: &str) -> Vec<&str> {
    s.split(is_space).filter(|p| !p.is_empty()).collect()
}

/// `len(text.splitlines())`.
pub fn splitlines_count(s: &str) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut count = 0;
    let mut i = 0;
    let mut line_open = false;
    while i < chars.len() {
        let c = chars[i];
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
        if is_break {
            count += 1;
            line_open = false;
            if c == '\r' && chars.get(i + 1) == Some(&'\n') {
                i += 1;
            }
        } else {
            line_open = true;
        }
        i += 1;
    }
    if line_open {
        count += 1;
    }
    count
}

/// `f"{n:,}"`.
pub fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Python `repr(str)`.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c if c.is_ascii() => out.push(c),
            c if char_matches(&NONPRINTABLE_RE, c) => {
                let v = c as u32;
                if v <= 0xff {
                    out.push_str(&format!("\\x{v:02x}"));
                } else if v <= 0xffff {
                    out.push_str(&format!("\\u{v:04x}"));
                } else {
                    out.push_str(&format!("\\U{v:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python truthiness of a JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python-style name of a JSON value's type, for error messages.
pub fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `pathlib.PurePath.suffix`.
pub fn path_suffix(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => name[i..].to_string(),
        _ => String::new(),
    }
}

/// `path.with_suffix(path.suffix + extra)` == file name + extra for every
/// name Python accepts here.
pub fn sibling_with_extra(path: &Path, extra: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(extra);
    path.with_file_name(name)
}

// ------------------------------------------------------------- json.dumps ---

/// Python `json.dumps(value, indent=indent, ensure_ascii=ensure_ascii)` with
/// default separators. Objects keep insertion order (serde_json
/// preserve_order).
pub fn json_dumps(value: &Value, indent: Option<usize>, ensure_ascii: bool) -> String {
    let mut out = String::new();
    write_json(&mut out, value, indent, ensure_ascii, 0);
    out
}

fn write_json(out: &mut String, value: &Value, indent: Option<usize>, ascii: bool, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else {
                out.push_str(&float_repr(n.as_f64().unwrap_or(0.0)));
            }
        }
        Value::String(s) => write_json_str(out, s, ascii),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if indent.is_none() {
                        out.push(' ');
                    }
                }
                newline_indent(out, indent, level + 1);
                write_json(out, item, indent, ascii, level + 1);
            }
            newline_indent(out, indent, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if indent.is_none() {
                        out.push(' ');
                    }
                }
                newline_indent(out, indent, level + 1);
                write_json_str(out, key, ascii);
                out.push_str(": ");
                write_json(out, item, indent, ascii, level + 1);
            }
            newline_indent(out, indent, level);
            out.push('}');
        }
    }
}

fn newline_indent(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(width) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(width * level));
    }
}

fn write_json_str(out: &mut String, s: &str, ascii: bool) {
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
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python `repr(float)` for finite values.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    let sci = format!("{f:e}"); // shortest round-trip digits, e.g. "-1.5e-5"
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            let mut int_part = digits.chars().take(int_len).collect::<String>();
            while int_part.len() < int_len {
                int_part.push('0');
            }
            let frac: String = digits.chars().skip(int_len).collect();
            format!(
                "{sign}{int_part}.{}",
                if frac.is_empty() { "0" } else { &frac }
            )
        } else {
            format!("{sign}0.{}{digits}", "0".repeat((-exp - 1) as usize))
        }
    } else {
        let mut out = format!("{sign}{}", &digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push_str(&format!(
            "e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        ));
        out
    }
}

// ---------------------------------------------------------- file plumbing ---

/// Holds the cross-process lock on a sidecar `.lock` file (released on
/// drop). Uses `flock(LOCK_EX)` on Unix — the same lock Python's
/// `fcntl.flock` takes — and `LockFileEx` on Windows, which conflicts with
/// Python's `msvcrt.locking(fd, LK_LOCK, 1)` byte-range lock.
pub struct SidecarLock {
    file: Option<File>,
}

impl SidecarLock {
    /// Acquire, or warn and proceed unlocked exactly like the Python stores.
    pub fn acquire(lock_path: &Path, parent: &Path, label: &str) -> SidecarLock {
        let opened = fs::create_dir_all(parent).and_then(|_| {
            OpenOptions::new()
                .read(true)
                .append(true)
                .create(true)
                .open(lock_path)
        });
        let file = match opened {
            Ok(file) => file,
            Err(err) => {
                log::warn!(
                    "{label} lock unavailable, proceeding unlocked: {} ({err})",
                    lock_path.display()
                );
                return SidecarLock { file: None };
            }
        };
        match file.lock() {
            Ok(()) => SidecarLock { file: Some(file) },
            Err(err) => {
                log::warn!(
                    "{label} lock unavailable, proceeding unlocked: {} ({err})",
                    lock_path.display()
                );
                SidecarLock { file: None }
            }
        }
    }

    pub fn is_locked(&self) -> bool {
        self.file.is_some()
    }
}

impl Drop for SidecarLock {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            let _ = file.unlock();
        }
    }
}

/// `(st_mtime_ns, st_size)` change signature.
pub fn file_signature(path: &Path) -> std::io::Result<(i128, u64)> {
    let meta = fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((
            meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128,
            meta.size(),
        ))
    }
    #[cfg(not(unix))]
    {
        let modified = meta.modified()?;
        let ns = match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        };
        Ok((ns, meta.len()))
    }
}

// ------------------------------------------------------------ mini regex ---

/// A small backtracking matcher reproducing Python `re` semantics for the
/// fixed patterns these modules use: leftmost-first alternation, greedy
/// quantifiers with backtracking, lookahead, zero-width assertions (used for
/// Python's single-character lookbehinds, `\b`, `^`, `$`), capture groups and
/// IGNORECASE literals with CPython's extra case equivalences. The Rust
/// `regex` crate has no lookaround and a different `\w`, so it is not used
/// for these patterns.
pub mod re {
    use super::{is_cased, is_space, is_word, simple_lower};
    use std::sync::Arc;

    type CharPred = Arc<dyn Fn(char) -> bool + Send + Sync>;
    type PosPred = Arc<dyn Fn(&[char], usize) -> bool + Send + Sync>;
    pub type Caps = Vec<Option<(usize, usize)>>;

    #[derive(Clone)]
    pub enum Node {
        Char(CharPred),
        /// IGNORECASE literal: matches when lower(text char) is the pattern's
        /// lower form or one of its `_casefix` equivalents.
        Ci(char),
        Seq(Vec<Node>),
        Alt(Vec<Node>),
        /// Greedy repeat {min, max}.
        Repeat(Box<Node>, usize, usize),
        Group(usize, Box<Node>),
        /// Lookahead; `true` = negative.
        Look(Box<Node>, bool),
        Assert(PosPred),
    }

    pub fn pred(f: impl Fn(char) -> bool + Send + Sync + 'static) -> Node {
        Node::Char(Arc::new(f))
    }
    pub fn ch(c: char) -> Node {
        pred(move |t| t == c)
    }
    /// A character class listing literal characters.
    pub fn set(chars: &'static str) -> Node {
        pred(move |t| chars.contains(t))
    }
    pub fn space() -> Node {
        pred(is_space)
    }
    pub fn word() -> Node {
        pred(is_word)
    }
    pub fn lit(s: &str) -> Node {
        Node::Seq(s.chars().map(ch).collect())
    }
    /// `re.escape(s)` compiled with IGNORECASE.
    pub fn lit_ci(s: &str) -> Node {
        Node::Seq(
            s.chars()
                .map(|c| {
                    if is_cased(c) {
                        Node::Ci(simple_lower(c))
                    } else {
                        ch(c)
                    }
                })
                .collect(),
        )
    }
    pub fn seq(nodes: Vec<Node>) -> Node {
        Node::Seq(nodes)
    }
    pub fn alt(nodes: Vec<Node>) -> Node {
        Node::Alt(nodes)
    }
    pub fn star(n: Node) -> Node {
        Node::Repeat(Box::new(n), 0, usize::MAX)
    }
    pub fn plus(n: Node) -> Node {
        Node::Repeat(Box::new(n), 1, usize::MAX)
    }
    pub fn opt(n: Node) -> Node {
        Node::Repeat(Box::new(n), 0, 1)
    }
    pub fn group(i: usize, n: Node) -> Node {
        Node::Group(i, Box::new(n))
    }
    pub fn ahead(n: Node) -> Node {
        Node::Look(Box::new(n), false)
    }
    pub fn not_ahead(n: Node) -> Node {
        Node::Look(Box::new(n), true)
    }
    pub fn assert(f: impl Fn(&[char], usize) -> bool + Send + Sync + 'static) -> Node {
        Node::Assert(Arc::new(f))
    }
    /// `\b` with Python's `\w`.
    pub fn boundary() -> Node {
        assert(|t, p| {
            let before = p > 0 && is_word(t[p - 1]);
            let after = p < t.len() && is_word(t[p]);
            before != after
        })
    }
    /// Single-character lookbehind `(?<!X)`.
    pub fn not_after(f: impl Fn(char) -> bool + Send + Sync + 'static) -> Node {
        assert(move |t, p| p == 0 || !f(t[p - 1]))
    }
    /// `^` without MULTILINE.
    pub fn start() -> Node {
        assert(|_, p| p == 0)
    }
    /// `$` without MULTILINE: end, or before a final newline.
    pub fn dollar() -> Node {
        assert(|t, p| p == t.len() || (p + 1 == t.len() && t[p] == '\n'))
    }

    struct Ctx<'a> {
        text: &'a [char],
        lower: &'a [char],
    }

    /// `lo` is the pattern char's simple lowercase (only cased chars become
    /// `Node::Ci`); CPython compares `lower(text char)` against `lo` and its
    /// `_casefix` equivalents.
    fn ci_at(ctx: &Ctx, lo: char, pos: usize) -> bool {
        let tl = ctx.lower[pos];
        tl == lo || super::extra_cases(lo).contains(&tl)
    }

    /// A single-character test (the only nodes that cannot backtrack).
    #[derive(Clone)]
    enum Single {
        Char(CharPred),
        Ci(char),
    }

    impl Single {
        fn of(node: &Node) -> Option<Single> {
            match node {
                Node::Char(f) => Some(Single::Char(f.clone())),
                Node::Ci(c) => Some(Single::Ci(*c)),
                _ => None,
            }
        }

        fn test(&self, ctx: &Ctx, pos: usize) -> bool {
            pos < ctx.text.len()
                && match self {
                    Single::Char(f) => f(ctx.text[pos]),
                    Single::Ci(lo) => ci_at(ctx, *lo, pos),
                }
        }
    }

    /// Backtracking VM instruction. Patterns are compiled once, then matched
    /// with an explicit heap stack (like CPython's `_sre`), so neither the
    /// pattern length (e.g. a command name with thousands of separators) nor
    /// the text length deepens the Rust call stack. Only lookaround nesting,
    /// which is fixed by the program's own patterns, recurses.
    enum Inst {
        One(Single),
        /// Greedy repeat {min, max} of a single-character test.
        RepeatOne(Single, usize, usize),
        /// Try the first target; on failure resume at the second.
        Split(usize, usize),
        Jmp(usize),
        GroupStart(usize),
        GroupEnd(usize),
        /// Record the position an iteration of a general repeat starts at.
        Mark(usize),
        /// Fail an iteration that matched the empty string (it cannot make
        /// progress), so a repeat of a nullable node terminates.
        Progress(usize),
        Look {
            entry: usize,
            negative: bool,
            next: usize,
        },
        Assert(PosPred),
        Fail,
        Match,
    }

    #[derive(Default)]
    struct Program {
        code: Vec<Inst>,
        marks: usize,
    }

    impl Program {
        fn emit(&mut self, inst: Inst) -> usize {
            self.code.push(inst);
            self.code.len() - 1
        }

        /// Recurses only over node nesting depth (a sequence is flat).
        fn compile(&mut self, node: &Node) {
            match node {
                Node::Char(_) | Node::Ci(_) => {
                    let single = Single::of(node).expect("single-character node");
                    self.emit(Inst::One(single));
                }
                Node::Seq(nodes) => nodes.iter().for_each(|n| self.compile(n)),
                Node::Alt(alts) => self.alternation(alts),
                Node::Repeat(inner, min, max) => self.repeat(inner, *min, *max),
                Node::Group(i, inner) => {
                    self.emit(Inst::GroupStart(*i));
                    self.compile(inner);
                    self.emit(Inst::GroupEnd(*i));
                }
                Node::Look(inner, negative) => {
                    let at = self.emit(Inst::Fail);
                    self.compile(inner);
                    self.emit(Inst::Match);
                    let next = self.code.len();
                    self.code[at] = Inst::Look {
                        entry: at + 1,
                        negative: *negative,
                        next,
                    };
                }
                Node::Assert(f) => {
                    self.emit(Inst::Assert(f.clone()));
                }
            }
        }

        fn alternation(&mut self, alts: &[Node]) {
            let Some((last, rest)) = alts.split_last() else {
                self.emit(Inst::Fail);
                return;
            };
            let mut jumps = Vec::new();
            for alt in rest {
                let split = self.emit(Inst::Fail);
                self.compile(alt);
                jumps.push(self.emit(Inst::Fail));
                self.code[split] = Inst::Split(split + 1, self.code.len());
            }
            self.compile(last);
            let end = self.code.len();
            for jump in jumps {
                self.code[jump] = Inst::Jmp(end);
            }
        }

        fn repeat(&mut self, inner: &Node, min: usize, max: usize) {
            if min > max {
                self.emit(Inst::Fail);
                return;
            }
            if let Some(single) = Single::of(inner) {
                self.emit(Inst::RepeatOne(single, min, max));
                return;
            }
            for _ in 0..min {
                self.iteration(inner);
            }
            if max == usize::MAX {
                let split = self.emit(Inst::Fail);
                self.iteration(inner);
                self.emit(Inst::Jmp(split));
                self.code[split] = Inst::Split(split + 1, self.code.len());
            } else {
                // Optional iterations are unrolled (the patterns here only
                // use `?`, i.e. max - min == 1).
                let mut splits = Vec::new();
                for _ in min..max {
                    splits.push(self.emit(Inst::Fail));
                    self.iteration(inner);
                }
                let exit = self.code.len();
                for split in splits {
                    self.code[split] = Inst::Split(split + 1, exit);
                }
            }
        }

        fn iteration(&mut self, inner: &Node) {
            let mark = self.marks;
            self.marks += 1;
            self.emit(Inst::Mark(mark));
            self.compile(inner);
            self.emit(Inst::Progress(mark));
        }
    }

    /// Choice points and undo records on the backtrack stack.
    enum Frame {
        Resume {
            pc: usize,
            pos: usize,
        },
        /// Greedy single-character repeat that can still give back.
        GiveBack {
            pc: usize,
            base: usize,
            count: usize,
            min: usize,
        },
        Cap(usize, Option<(usize, usize)>),
        Start(usize, usize),
        Mark(usize, usize),
    }

    fn run(
        program: &Program,
        ctx: &Ctx,
        entry: usize,
        start: usize,
        caps: &mut Caps,
    ) -> Option<usize> {
        let mut starts = vec![0usize; caps.len()];
        let mut marks = vec![0usize; program.marks];
        let mut stack: Vec<Frame> = Vec::new();
        let (mut pc, mut pos) = (entry, start);
        loop {
            let ok = match &program.code[pc] {
                Inst::One(single) => {
                    let ok = single.test(ctx, pos);
                    pos += 1;
                    ok
                }
                Inst::RepeatOne(single, min, max) => {
                    let mut n = 0;
                    while n < *max && single.test(ctx, pos + n) {
                        n += 1;
                    }
                    if n > *min {
                        stack.push(Frame::GiveBack {
                            pc: pc + 1,
                            base: pos,
                            count: n,
                            min: *min,
                        });
                    }
                    pos += n;
                    n >= *min
                }
                Inst::Split(first, second) => {
                    stack.push(Frame::Resume { pc: *second, pos });
                    pc = *first;
                    continue;
                }
                Inst::Jmp(target) => {
                    pc = *target;
                    continue;
                }
                Inst::GroupStart(i) => {
                    stack.push(Frame::Start(*i, starts[*i]));
                    starts[*i] = pos;
                    true
                }
                Inst::GroupEnd(i) => {
                    stack.push(Frame::Cap(*i, caps[*i]));
                    caps[*i] = Some((starts[*i], pos));
                    true
                }
                Inst::Mark(r) => {
                    stack.push(Frame::Mark(*r, marks[*r]));
                    marks[*r] = pos;
                    true
                }
                Inst::Progress(r) => pos != marks[*r],
                Inst::Look {
                    entry,
                    negative,
                    next,
                } => {
                    let mut scratch = caps.clone();
                    let found = run(program, ctx, *entry, pos, &mut scratch).is_some();
                    if found != *negative {
                        pc = *next;
                        continue;
                    }
                    false
                }
                Inst::Assert(f) => f(ctx.text, pos),
                Inst::Fail => false,
                Inst::Match => return Some(pos),
            };
            if ok {
                pc += 1;
                continue;
            }
            loop {
                match stack.pop()? {
                    Frame::Cap(i, old) => caps[i] = old,
                    Frame::Start(i, old) => starts[i] = old,
                    Frame::Mark(r, old) => marks[r] = old,
                    Frame::Resume { pc: p, pos: q } => {
                        (pc, pos) = (p, q);
                        break;
                    }
                    Frame::GiveBack {
                        pc: p,
                        base,
                        count,
                        min,
                    } => {
                        let count = count - 1;
                        if count > min {
                            stack.push(Frame::GiveBack {
                                pc: p,
                                base,
                                count,
                                min,
                            });
                        }
                        (pc, pos) = (p, base + count);
                        break;
                    }
                }
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Match {
        pub start: usize,
        pub end: usize,
        /// Index 0 is the whole match.
        pub groups: Caps,
    }

    impl Match {
        pub fn span(&self, i: usize) -> Option<(usize, usize)> {
            self.groups.get(i).copied().flatten()
        }
        pub fn group(&self, text: &[char], i: usize) -> Option<String> {
            self.span(i).map(|(a, b)| text[a..b].iter().collect())
        }
        pub fn text(&self, text: &[char]) -> String {
            text[self.start..self.end].iter().collect()
        }
    }

    pub struct Regex {
        program: Program,
        /// Entry of the pattern followed by `Match` (`re.match`).
        entry: usize,
        /// Entry of the pattern followed by an end-of-text assertion
        /// (`re.fullmatch`, which backtracks until the match ends at len).
        full_entry: usize,
        groups: usize,
    }

    impl Regex {
        /// `groups` is the highest group index used by `node`.
        pub fn new(node: Node, groups: usize) -> Self {
            let mut program = Program::default();
            let entry = program.code.len();
            program.compile(&node);
            program.emit(Inst::Match);
            let full_entry = program.code.len();
            program.compile(&node);
            program.emit(Inst::Assert(Arc::new(|t: &[char], p| p == t.len())));
            program.emit(Inst::Match);
            Regex {
                program,
                entry,
                full_entry,
                groups,
            }
        }

        fn match_ctx(&self, ctx: &Ctx, pos: usize) -> Option<Match> {
            self.run_at(ctx, self.entry, pos)
        }

        fn run_at(&self, ctx: &Ctx, entry: usize, pos: usize) -> Option<Match> {
            let mut groups: Caps = vec![None; self.groups + 1];
            let end = run(&self.program, ctx, entry, pos, &mut groups)?;
            groups[0] = Some((pos, end));
            Some(Match {
                start: pos,
                end,
                groups,
            })
        }

        /// `re.match` at `pos` (anchored at pos, not at end).
        pub fn match_at(&self, text: &[char], pos: usize) -> Option<Match> {
            let lower: Vec<char> = text.iter().map(|&c| simple_lower(c)).collect();
            self.match_ctx(
                &Ctx {
                    text,
                    lower: &lower,
                },
                pos,
            )
        }

        /// `re.fullmatch`.
        pub fn fullmatch(&self, text: &[char]) -> bool {
            let lower: Vec<char> = text.iter().map(|&c| simple_lower(c)).collect();
            self.run_at(
                &Ctx {
                    text,
                    lower: &lower,
                },
                self.full_entry,
                0,
            )
            .is_some()
        }

        /// `re.finditer` (non-overlapping, left to right).
        pub fn find_iter(&self, text: &[char]) -> Vec<Match> {
            let lower: Vec<char> = text.iter().map(|&c| simple_lower(c)).collect();
            let ctx = Ctx {
                text,
                lower: &lower,
            };
            let mut out = Vec::new();
            let mut pos = 0;
            while pos <= text.len() {
                let Some(found) = (pos..=text.len()).find_map(|s| self.match_ctx(&ctx, s)) else {
                    break;
                };
                pos = if found.end > found.start {
                    found.end
                } else {
                    found.end + 1
                };
                out.push(found);
            }
            out
        }

        /// `re.search`.
        pub fn search(&self, text: &[char]) -> Option<Match> {
            let lower: Vec<char> = text.iter().map(|&c| simple_lower(c)).collect();
            let ctx = Ctx {
                text,
                lower: &lower,
            };
            (0..=text.len()).find_map(|start| self.match_ctx(&ctx, start))
        }

        /// `re.sub` with a callable replacement, on a char slice.
        pub fn sub_chars(&self, text: &[char], mut f: impl FnMut(&Match) -> String) -> String {
            let mut out = String::new();
            let mut last = 0;
            for found in self.find_iter(text) {
                out.extend(&text[last..found.start]);
                out.push_str(&f(&found));
                last = found.end;
            }
            out.extend(&text[last..]);
            out
        }

        /// `re.sub` with a callable replacement.
        pub fn sub(&self, text: &str, mut f: impl FnMut(&Match, &[char]) -> String) -> String {
            let chars: Vec<char> = text.chars().collect();
            self.sub_chars(&chars, |found| f(found, &chars))
        }
    }

    /// `[^\W_]`
    pub fn alnum() -> Node {
        pred(super::is_alnum)
    }
    /// `[\W_]`
    pub fn non_alnum() -> Node {
        pred(|c| !super::is_alnum(c))
    }
    /// `\d`
    pub fn digit() -> Node {
        pred(super::is_decimal)
    }
}

/// Run `f` on `text` as chars and collect.
pub fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

#[cfg(test)]
mod tests {
    use super::re::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn word_and_space_classes_follow_python() {
        assert!(is_word('é') && is_word('_') && is_word('٣') && is_word('Ⅷ'));
        assert!(!is_word('\u{0301}')); // combining acute is not \w in Python
        assert!(!is_word('\u{093e}')); // Devanagari vowel sign (Other_Alphabetic)
        assert!(!is_word('-') && !is_word(' '));
        assert!(is_space('\u{1c}') && is_space('\u{a0}') && is_space('\u{2028}'));
        assert!(!is_space('\u{200b}') && !is_space('x'));
    }

    #[test]
    fn ignorecase_matches_cpython_fixes() {
        assert!(ignorecase_eq('i', 'I') && ignorecase_eq('i', 'ı') && ignorecase_eq('i', 'İ'));
        assert!(ignorecase_eq('I', 'ı'));
        assert!(ignorecase_eq('s', 'ſ') && ignorecase_eq('k', '\u{212a}'));
        assert!(ignorecase_eq('σ', 'ς') && ignorecase_eq('µ', 'μ'));
        assert!(!ignorecase_eq('a', 'b') && !ignorecase_eq('-', '_'));
        assert!(ignorecase_eq('ß', 'ẞ'));
    }

    #[test]
    fn str_methods() {
        assert_eq!(strip("\u{1c} a b \n"), "a b");
        assert_eq!(split_ws(" a\tb\u{a0}c  "), vec!["a", "b", "c"]);
        assert!(split_ws("   ").is_empty());
        assert!(isupper("NASA") && isupper("A1") && !isupper("Nasa") && !isupper("123"));
        assert!(!isupper("ǅ"));
        assert_eq!(casefold("Straße"), "strasse");
        assert_eq!(lower("ΑΣ"), "ας");
        assert_eq!(splitlines_count(""), 0);
        assert_eq!(splitlines_count("a"), 1);
        assert_eq!(splitlines_count("a\n"), 1);
        assert_eq!(splitlines_count("a\r\nb\rc"), 3);
        assert_eq!(splitlines_count("\n\n"), 2);
        assert_eq!(thousands(20000), "20,000");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(repr_str("abc"), "'abc'");
        assert_eq!(repr_str("it's"), "\"it's\"");
        assert_eq!(repr_str("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(repr_str("x\ny\\"), "'x\\ny\\\\'");
        assert_eq!(repr_str("café\u{a0}\u{200b}"), "'café\\xa0\\u200b'");
        assert_eq!(repr_str("\u{7f}"), "'\\x7f'");
    }

    #[test]
    fn json_dumps_matches_python_layout() {
        let v = json!({"words": ["Ada", "é"], "replacements": [], "n": {"x": 1, "f": 1.5}});
        assert_eq!(
            json_dumps(&v, Some(2), false),
            "{\n  \"words\": [\n    \"Ada\",\n    \"é\"\n  ],\n  \"replacements\": [],\n  \"n\": {\n    \"x\": 1,\n    \"f\": 1.5\n  }\n}"
        );
        assert_eq!(
            json_dumps(&json!(["é😀\u{7f}"]), None, true),
            "[\"\\u00e9\\ud83d\\ude00\\u007f\"]"
        );
        assert_eq!(
            json_dumps(&json!([1, "a\u{1}"]), None, false),
            "[1, \"a\\u0001\"]"
        );
        assert_eq!(json_dumps(&json!({}), Some(2), true), "{}");
    }

    #[test]
    fn float_repr_matches_python() {
        assert_eq!(float_repr(1.0), "1.0");
        assert_eq!(float_repr(4.0), "4.0");
        assert_eq!(float_repr(0.1), "0.1");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1e15), "1000000000000000.0");
        assert_eq!(float_repr(1.5e-5), "1.5e-05");
        assert_eq!(float_repr(0.0001), "0.0001");
        assert_eq!(float_repr(-2.5), "-2.5");
        assert_eq!(float_repr(123.456), "123.456");
    }

    #[test]
    fn engine_backtracks_like_python() {
        // (?:ab|a)(?!b) on "abb": first alt fails lookahead, second alt too.
        let re = Regex::new(
            seq(vec![alt(vec![lit("ab"), lit("a")]), not_ahead(ch('c'))]),
            0,
        );
        let text = chars("abc");
        let found = re.match_at(&text, 0).unwrap();
        assert_eq!((found.start, found.end), (0, 1));
        // greedy star gives back
        let re = Regex::new(seq(vec![star(set("a")), lit("ab")]), 0);
        assert_eq!(re.match_at(&chars("aaab"), 0).unwrap().end, 4);
        // finditer non-overlapping
        let re = Regex::new(lit_ci("aa"), 0);
        assert_eq!(re.find_iter(&chars("AaAaA")).len(), 2);
        // empty-match guard terminates
        let re = Regex::new(star(ch('x')), 0);
        assert_eq!(re.find_iter(&chars("ab")).len(), 3);
        assert!(re.search(&chars("")).is_some());
        // groups
        let re = Regex::new(seq(vec![group(1, plus(word())), dollar()]), 1);
        let text = chars("hi there\n");
        let found = re.search(&text).unwrap();
        assert_eq!(found.group(&text, 1).as_deref(), Some("there"));
        assert!(Regex::new(lit_ci("i"), 0).fullmatch(&chars("İ")));
        assert!(!Regex::new(lit_ci("i"), 0).fullmatch(&chars("ii")));
        // fullmatch backtracks to a longer alternative ending at the end
        let re = Regex::new(alt(vec![lit("a"), lit("ab")]), 0);
        assert!(re.fullmatch(&chars("ab")));
        assert_eq!(re.match_at(&chars("ab"), 0).unwrap().end, 1);
        // general repeats: greedy, give back, empty iterations stop
        let re = Regex::new(
            seq(vec![
                star(group(1, alt(vec![lit("ab"), lit("a")]))),
                lit("b"),
            ]),
            1,
        );
        let text = chars("ababab");
        let found = re.match_at(&text, 0).unwrap();
        assert_eq!((found.end, found.span(1)), (6, Some((4, 5))));
        let re = Regex::new(seq(vec![star(opt(ch('x'))), lit("y")]), 0);
        assert_eq!(re.match_at(&chars("xxy"), 0).unwrap().end, 3);
        assert!(re.match_at(&chars("xxz"), 0).is_none());
        let re = Regex::new(
            seq(vec![plus(seq(vec![ch('a'), opt(ch('b'))])), dollar()]),
            0,
        );
        assert!(re.fullmatch(&chars("aabab")));
        // a failed alternative restores the group it set
        let re = Regex::new(
            alt(vec![
                seq(vec![group(1, ch('a')), ch('z')]),
                seq(vec![ch('a'), opt(group(2, ch('b')))]),
            ]),
            2,
        );
        let found = re.match_at(&chars("ab"), 0).unwrap();
        assert_eq!((found.span(1), found.span(2)), (None, Some((1, 2))));
        // empty alternation never matches
        assert!(Regex::new(alt(vec![]), 0).search(&chars("abc")).is_none());
    }

    /// Long patterns and long inputs run on the heap stack, not recursion.
    #[test]
    fn engine_does_not_recurse_per_pattern_node_or_char() {
        let parts = 100_000;
        let mut nodes = Vec::new();
        for i in 0..parts {
            if i > 0 {
                nodes.push(alt(vec![
                    seq(vec![star(space()), set("-_"), star(space())]),
                    plus(space()),
                ]));
            }
            nodes.push(group(1, lit_ci("ab")));
        }
        let re = Regex::new(seq(nodes), 1);
        let text = chars(&vec!["AB"; parts].join(" - "));
        assert!(re.fullmatch(&text));
        let long = chars(&"a".repeat(200_000));
        let re = Regex::new(star(seq(vec![ch('a'), opt(ch('b'))])), 0);
        assert_eq!(re.match_at(&long, 0).unwrap().end, long.len());
    }

    #[test]
    fn decimal_values() {
        assert_eq!(decimal_value('7'), 7);
        assert_eq!(decimal_value('٣'), 3);
        assert_eq!(decimal_value('\u{1d7d0}'), 2); // MATHEMATICAL BOLD DIGIT TWO
        assert_eq!(decimal_value('\u{1d7d8}'), 0); // DOUBLE-STRUCK ZERO (2nd block)
        assert_eq!(parse_decimal(&chars("12")), Some(12));
    }

    #[test]
    fn path_helpers() {
        assert_eq!(path_suffix(Path::new("a/b.CSV")), ".CSV");
        assert_eq!(path_suffix(Path::new(".csv")), "");
        assert_eq!(path_suffix(Path::new("noext")), "");
        assert_eq!(
            sibling_with_extra(Path::new("/x/dictionary.json"), ".lock"),
            PathBuf::from("/x/dictionary.json.lock")
        );
    }

    #[test]
    fn sidecar_lock_creates_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("sub").join("f.json.lock");
        let lock = SidecarLock::acquire(&lock_path, lock_path.parent().unwrap(), "test");
        assert!(lock.is_locked());
        assert!(lock_path.exists());
        drop(lock);
        let again = SidecarLock::acquire(&lock_path, lock_path.parent().unwrap(), "test");
        assert!(again.is_locked());
    }

    #[test]
    fn truthiness() {
        assert!(!truthy(&json!(null)) && !truthy(&json!(0)) && !truthy(&json!("")));
        assert!(!truthy(&json!([])) && !truthy(&json!({})) && !truthy(&json!(0.0)));
        assert!(truthy(&json!(1)) && truthy(&json!("x")) && truthy(&json!(true)));
    }
}
