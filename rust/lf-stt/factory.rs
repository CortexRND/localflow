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
    let backend = canonical_backend(config.field_str("stt_backend").unwrap_or("auto"))?;
    let model = config.field_str("model_size").unwrap_or("base");
    let language = config
        .field_str("language")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let provider = provider_override.unwrap_or_else(|| saved_device(config));
    let engine = build_engine(backend, model, language.clone(), provider)?;
    Ok(EngineSpec {
        key: serde_json::to_string(&(backend, model, &language, provider))?,
        backend: backend.into(),
        model_size: model.into(),
        engine,
    })
}

/// The saved `stt_device`, as a native engine provider. `cuda` is a valid
/// Python setting (faster-whisper honors it), but no native engine has a CUDA
/// path, so it runs on CPU; the config itself is left untouched. Explicit
/// provider overrides are not mapped and still reject `cuda`.
fn saved_device(config: &Config) -> &str {
    match config.field_str("stt_device").unwrap_or("auto") {
        "cuda" => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                log::warn!("stt_device = \"cuda\" is not supported by native engines; using CPU");
            });
            "cpu"
        }
        device => device,
    }
}

fn build_engine(
    backend: &str,
    _model: &str,
    _language: Option<String>,
    _provider: &str,
) -> Result<Box<dyn crate::SpeechEngine>> {
    match backend {
        #[cfg(feature = "whisper")]
        "whisper-cpp" => Ok(Box::new(crate::engines::WhisperEngine::new(
            _model, _language, _provider,
        )?)),
        #[cfg(feature = "parakeet")]
        "parakeet-onnx" => Ok(Box::new(crate::engines::ParakeetEngine::new(_provider)?)),
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
    #[test]
    fn saved_cuda_runs_native_engines_on_cpu_but_explicit_cuda_is_rejected() {
        for (backend, enabled) in [
            ("faster-whisper", cfg!(feature = "whisper")),
            ("parakeet", cfg!(feature = "parakeet")),
        ] {
            if !enabled {
                continue;
            }
            let mut config = Config::default();
            config.set_field("stt_backend", json!(backend));
            config.set_field("stt_device", json!("cuda"));
            let spec = from_config(&config, None).unwrap();
            assert!(!spec.engine.loaded());
            // Same snapshot as an explicit CPU request; nothing claims CUDA.
            assert_eq!(spec.key, from_config(&config, Some("cpu")).unwrap().key);
            assert!(!spec.key.contains("cuda"));
            assert!(from_config(&config, Some("cuda")).is_err());
            // The saved setting is not rewritten.
            assert_eq!(config.field_str("stt_device"), Some("cuda"));
        }
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
