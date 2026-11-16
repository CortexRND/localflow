//! Typed reads of the shared config plus the dictation on/off switch.
//!
//! `dictation_enabled` (`[dictation] enabled`) and `dictation_toggle_hotkey`
//! (`[hotkey] dictation_toggle`) are the Python app's keys; until lf-core's
//! field table carries them they are filled from the raw TOML. The toggle edits
//! just that key in place (toml_edit), so every other byte of the user's
//! file (comments, ordering) is preserved.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use lf_core::config::Config;
use serde_json::Value;

use crate::status::write_atomic;

pub const DICTATION_ENABLED: &str = "dictation_enabled";
pub const DICTATION_TOGGLE_HOTKEY: &str = "dictation_toggle_hotkey";

pub fn bool_field(config: &Config, name: &str, default: bool) -> bool {
    config.field_bool(name).unwrap_or(default)
}

pub fn str_field(config: &Config, name: &str) -> String {
    config.field_str(name).unwrap_or_default().to_string()
}

pub fn f64_field(config: &Config, name: &str, default: f64) -> f64 {
    config.field_f64(name).unwrap_or(default)
}

pub fn i64_field(config: &Config, name: &str, default: i64) -> i64 {
    config.field_i64(name).unwrap_or(default)
}

pub fn dictation_enabled(config: &Config) -> bool {
    bool_field(config, DICTATION_ENABLED, true)
}

/// Fill `dictation_enabled` / `dictation_toggle_hotkey` from the raw file
/// data when lf-core's field table doesn't carry them (yet). A wrong type
/// keeps the default, as Python does.
pub fn apply_extras(config: &mut Config, data: &serde_json::Map<String, Value>) {
    let section = |name: &str| data.get(name).and_then(Value::as_object);
    if config.field(DICTATION_ENABLED).is_none() {
        let enabled = match section("dictation").and_then(|s| s.get("enabled")) {
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                log::warn!("invalid config value for dictation.enabled; using default");
                true
            }
            None => true,
        };
        config.set_field(DICTATION_ENABLED, Value::Bool(enabled));
    }
    if config.field(DICTATION_TOGGLE_HOTKEY).is_none() {
        let toggle = match section("hotkey").and_then(|s| s.get("dictation_toggle")) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        config.set_field(DICTATION_TOGGLE_HOTKEY, Value::String(toggle));
    }
}

/// The runtime config: lf-core's `load_config`, or `load_from(path)` when
/// that file exists (defaults when it doesn't), plus the dictation switch.
pub fn load_runtime_config(path: Option<&Path>) -> Result<Config> {
    let (mut config, file) = match path {
        Some(p) if p.exists() => (lf_core::config::load_from(p)?, Some(p.to_path_buf())),
        Some(_) => (Config::default(), None),
        None => {
            let p = lf_core::config::try_config_path()?;
            let exists = p.exists();
            (lf_core::config::load_config()?, exists.then_some(p))
        }
    };
    let data = match &file {
        Some(p) => lf_core::config::parse_toml_file(p)?,
        None => serde_json::Map::new(),
    };
    apply_extras(&mut config, &data);
    Ok(config)
}

/// Write `[dictation] enabled = <enabled>` into the file at `path`, keeping
/// everything else as is. A missing file is first created with the defaults.
pub fn set_dictation_enabled(path: &Path, enabled: bool) -> Result<()> {
    let text = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        lf_core::config::config_toml(&Config::default())?
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| anyhow!("invalid config file {}: {e}", path.display()))?;
    // An inline `dictation = { ... }` table is edited in place too.
    match doc
        .get_mut("dictation")
        .and_then(|item| item.as_table_like_mut())
    {
        Some(section) => {
            section.insert("enabled", toml_edit::value(enabled));
        }
        None => {
            doc["dictation"] = toml_edit::table();
            doc["dictation"]["enabled"] = toml_edit::value(enabled);
        }
    }
    write_atomic(path, &doc.to_string())?;
    Ok(())
}

/// The `dictation_toggle` event: reload, flip, save; the new value.
pub fn toggle_dictation(path: &Path) -> Result<bool> {
    let fresh = load_runtime_config(Some(path))?;
    let enabled = !dictation_enabled(&fresh);
    set_dictation_enabled(path, enabled)?;
    Ok(enabled)
}

/// `hub_model.dictation_state` labels for the menu bar.
pub fn dictation_label(enabled: bool) -> &'static str {
    if enabled {
        "Dictation: On"
    } else {
        "Dictation: Off"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_enabled_and_toggle_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = load_runtime_config(Some(&path)).unwrap();
        assert!(dictation_enabled(&config));
        assert_eq!(str_field(&config, DICTATION_TOGGLE_HOTKEY), "");

        assert!(!toggle_dictation(&path).unwrap());
        assert!(!dictation_enabled(
            &load_runtime_config(Some(&path)).unwrap()
        ));
        assert!(toggle_dictation(&path).unwrap());
        assert!(dictation_enabled(
            &load_runtime_config(Some(&path)).unwrap()
        ));
    }

    #[test]
    fn toggle_preserves_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# my notes\n[hotkey]\nkey = \"alt_r\" # keep me\ndictation_toggle = \"<ctrl>+<alt>+d\"\n";
        std::fs::write(&path, original).unwrap();
        set_dictation_enabled(&path, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(original), "{text}");
        assert!(text.contains("[dictation]\nenabled = false"));
        let config = load_runtime_config(Some(&path)).unwrap();
        assert!(!dictation_enabled(&config));
        assert_eq!(
            str_field(&config, DICTATION_TOGGLE_HOTKEY),
            "<ctrl>+<alt>+d"
        );
    }

    #[test]
    fn toggle_keeps_an_inline_dictation_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "dictation = { sample_rate = 48000 }\n").unwrap();
        set_dictation_enabled(&path, false).unwrap();
        let config = load_runtime_config(Some(&path)).unwrap();
        assert!(!dictation_enabled(&config));
        assert_eq!(config.field_i64("sample_rate"), Some(48000));
    }

    #[test]
    fn wrong_type_falls_back_to_the_default_like_python() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[dictation]\nenabled = \"false\"\n").unwrap();
        assert!(dictation_enabled(
            &load_runtime_config(Some(&path)).unwrap()
        ));
    }
}
