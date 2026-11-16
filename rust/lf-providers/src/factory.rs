//! Port of the LLM half of `localflow/providers/factory.py`.

use crate::activation;
use crate::base::LlmProvider;
use crate::ollama::OllamaLlm;
use crate::openai::OpenAiCompatLlm;
use crate::registry::UnknownProvider;
use crate::store::{LlmConfig, SecretStore};

pub const FIREWORKS_BASE_URL: &str = "https://api.fireworks.ai/inference/v1";
pub const WORKPROMPTS_TEMPERATURE: f64 = 0.3;

/// The key this config's OpenAI-compatible client may send.
///
/// A config activated on Models & keys names an endpoint-bound snapshot
/// (`llm_key_ref`) and gets that key only for that endpoint, never a
/// fallback; a legacy config without one uses the generic `llm_api_key`.
pub fn llm_api_key<C: LlmConfig>(config: &C, secrets: &dyn SecretStore) -> String {
    let reference = config.get_str("llm_key_ref");
    if !reference.is_empty() {
        return activation::resolve(&reference, &config.get_str("llm_base_url"), |name| {
            secrets.get(name)
        });
    }
    secrets.get("llm_api_key")
}

#[derive(Debug, Clone, PartialEq)]
pub struct BuildOptions {
    /// `None` (or 0, which Python treats as falsy) uses `cleanup_timeout`.
    pub timeout: Option<i64>,
    /// `None` (or 0) uses `cleanup_num_ctx`.
    pub num_ctx: Option<i64>,
    pub temperature: Option<f64>,
    pub with_api_key: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            timeout: None,
            num_ctx: None,
            temperature: None,
            with_api_key: true,
        }
    }
}

impl BuildOptions {
    pub fn with_timeout(timeout: i64) -> Self {
        BuildOptions {
            timeout: Some(timeout),
            ..Self::default()
        }
    }
}

fn or_config<C: LlmConfig>(value: Option<i64>, config: &C, field: &str, default: i64) -> i64 {
    match value {
        Some(v) if v != 0 => v,
        _ => config.get_int(field).unwrap_or(default),
    }
}

/// Build the configured provider (`build_llm`).
pub fn build_llm<C: LlmConfig>(
    config: &C,
    options: &BuildOptions,
    secrets: &dyn SecretStore,
) -> Result<Box<dyn LlmProvider>, UnknownProvider> {
    let timeout = or_config(options.timeout, config, "cleanup_timeout", 120);
    let num_ctx = or_config(options.num_ctx, config, "cleanup_num_ctx", 8192);
    let base_url = config.get_str("llm_base_url");
    let model = config.get_str("llm_model");
    match config.get_str("llm_provider").as_str() {
        "ollama" => Ok(Box::new(OllamaLlm::new(
            &base_url, &model, timeout, num_ctx,
        ))),
        "openai-compatible" => {
            let key = if options.with_api_key {
                llm_api_key(config, secrets)
            } else {
                String::new()
            };
            Ok(Box::new(OpenAiCompatLlm::new(
                &base_url,
                &model,
                &key,
                timeout,
                options.temperature,
            )))
        }
        other => Err(UnknownProvider {
            id: other.to_string(),
        }),
    }
}

/// `build_workprompts_llm`: Fireworks with `fireworks_api_key`, or None.
pub fn build_workprompts_llm<C: LlmConfig>(
    config: &C,
    secrets: &dyn SecretStore,
) -> Option<OpenAiCompatLlm> {
    let key = secrets.get("fireworks_api_key");
    if key.is_empty() {
        return None;
    }
    Some(OpenAiCompatLlm::new(
        FIREWORKS_BASE_URL,
        &config.get_str("fireworks_model"),
        &key,
        crate::openai::DEFAULT_TIMEOUT,
        Some(WORKPROMPTS_TEMPERATURE),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{config_with, default_config, MemSecrets, MockResponse, MockServer};

    const REF: &str = "llm_activation_groq_0123456789abcdef0123456789abcdef";

    #[test]
    fn legacy_and_bound_keys() {
        let secrets = MemSecrets::with(&[
            ("llm_api_key", "GENERIC"),
            (
                REF,
                &activation::encode("https://api.groq.com/openai/v1", "BOUND"),
            ),
        ]);
        let legacy = config_with(&[
            ("llm_provider", "openai-compatible"),
            ("llm_base_url", "https://x/v1"),
        ]);
        assert_eq!(llm_api_key(&legacy, &secrets), "GENERIC");

        let bound = config_with(&[
            ("llm_provider", "openai-compatible"),
            ("llm_base_url", "https://api.groq.com/openai/v1/"),
            ("llm_key_ref", REF),
        ]);
        assert_eq!(llm_api_key(&bound, &secrets), "BOUND");

        let mut moved = bound.clone();
        moved.set_str("llm_base_url", "https://evil.example/v1");
        assert_eq!(
            llm_api_key(&moved, &secrets),
            "",
            "never the snapshot nor the generic key"
        );

        let mut invalid = bound.clone();
        invalid.set_str("llm_key_ref", "llm_activation_invalid");
        assert_eq!(
            llm_api_key(&invalid, &secrets),
            "",
            "invalid ref fails closed"
        );

        let missing = MemSecrets::with(&[("llm_api_key", "GENERIC")]);
        assert_eq!(llm_api_key(&bound, &missing), "");
    }

    #[test]
    fn build_picks_client_and_options() {
        let secrets = MemSecrets::default();
        let ollama = build_llm(&default_config(), &BuildOptions::default(), &secrets).unwrap();
        assert_eq!(ollama.id(), "ollama");

        let cfg = config_with(&[
            ("llm_provider", "openai-compatible"),
            ("llm_base_url", "https://x"),
        ]);
        let client = build_llm(&cfg, &BuildOptions::with_timeout(10), &secrets).unwrap();
        assert_eq!(client.id(), "openai-compatible");

        let err = build_llm(
            &config_with(&[("llm_provider", "nope")]),
            &BuildOptions::default(),
            &secrets,
        )
        .err()
        .unwrap();
        assert_eq!(err.id, "nope");
        assert_eq!(
            or_config(Some(0), &default_config(), "cleanup_timeout", 1),
            120
        );
        assert_eq!(
            or_config(Some(7), &default_config(), "cleanup_timeout", 1),
            7
        );
        assert_eq!(
            or_config(None, &TestMap::new(), "cleanup_timeout", 120),
            120
        );
    }

    type TestMap = std::collections::BTreeMap<String, serde_json::Value>;

    #[test]
    fn built_clients_send_the_bound_key_only_with_api_key() {
        let server = MockServer::start(vec![
            MockResponse::json(200, r#"{"data":[]}"#),
            MockResponse::json(200, r#"{"data":[]}"#),
        ]);
        let url = format!("{}/v1", server.url());
        let secrets = MemSecrets::with(&[
            (REF, &activation::encode(&url, "BOUND")),
            ("llm_api_key", "GENERIC"),
        ]);
        let cfg = config_with(&[
            ("llm_provider", "openai-compatible"),
            ("llm_base_url", &url),
            ("llm_key_ref", REF),
        ]);
        build_llm(&cfg, &BuildOptions::with_timeout(5), &secrets)
            .unwrap()
            .list_models()
            .unwrap();
        let options = BuildOptions {
            with_api_key: false,
            ..BuildOptions::with_timeout(5)
        };
        build_llm(&cfg, &options, &secrets)
            .unwrap()
            .list_models()
            .unwrap();
        assert_eq!(
            server.request(0).header("authorization").as_deref(),
            Some("Bearer BOUND")
        );
        assert!(server.request(1).header("authorization").is_none());
    }

    #[test]
    fn ollama_uses_config_timeout_and_ctx() {
        let server = MockServer::start(vec![MockResponse::json(200, r#"{"response":"x"}"#)]);
        let mut cfg = config_with(&[("llm_base_url", &server.url())]);
        cfg.insert("cleanup_num_ctx".into(), serde_json::json!(2048));
        build_llm(&cfg, &BuildOptions::default(), &MemSecrets::default())
            .unwrap()
            .complete("s", "u", None)
            .unwrap();
        assert_eq!(server.request(0).json()["options"]["num_ctx"], 2048);
        assert_eq!(server.request(0).json()["model"], "llama3.2:3b");
    }

    #[test]
    fn workprompts_needs_fireworks_key() {
        assert!(build_workprompts_llm(&default_config(), &MemSecrets::default()).is_none());
        let client = build_workprompts_llm(
            &default_config(),
            &MemSecrets::with(&[("fireworks_api_key", "fw")]),
        )
        .unwrap();
        assert_eq!(client.base_url, FIREWORKS_BASE_URL);
        assert_eq!(client.model, "accounts/fireworks/models/kimi-k3");
        assert_eq!(client.temperature, Some(0.3));
        assert_eq!(client.timeout, 120);
        assert!(client.has_api_key());
    }
}
