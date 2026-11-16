//! Port of the LLM / secret helpers in `localflow/api.py`.

use crate::activation;
use crate::base::ModelInfo;
use crate::factory::{build_llm, llm_api_key, BuildOptions};
use crate::keys::provider_secret_names;
use crate::pyfmt::{py_repr, py_strip};
use crate::registry;
use crate::store::{LlmConfig, SecretStore};
use serde::Serialize;
use std::fmt;

/// `api.SECRET_NAMES`.
pub fn secret_names() -> Vec<String> {
    let mut names = vec!["llm_api_key".to_string(), "fireworks_api_key".to_string()];
    names.extend(provider_secret_names());
    names
}

/// Python `KeyError(name)` for a secret outside `SECRET_NAMES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSecret(pub String);

impl fmt::Display for UnknownSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&py_repr(&self.0))
    }
}

impl std::error::Error for UnknownSecret {}

/// The active config names an LLM key outside the activation namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidKeyRef;

impl fmt::Display for InvalidKeyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "The active LLM key reference is invalid; activate the provider again in Models & keys",
        )
    }
}

impl std::error::Error for InvalidKeyRef {}

/// The activation slot `llm_api_key` means for `config`, or "" (legacy).
/// A bad ref is refused before anything is written.
fn bound_ref<C: LlmConfig>(name: &str, config: Option<&C>) -> Result<String, InvalidKeyRef> {
    let Some(config) = config else {
        return Ok(String::new());
    };
    if name != "llm_api_key" {
        return Ok(String::new());
    }
    let reference = config.get_str("llm_key_ref");
    if reference.is_empty() {
        return Ok(String::new());
    }
    if !activation::is_ref(&reference) {
        return Err(InvalidKeyRef);
    }
    Ok(reference)
}

/// Which secrets are set, in `SECRET_NAMES` order. With `config`,
/// `llm_api_key` reports the key its LLM client really sends.
pub fn secrets_status<C: LlmConfig>(
    secrets: &dyn SecretStore,
    config: Option<&C>,
) -> Vec<(String, bool)> {
    let mut status: Vec<(String, bool)> = secret_names()
        .into_iter()
        .map(|name| {
            let set = !secrets.get(&name).is_empty();
            (name, set)
        })
        .collect();
    if let Some(config) = config {
        if !config.get_str("llm_key_ref").is_empty() {
            status[0].1 = !llm_api_key(config, secrets).is_empty();
        }
    }
    status
}

/// Set (or clear, with "") a secret. With an activated `config`,
/// `llm_api_key` rotates that config's own endpoint-bound slot in place;
/// clearing deletes the slot (never the index), so the client sends no key
/// rather than falling back to the generic one.
pub fn update_secret<C: LlmConfig>(
    secrets: &dyn SecretStore,
    name: &str,
    value: &str,
    config: Option<&C>,
) -> anyhow::Result<()> {
    if !secret_names().iter().any(|known| known == name) {
        return Err(UnknownSecret(name.to_string()).into());
    }
    let reference = bound_ref(name, config)?;
    let target = if reference.is_empty() {
        name.to_string()
    } else {
        reference.clone()
    };
    if !value.is_empty() {
        let stored = match config {
            Some(config) if !reference.is_empty() => {
                // A slot recreated after Remove must stay revocable by name:
                // list it first, so a failed write leaves at most an entry.
                let mut index = activation::read_index(|n| secrets.get(n));
                if !index.contains(&reference) {
                    index.push(reference.clone());
                    secrets.set(
                        activation::INDEX_NAME,
                        &activation::encode_index(index.iter().map(String::as_str)),
                    )?;
                }
                activation::encode(&config.get_str("llm_base_url"), value)
            }
            _ => value.to_string(),
        };
        secrets.set(&target, &stored)
    } else {
        match secrets.delete(&target) {
            Ok(()) => Ok(()),
            Err(err) if !secrets.get(&target).is_empty() => Err(err),
            Err(_) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LlmProviderInfo {
    pub id: String,
    pub offline: bool,
}

pub fn llm_providers() -> Vec<LlmProviderInfo> {
    registry::LLM_BUILTINS
        .iter()
        .map(|(id, offline)| LlmProviderInfo {
            id: id.to_string(),
            offline: *offline,
        })
        .collect()
}

pub const UNSAVED_URL_HINT: &str =
    " (the stored API key is only sent to the saved base URL; save it first)";

/// The stored key may only go to the URL the user saved.
fn is_saved_url<C: LlmConfig>(config: &C, base_url: Option<&str>) -> bool {
    match base_url {
        None | Some("") => true,
        Some(url) => {
            py_strip(url).trim_end_matches('/')
                == py_strip(&config.get_str("llm_base_url")).trim_end_matches('/')
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

fn with_endpoint<C: LlmConfig>(
    config: &C,
    provider: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
) -> C {
    let mut selected = config.clone();
    if let Some(provider) = non_empty(provider) {
        selected.set_str("llm_provider", provider);
    }
    if let Some(url) = non_empty(base_url) {
        selected.set_str("llm_base_url", url);
    }
    if let Some(model) = non_empty(model) {
        selected.set_str("llm_model", model);
    }
    selected
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelsResult {
    pub models: Vec<ModelInfo>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub detail: String,
}

const PROBE_TIMEOUT: i64 = 10;

/// `api.models(config, "llm", provider, base_url)`.
pub fn models_llm<C: LlmConfig>(
    config: &C,
    provider: &str,
    base_url: Option<&str>,
    secrets: &dyn SecretStore,
) -> ModelsResult {
    let with_key = is_saved_url(config, base_url);
    let mut selected = with_endpoint(config, None, base_url, None);
    selected.set_str("llm_provider", provider);
    let options = BuildOptions {
        with_api_key: with_key,
        ..BuildOptions::with_timeout(PROBE_TIMEOUT)
    };
    let result = build_llm(&selected, &options, secrets)
        .map_err(|e| e.to_string())
        .and_then(|llm| llm.list_models().map_err(|e| e.to_string()));
    match result {
        Ok(models) => ModelsResult {
            models,
            error: None,
        },
        Err(exc) => {
            let hint = if with_key { "" } else { UNSAVED_URL_HINT };
            ModelsResult {
                models: Vec::new(),
                error: Some(format!("{exc}{hint}")),
            }
        }
    }
}

/// `api.test_llm` (used by `lf llm test`).
pub fn test_llm<C: LlmConfig>(
    config: &C,
    provider: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
    secrets: &dyn SecretStore,
) -> TestResult {
    let with_key = is_saved_url(config, base_url);
    let hint = if with_key { "" } else { UNSAVED_URL_HINT };
    let selected = with_endpoint(config, provider, base_url, model);
    let options = BuildOptions {
        with_api_key: with_key,
        ..BuildOptions::with_timeout(PROBE_TIMEOUT)
    };
    match build_llm(&selected, &options, secrets) {
        Ok(llm) => {
            let health = llm.healthcheck();
            let detail = if health.ok {
                health.detail
            } else {
                format!("{}{hint}", health.detail)
            };
            TestResult {
                ok: health.ok,
                detail,
            }
        }
        Err(exc) => TestResult {
            ok: false,
            detail: format!("{exc}{hint}"),
        },
    }
}

/// `lf llm list`: the selected (or configured) provider at `base_url` (or
/// the configured one) with timeout 10. Like Python this always attaches the
/// config's key: a bound snapshot only for its own endpoint, but a legacy
/// generic `llm_api_key` goes to whatever `--base-url` is given.
pub fn cli_list_models<C: LlmConfig>(
    config: &C,
    provider: Option<&str>,
    base_url: Option<&str>,
    secrets: &dyn SecretStore,
) -> Result<Vec<ModelInfo>, String> {
    let selected = with_endpoint(config, provider, base_url, None);
    build_llm(
        &selected,
        &BuildOptions::with_timeout(PROBE_TIMEOUT),
        secrets,
    )
    .map_err(|e| e.to_string())?
    .list_models()
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        config_with, default_config, MemSecrets, MockResponse, MockServer, TestConfig,
    };

    const REF: &str = "llm_activation_groq_0123456789abcdef0123456789abcdef";
    const GROQ: &str = "https://api.groq.com/openai/v1";

    fn bound() -> TestConfig {
        config_with(&[
            ("llm_provider", "openai-compatible"),
            ("llm_base_url", GROQ),
            ("llm_key_ref", REF),
        ])
    }

    #[test]
    fn names_and_providers() {
        assert_eq!(
            secret_names()[..3],
            ["llm_api_key", "fireworks_api_key", "provider_key_fireworks"]
        );
        assert_eq!(secret_names().len(), 8);
        assert_eq!(
            serde_json::to_string(&llm_providers()).unwrap(),
            r#"[{"id":"ollama","offline":true},{"id":"openai-compatible","offline":false}]"#
        );
    }

    #[test]
    fn status_reports_the_active_binding() {
        let secrets = MemSecrets::with(&[("llm_api_key", "GENERIC"), ("provider_key_groq", "g")]);
        let status = secrets_status::<TestConfig>(&secrets, None);
        assert_eq!(status[0], ("llm_api_key".to_string(), true));
        assert!(status.contains(&("provider_key_groq".to_string(), true)));
        assert!(status.contains(&("fireworks_api_key".to_string(), false)));
        assert!(
            !secrets_status(&secrets, Some(&bound()))[0].1,
            "unused generic key is not reported"
        );
        secrets
            .map
            .borrow_mut()
            .insert(REF.into(), activation::encode(GROQ, "B"));
        assert!(secrets_status(&secrets, Some(&bound()))[0].1);
        assert!(secrets_status(&secrets, Some(&default_config()))[0].1);
    }

    #[test]
    fn rotation_rewrites_only_the_bound_slot() {
        let secrets = MemSecrets::with(&[
            ("llm_api_key", "GENERIC"),
            (REF, &activation::encode(GROQ, "OLD")),
        ]);
        update_secret(&secrets, "llm_api_key", "NEW", Some(&bound())).unwrap();
        assert_eq!(secrets.get(REF), activation::encode(GROQ, "NEW"));
        assert_eq!(
            secrets.get("llm_api_key"),
            "GENERIC",
            "legacy key untouched"
        );
        assert_eq!(
            activation::read_index(|n| secrets.get(n)),
            vec![REF.to_string()],
            "slot indexed"
        );
        assert_eq!(llm_api_key(&bound(), &secrets), "NEW");
        // Legacy config: the generic key itself.
        update_secret(&secrets, "llm_api_key", "G2", Some(&default_config())).unwrap();
        assert_eq!(secrets.get("llm_api_key"), "G2");
        update_secret::<TestConfig>(&secrets, "fireworks_api_key", "fw", None).unwrap();
        assert_eq!(secrets.get("fireworks_api_key"), "fw");
    }

    #[test]
    fn clear_deletes_the_slot_without_fallback() {
        let secrets = MemSecrets::with(&[
            ("llm_api_key", "GENERIC"),
            (REF, &activation::encode(GROQ, "OLD")),
        ]);
        secrets.strict_delete.set(true);
        update_secret(&secrets, "llm_api_key", "", Some(&bound())).unwrap();
        assert_eq!(secrets.get(REF), "");
        assert_eq!(
            llm_api_key(&bound(), &secrets),
            "",
            "no fallback to GENERIC"
        );
        // Clearing something already missing is fine; a delete failure while it is still set is not.
        update_secret(&secrets, "llm_api_key", "", Some(&bound())).unwrap();
        secrets.on_delete(|_| anyhow::bail!("locked"));
        assert_eq!(
            update_secret::<TestConfig>(&secrets, "llm_api_key", "", None)
                .unwrap_err()
                .to_string(),
            "locked"
        );
        assert_eq!(secrets.get("llm_api_key"), "GENERIC");
    }

    #[test]
    fn rotation_after_another_endpoint_is_bound_to_the_config_endpoint() {
        let secrets = MemSecrets::default();
        update_secret(&secrets, "llm_api_key", "B", Some(&bound())).unwrap();
        let mut other = bound();
        other.set_str("llm_base_url", "https://api.openai.com/v1");
        assert_eq!(llm_api_key(&other, &secrets), "");
        assert_eq!(llm_api_key(&bound(), &secrets), "B");
    }

    #[test]
    fn invalid_ref_and_unknown_names_refused_without_writes() {
        let secrets = MemSecrets::default();
        let bad = config_with(&[("llm_key_ref", "llm_activation_invalid")]);
        for value in ["", "x"] {
            let err = update_secret(&secrets, "llm_api_key", value, Some(&bad)).unwrap_err();
            assert!(err.downcast_ref::<InvalidKeyRef>().is_some());
        }
        let err = update_secret::<TestConfig>(&secrets, "other", "x", None).unwrap_err();
        assert_eq!(err.to_string(), "'other'");
        assert!(err.downcast_ref::<UnknownSecret>().is_some());
        // Only llm_api_key is redirected.
        update_secret(&secrets, "fireworks_api_key", "fw", Some(&bad)).unwrap();
        assert_eq!(
            secrets.writes.borrow().clone(),
            vec!["set fireworks_api_key".to_string()]
        );
    }

    #[test]
    fn models_and_test_keep_the_key_to_the_saved_url() {
        let server = MockServer::start(vec![
            MockResponse::json(200, r#"{"data":[{"id":"m1"}]}"#),
            MockResponse::json(500, "{}"),
            MockResponse::json(200, r#"{"data":[]}"#),
            MockResponse::json(401, "{}"),
            MockResponse::json(200, r#"{"data":[]}"#),
        ]);
        let saved = format!("{}/v1", server.url());
        let other = format!("{}/other", server.url());
        let secrets = MemSecrets::with(&[("llm_api_key", "GENERIC")]);
        let cfg = config_with(&[
            ("llm_provider", "openai-compatible"),
            ("llm_base_url", &saved),
        ]);

        let result = models_llm(&cfg, "openai-compatible", None, &secrets);
        assert_eq!(
            result,
            ModelsResult {
                models: vec![ModelInfo {
                    id: "m1".into(),
                    label: "m1".into(),
                    installed: None
                }],
                error: None
            }
        );
        assert_eq!(
            server.request(0).header("authorization").as_deref(),
            Some("Bearer GENERIC")
        );

        let result = models_llm(&cfg, "openai-compatible", Some(&other), &secrets);
        let error = result.error.unwrap();
        assert!(
            error.starts_with("OpenAI-compatible model listing failed: 500 Server Error"),
            "{error}"
        );
        assert!(error.ends_with(UNSAVED_URL_HINT));
        assert!(
            server.request(1).header("authorization").is_none(),
            "never sent to an unsaved URL"
        );

        assert_eq!(
            test_llm(&cfg, None, Some(&format!("{saved}/")), None, &secrets),
            TestResult {
                ok: true,
                detail: String::new()
            }
        );
        assert_eq!(
            server.request(2).header("authorization").as_deref(),
            Some("Bearer GENERIC")
        );

        let result = test_llm(&cfg, Some(""), Some(&other), Some("x"), &secrets);
        assert!(!result.ok);
        assert!(
            result
                .detail
                .starts_with("OpenAI-compatible model listing failed: 401 Client Error")
                && result.detail.ends_with(UNSAVED_URL_HINT)
        );
        assert!(server.request(3).header("authorization").is_none());

        assert_eq!(cli_list_models(&cfg, None, None, &secrets).unwrap(), vec![]);
        assert_eq!(server.request(4).path, "/v1/models");
    }

    #[test]
    fn unknown_provider_errors() {
        let secrets = MemSecrets::default();
        let cfg = default_config();
        let expected = "\"unknown LLM provider 'x'; valid ids: ollama, openai-compatible\"";
        assert_eq!(
            models_llm(&cfg, "x", None, &secrets).error.as_deref(),
            Some(expected)
        );
        assert_eq!(
            test_llm(&cfg, Some("x"), None, None, &secrets),
            TestResult {
                ok: false,
                detail: expected.into()
            }
        );
        assert_eq!(
            test_llm(&cfg, Some("x"), Some("http://elsewhere"), None, &secrets).detail,
            format!("{expected}{UNSAVED_URL_HINT}")
        );
        assert_eq!(
            cli_list_models(&cfg, Some("x"), None, &secrets).unwrap_err(),
            expected
        );
    }

    #[test]
    fn test_llm_against_ollama_and_unreachable() {
        let server = MockServer::start(vec![MockResponse::json(200, r#"{"models":[]}"#)]);
        let cfg = config_with(&[("llm_base_url", &server.url())]);
        assert!(test_llm(&cfg, None, None, None, &MemSecrets::default()).ok);
        let cfg = config_with(&[("llm_base_url", &crate::testutil::closed_port_url())]);
        let result = test_llm(&cfg, None, None, None, &MemSecrets::default());
        assert!(!result.ok && !result.detail.is_empty());
        assert!(cli_list_models(&cfg, None, None, &MemSecrets::default())
            .unwrap_err()
            .starts_with("Ollama model listing failed: "));
    }
}
