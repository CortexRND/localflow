use anyhow::{bail, Result};
use lf_core::config::Config;

use crate::EngineSpec;

pub fn backend_available(provider: &str) -> bool {
    matches!(canonical_backend(provider), Ok("whisper-cpp") if cfg!(feature = "whisper"))
        || matches!(canonical_backend(provider), Ok("parakeet-onnx") if cfg!(feature = "parakeet"))
}

/// Python provider ids select their native-Rust counterpart; model size is unchanged.
pub fn canonical_backend(provider: &str) -> Result<&'static str> {
    match provider {
        "auto" | "faster-whisper" | "mlx" | "mlx-whisper" | "whisper-cpp" => Ok("whisper-cpp"),
        "parakeet" | "parakeet-onnx" => Ok("parakeet-onnx"),
        _ => bail!("unsupported speech provider: {provider}"),
    }
}

pub fn from_config(config: &Config, provider_override: Option<&str>) -> Result<EngineSpec> {
    spec(config, provider_override, None)
}

/// [`from_config`] for an already installed model in the HF cache at
/// `cache_root` only: construction fails when the configured model's files
/// aren't there, and the engine never downloads them later. Same backend,
/// model, language, provider and key; no other model or backend is chosen.
pub fn from_config_cached(
    config: &Config,
    provider_override: Option<&str>,
    cache_root: &std::path::Path,
) -> Result<EngineSpec> {
    spec(config, provider_override, Some(cache_root))
}

fn spec(
    config: &Config,
    provider_override: Option<&str>,
    cache_root: Option<&std::path::Path>,
) -> Result<EngineSpec> {
    let backend = canonical_backend(config.field_str("stt_backend").unwrap_or("auto"))?;
    let model = config.field_str("model_size").unwrap_or("base");
    let language = config
        .field_str("language")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let provider =
        provider_override.unwrap_or_else(|| config.field_str("stt_device").unwrap_or("auto"));
    let engine = build_engine(backend, model, language.clone(), provider, cache_root)?;
    Ok(EngineSpec {
        key: serde_json::to_string(&(backend, model, &language, provider))?,
        backend: backend.into(),
        model_size: model.into(),
        engine,
    })
}

fn build_engine(
    backend: &str,
    _model: &str,
    _language: Option<String>,
    _provider: &str,
    _cache_root: Option<&std::path::Path>,
) -> Result<Box<dyn crate::SpeechEngine>> {
    match backend {
        #[cfg(feature = "whisper")]
        "whisper-cpp" => Ok(Box::new(match _cache_root {
            Some(root) => {
                crate::engines::WhisperEngine::cached(_model, _language, _provider, root)?
            }
            None => crate::engines::WhisperEngine::new(_model, _language, _provider)?,
        })),
        #[cfg(feature = "parakeet")]
        "parakeet-onnx" => Ok(Box::new(match _cache_root {
            Some(root) => crate::engines::ParakeetEngine::cached(_provider, root)?,
            None => crate::engines::ParakeetEngine::new(_provider)?,
        })),
        _ => bail!(
            "{backend} feature is disabled; rebuild lf-stt with the corresponding engine feature"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn availability_uses_compiled_engines_without_constructing_models() {
        for provider in [
            "auto",
            "faster-whisper",
            "mlx",
            "mlx-whisper",
            "whisper-cpp",
        ] {
            assert_eq!(backend_available(provider), cfg!(feature = "whisper"));
        }
        for provider in ["parakeet", "parakeet-onnx"] {
            assert_eq!(backend_available(provider), cfg!(feature = "parakeet"));
        }
        for provider in ["", "plugin", "unknown"] {
            assert!(!backend_available(provider));
        }
    }
    #[test]
    fn maps_all_provider_ids_and_rejects_plugins() {
        for id in [
            "auto",
            "faster-whisper",
            "mlx",
            "mlx-whisper",
            "whisper-cpp",
            "parakeet",
            "parakeet-onnx",
        ] {
            let expected = if id.starts_with("parakeet") {
                "parakeet-onnx"
            } else {
                "whisper-cpp"
            };
            assert_eq!(canonical_backend(id).unwrap(), expected);
            let mut config = Config::default();
            config.fields.insert("stt_backend".into(), json!(id));
            config.fields.insert("model_size".into(), json!("small.en"));
            let spec = from_config(&config, Some("cpu"));
            if (expected == "whisper-cpp" && cfg!(feature = "whisper"))
                || (expected == "parakeet-onnx" && cfg!(feature = "parakeet"))
            {
                let spec = spec.unwrap();
                assert_eq!(spec.backend, expected);
                assert_eq!(spec.engine.id(), expected);
                assert_eq!(spec.model_size, "small.en");
                assert!(!spec.engine.loaded());
                let (repo, known) = crate::stt_lease::runtime_repo(&spec.backend, &spec.model_size);
                assert!(known && repo.is_some());
            } else {
                assert!(spec.is_err());
            }
        }
        assert!(canonical_backend("").is_err());
        assert!(canonical_backend("plugin").is_err());
    }
    #[cfg(feature = "whisper")]
    #[test]
    fn cached_specs_keep_the_configured_model_and_refuse_missing_files() {
        let cache = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.fields.insert("model_size".into(), json!("small.en"));
        let error = from_config_cached(&config, Some("cpu"), cache.path())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("small.en"), "{error}");
        crate::downloads::install_for_test(
            cache.path(),
            crate::model_storage::WHISPER_CPP_REPO,
            &[("ggml-small.en.bin", b"weights")],
        );
        let cached = from_config_cached(&config, Some("cpu"), cache.path()).unwrap();
        let plain = from_config(&config, Some("cpu")).unwrap();
        assert_eq!(
            (&cached.key, &cached.backend, &cached.model_size),
            (&plain.key, &plain.backend, &plain.model_size)
        );
        assert!(!cached.engine.loaded());
        // A Parakeet selection never silently becomes the installed Whisper.
        config
            .fields
            .insert("stt_backend".into(), json!("parakeet"));
        assert!(from_config_cached(&config, Some("cpu"), cache.path()).is_err());
    }

    #[cfg(feature = "whisper")]
    #[test]
    fn key_covers_language_model_and_provider() {
        let mut config = Config::default();
        let first = from_config(&config, Some("cpu")).unwrap().key;
        config.fields.insert("language".into(), json!("en"));
        let language = from_config(&config, Some("cpu")).unwrap().key;
        assert_ne!(first, language);
        config.fields.insert("model_size".into(), json!("tiny.en"));
        assert_ne!(language, from_config(&config, Some("cpu")).unwrap().key);
        assert_ne!(
            from_config(&config, Some("cpu")).unwrap().key,
            from_config(&config, Some("auto")).unwrap().key
        );
    }
}
