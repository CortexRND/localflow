//! Python `str` whitespace semantics for the ported UI code.
//!
//! Rust's `str::trim` / `split_whitespace` use the Unicode `White_Space`
//! property, which lacks U+001C..U+001F; Python's `str.strip()` / `str.split()`
//! treat those four separators as whitespace too. The porting code uses these
//! methods wherever the Python source calls `.strip()` / `.split()`.

use lf_providers::pyfmt::{py_isspace, py_strip};

/// Python-flavoured string helpers.
pub trait PyStr {
    /// `str.strip()` with no argument.
    fn py_strip(&self) -> &str;
    /// `str.split()` with no argument: runs of whitespace separate items and
    /// no empty items are produced.
    fn py_split(&self) -> Vec<&str>;
}

impl PyStr for str {
    fn py_strip(&self) -> &str {
        py_strip(self)
    }

    fn py_split(&self) -> Vec<&str> {
        self.split(py_isspace)
            .filter(|part| !part.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_includes_the_information_separators() {
        assert_eq!("\u{1c}\u{1d} a \u{1e}\u{1f}".py_strip(), "a");
        assert_eq!("\u{a0}\u{2003}x\u{85}".py_strip(), "x");
        // Rust's own trim keeps U+001C..U+001F, which is the difference.
        assert_eq!("\u{1c}a".trim(), "\u{1c}a");
        assert_eq!("".py_strip(), "");
        assert_eq!("\u{1f}".py_strip(), "");
    }

    #[test]
    fn split_matches_python_split() {
        assert_eq!(" a\u{1c}b  c\t\n".py_split(), vec!["a", "b", "c"]);
        assert!("  ".py_split().is_empty());
        assert_eq!("日本\u{3000}語".py_split(), vec!["日本", "語"]);
    }
}
