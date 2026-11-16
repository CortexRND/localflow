//! Port of the LLM half of `localflow/providers/registry.py`.
//!
//! Python also discovers third-party providers through the
//! `localflow.llm_providers` entry-point group; the Rust port has only the
//! built-ins (documented parity gap).

use crate::pyfmt::py_repr;
use std::fmt;

/// Built-in LLM provider ids and whether each is offline, sorted like
/// Python `_valid_ids` (no entry points installed).
pub const LLM_BUILTINS: [(&str, bool); 2] = [("ollama", true), ("openai-compatible", false)];

pub fn list_llm_ids() -> Vec<&'static str> {
    LLM_BUILTINS.iter().map(|(id, _)| *id).collect()
}

/// The provider class's `offline` flag, or None for an unknown id.
pub fn llm_provider_offline(id: &str) -> Option<bool> {
    LLM_BUILTINS
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, offline)| *offline)
}

/// Python raises `KeyError(f"unknown LLM provider {id!r}; valid ids: ...")`;
/// `Display` is `str()` of that KeyError (the message's repr).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownProvider {
    pub id: String,
}

impl UnknownProvider {
    pub fn message(&self) -> String {
        format!(
            "unknown LLM provider {}; valid ids: {}",
            py_repr(&self.id),
            list_llm_ids().join(", ")
        )
    }
}

impl fmt::Display for UnknownProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&py_repr(&self.message()))
    }
}

impl std::error::Error for UnknownProvider {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins() {
        assert_eq!(list_llm_ids(), vec!["ollama", "openai-compatible"]);
        assert_eq!(llm_provider_offline("ollama"), Some(true));
        assert_eq!(llm_provider_offline("openai-compatible"), Some(false));
        assert_eq!(llm_provider_offline("x"), None);
        assert_eq!(
            UnknownProvider { id: "x".into() }.to_string(),
            "\"unknown LLM provider 'x'; valid ids: ollama, openai-compatible\""
        );
    }
}
