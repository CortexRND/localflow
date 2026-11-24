//! Port of `localflow/providers/base.py` (LLM side).

use serde::Serialize;
use std::fmt;

/// Raised when an inference provider cannot complete a request.
/// `Display` is the Python `str(ProviderError(...))` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError(pub String);

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProviderError {}

/// `dataclasses.asdict(ModelInfo)` serializes as `{"id", "label", "installed"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
    pub installed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Health {
    pub ok: bool,
    pub detail: String,
}

impl Health {
    pub fn ok() -> Self {
        Health {
            ok: true,
            detail: String::new(),
        }
    }

    pub fn failed(detail: impl Into<String>) -> Self {
        Health {
            ok: false,
            detail: detail.into(),
        }
    }
}

/// The `LLMProvider` protocol.
pub trait LlmProvider {
    fn id(&self) -> &'static str;
    fn offline(&self) -> bool;
    fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<i64>,
    ) -> Result<String, ProviderError>;
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
    fn healthcheck(&self) -> Health;

    /// The `max_tokens` to pass to [`complete`](Self::complete) when a caller
    /// wants about `visible` tokens of reply text. Opt-in: callers that ask
    /// use it; `complete` itself never changes an explicit bound. The default
    /// is `visible`; a model whose bound also counts hidden reasoning
    /// returns a larger, still bounded allowance.
    fn reply_budget(&self, visible: i64) -> i64 {
        visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_info_serializes_like_asdict() {
        let info = ModelInfo {
            id: "m".into(),
            label: "m".into(),
            installed: None,
        };
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"id":"m","label":"m","installed":null}"#
        );
        assert_eq!(ProviderError("boom".into()).to_string(), "boom");
        assert!(Health::ok().ok);
        assert_eq!(Health::failed("x").detail, "x");
    }
}
