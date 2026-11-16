//! HTTP-independent ports of the remaining `localflow/api.py` helpers.
use lf_core::config::{self, Config};
use lf_providers::{api, factory, store::CoreSecrets};
use serde_json::{json, Value};

pub const STT_IDS: [&str; 3] = ["faster-whisper", "mlx-whisper", "parakeet"];

pub fn resolve_stt_provider(provider: &str) -> &str {
    match provider {
        "auto" => "faster-whisper",
        "mlx" => "mlx-whisper",
        _ => provider,
    }
}

pub fn restart_required(update: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(update) = update.as_object() {
        for (section, values) in update {
            if let Some(values) = values.as_object() {
                for key in values.keys() {
                    if matches!(
                        section.as_str(),
                        "meetings" | "server" | "hotkey" | "features"
                    ) || section == "dictation"
                        && matches!(
                            key.as_str(),
                            "sample_rate" | "max_recording_seconds" | "pause_media"
                        )
                    {
                        keys.push(format!("{section}.{key}"));
                    }
                }
            }
        }
    }
    keys
}

pub fn validate_runtime(config: &Config) -> Result<(), String> {
    let stt = config.field_str("stt_backend").unwrap_or_default();
    if !STT_IDS.contains(&resolve_stt_provider(stt)) {
        return Err(format!(
            "unknown stt provider {}",
            lf_providers::pyfmt::py_repr(stt)
        ));
    }
    let llm = config.field_str("llm_provider").unwrap_or_default();
    if !lf_providers::registry::list_llm_ids().contains(&llm) {
        return Err(format!(
            "unknown llm provider {}",
            lf_providers::pyfmt::py_repr(llm)
        ));
    }
    factory::build_llm(config, &factory::BuildOptions::default(), &CoreSecrets)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn status(config: &Config, backend: &dyn crate::backend::Backend) -> Value {
    let f = |key| config.field(key).cloned().unwrap_or(Value::Null);
    let platform = std::env::var("LOCALFLOW_PLATFORM").unwrap_or_else(|_| {
        if cfg!(target_os = "macos") {
            "darwin"
        } else if cfg!(windows) {
            "win32"
        } else {
            "linux"
        }
        .into()
    });
    json!({
        "platform": platform,
        "config_path": config::config_path(),
        "stt": {"provider": f("stt_backend"), "model": f("model_size"), "device": f("stt_device"), "language": f("language"), "loaded": backend.transcriber_loaded()},
        "llm": {"provider": f("llm_provider"), "base_url": f("llm_base_url"), "model": f("llm_model"), "cleanup_enabled": f("cleanup_enabled")},
        "hotkey": f("hotkey"), "paste_method": f("paste_method"), "sounds_enabled": f("sounds_enabled"),
        "dictation_enabled": f("dictation_enabled"),
        "server": {"host": f("server_host"), "port": f("server_port")},
        "meetings_enabled": f("meetings_enabled"), "autostart": backend.autostart_status()
    })
}

pub fn stt_providers(backend: &dyn crate::backend::Backend) -> Value {
    Value::Array(
        STT_IDS
            .iter()
            .map(|id| json!({"id": id, "offline": true, "available": backend.stt_available(id)}))
            .collect(),
    )
}

pub fn models(config: &Config, kind: &str, provider: &str, base_url: Option<&str>) -> Value {
    if kind == "llm" {
        return serde_json::to_value(api::models_llm(config, provider, base_url, &CoreSecrets))
            .unwrap();
    }
    let models: &[&str] = match (kind, resolve_stt_provider(provider)) {
        ("stt", "faster-whisper") => &["tiny", "base", "small", "medium", "large-v3"],
        ("stt", "mlx-whisper") => &[
            "tiny",
            "base",
            "small",
            "medium",
            "large-v3",
            "large-v3-turbo",
        ],
        ("stt", "parakeet") => &["mlx-community/parakeet-tdt-0.6b-v2"],
        ("stt", unknown) => {
            return json!({"models": [], "error": lf_providers::pyfmt::py_repr(&format!("unknown STT provider {}; valid ids: {}", lf_providers::pyfmt::py_repr(unknown), STT_IDS.join(", ")))})
        }
        _ => {
            return json!({"models": [], "error": format!("unknown model kind: {}", lf_providers::pyfmt::py_repr(kind))})
        }
    };
    json!({"models": models.iter().map(|m| json!({"id": m, "label": m, "installed": null})).collect::<Vec<_>>(), "error": null})
}

pub fn secrets_status(config: &Config) -> Value {
    Value::Object(
        api::secrets_status(&CoreSecrets, Some(config))
            .into_iter()
            .map(|(k, v)| (k, Value::Bool(v)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_fields_follow_payload_order() {
        assert_eq!(
            restart_required(
                &json!({"dictation": {"model_size": "tiny", "pause_media": false, "sample_rate": 16000}, "server": {"port": 9000}, "llm": {"model": "x"}, "features": {"meetings": false}})
            ),
            vec![
                "dictation.pause_media",
                "dictation.sample_rate",
                "server.port",
                "features.meetings"
            ]
        );
    }
    #[test]
    fn stt_aliases_and_catalogs() {
        let config = Config::default();
        assert_eq!(
            models(&config, "stt", "auto", None),
            models(&config, "stt", "faster-whisper", None)
        );
        assert_eq!(
            models(&config, "stt", "mlx", None)["models"]
                .as_array()
                .unwrap()
                .len(),
            6
        );
        assert_eq!(
            models(&config, "bad", "p", None)["error"],
            "unknown model kind: 'bad'"
        );
    }
}
