//! `config path|show|get|set|migrate` (localflow/cli.py `config` group).

use anyhow::Result;
use lf_core::config::{self, MigrationOutcome};
use serde_json::{json, Value};

use crate::args::ConfigCommand;
use crate::pyfmt::{self, py_strip};
use crate::{fail, outln, Io};

pub(crate) fn run(command: ConfigCommand, io: &mut Io<'_>) -> Result<()> {
    match command {
        ConfigCommand::Path => path(io),
        ConfigCommand::Show => show(io),
        ConfigCommand::Get { section_key: None } => show(io),
        ConfigCommand::Get {
            section_key: Some(key),
        } => get(&key, io),
        ConfigCommand::Set {
            section_key,
            raw_value,
        } => set(&section_key, &raw_value, io),
        ConfigCommand::Migrate => migrate(io),
    }
}

fn path(io: &mut Io<'_>) -> Result<()> {
    let legacy = config::try_legacy_config_path()?;
    outln!(io.out, "path: {}", config::try_config_path()?.display())?;
    let state = if legacy.exists() { "exists" } else { "missing" };
    outln!(io.out, "legacy: {state} ({})", legacy.display())
}

fn show(io: &mut Io<'_>) -> Result<()> {
    let text = config::config_toml(&config::load_config()?)?;
    write!(io.out, "{text}")?;
    Ok(())
}

/// `(section, file_key, field_name)` for a dotted key, or the Python error.
fn resolve_key(section_key: &str) -> Result<(String, String, String)> {
    match config::config_field(section_key) {
        Some((section, field_name)) => {
            let key = section_key
                .split_once('.')
                .map(|(_, key)| key.to_string())
                .unwrap_or_default();
            Ok((section.to_string(), key, field_name.to_string()))
        }
        None => fail(format!(
            "unknown config key {}; valid keys: {}",
            pyfmt::str_repr(section_key),
            config::valid_config_keys().join(", ")
        )),
    }
}

/// Render a field value the way `lf config get` does: JSON for lists and
/// booleans, nothing for None, `str()` otherwise.
pub(crate) fn get_text(value: &Value) -> String {
    match value {
        Value::Array(_) | Value::Bool(_) => pyfmt::json_dumps(value),
        Value::Null => String::new(),
        other => pyfmt::value_str(other),
    }
}

fn get(section_key: &str, io: &mut Io<'_>) -> Result<()> {
    let (_, _, field_name) = resolve_key(section_key)?;
    let loaded = config::load_config()?;
    let value = loaded
        .fields
        .get(&field_name)
        .cloned()
        .unwrap_or(Value::Null);
    outln!(io.out, "{}", get_text(&value))
}

/// Digits with optional single underscores between digits, as Python's
/// `int()`/`float()` accept them; returns the digits without underscores.
fn digits_with_underscores(text: &str) -> Option<String> {
    if text.is_empty() || text.starts_with('_') || text.ends_with('_') || text.contains("__") {
        return None;
    }
    if !text.chars().all(|c| c.is_ascii_digit() || c == '_') {
        return None;
    }
    Some(text.replace('_', ""))
}

/// Python `int(raw)` for base-10 text (ASCII digits) within the i64 range
/// TOML can store.
pub(crate) fn parse_py_int(raw: &str) -> Option<i64> {
    let text = py_strip(raw);
    let (sign, body) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text.strip_prefix('+').unwrap_or(text)),
    };
    let digits = digits_with_underscores(body)?;
    format!("{sign}{digits}").parse().ok()
}

/// Python `float(raw)` restricted to finite values (a non-finite float would
/// be written as JSON `Infinity`/`NaN`, which is not valid TOML).
pub(crate) fn parse_py_float(raw: &str) -> std::result::Result<f64, &'static str> {
    let text = py_strip(raw);
    let unsigned = text
        .strip_prefix('-')
        .or_else(|| text.strip_prefix('+'))
        .unwrap_or(text);
    let lowered = unsigned.to_ascii_lowercase();
    if matches!(lowered.as_str(), "inf" | "infinity" | "nan") {
        return Err("non-finite");
    }
    // Split mantissa / exponent and validate underscores per digit group.
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (&unsigned[..index], Some(&unsigned[index + 1..])),
        None => (unsigned, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    let whole = if whole.is_empty() {
        String::new()
    } else {
        digits_with_underscores(whole).ok_or("invalid")?
    };
    let fraction = match fraction {
        Some("") => Some(String::new()),
        Some(part) => Some(digits_with_underscores(part).ok_or("invalid")?),
        None => None,
    };
    if whole.is_empty() && fraction.as_deref().unwrap_or("").is_empty() {
        return Err("invalid");
    }
    let exponent = match exponent {
        Some(part) => {
            let (sign, body) = match part.strip_prefix('-') {
                Some(rest) => ("-", rest),
                None => ("", part.strip_prefix('+').unwrap_or(part)),
            };
            Some(format!(
                "{sign}{}",
                digits_with_underscores(body).ok_or("invalid")?
            ))
        }
        None => None,
    };
    let sign = if text.starts_with('-') { "-" } else { "" };
    let mut normalized = format!("{sign}{}", if whole.is_empty() { "0" } else { &whole });
    if let Some(fraction) = fraction {
        normalized.push('.');
        normalized.push_str(if fraction.is_empty() { "0" } else { &fraction });
    }
    if let Some(exponent) = exponent {
        normalized.push('e');
        normalized.push_str(&exponent);
    }
    let value: f64 = normalized.parse().map_err(|_| "invalid")?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err("non-finite")
    }
}

/// `localflow.cli._coerce_config_value`: read RAW as the type of the key's
/// current value.
pub(crate) fn coerce_value(current: &Value, raw: &str) -> Result<Value> {
    match current {
        Value::Bool(_) => match raw.to_lowercase().as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => fail("boolean values must be true or false"),
        },
        Value::Number(number) if number.is_i64() || number.is_u64() => match parse_py_int(raw) {
            Some(value) => Ok(json!(value)),
            None => fail(format!("invalid integer: {}", pyfmt::str_repr(raw))),
        },
        Value::Number(_) => match parse_py_float(raw) {
            Ok(value) => Ok(json!(value)),
            Err("non-finite") => fail(format!(
                "non-finite float {} cannot be saved in TOML",
                pyfmt::str_repr(raw)
            )),
            Err(_) => fail(format!("invalid float: {}", pyfmt::str_repr(raw))),
        },
        Value::Array(_) => Ok(Value::Array(
            raw.split(',')
                .map(py_strip)
                .filter(|item| !item.is_empty())
                .map(|item| Value::String(item.to_string()))
                .collect(),
        )),
        Value::Null => Ok(if raw.is_empty() {
            Value::Null
        } else {
            Value::String(raw.to_string())
        }),
        _ => Ok(Value::String(raw.to_string())),
    }
}

fn set(section_key: &str, raw_value: &str, io: &mut Io<'_>) -> Result<()> {
    let (section, key, field_name) = resolve_key(section_key)?;
    if section_key == "paste.method" && !config::PASTE_METHODS.contains(&raw_value) {
        return fail(format!(
            "invalid paste method {}; valid methods: {}",
            pyfmt::str_repr(raw_value),
            config::PASTE_METHODS.join(", ")
        ));
    }
    let current = config::load_config()?;
    let existing = current
        .fields
        .get(&field_name)
        .cloned()
        .unwrap_or(Value::Null);
    let value = coerce_value(&existing, raw_value)?;
    let mut values = serde_json::Map::new();
    values.insert(key, value);
    let mut update = serde_json::Map::new();
    update.insert(section, Value::Object(values));
    let updated = config::apply_config_update(&current, &Value::Object(update))?;
    let saved = config::save_config(&updated, None)?;
    outln!(io.out, "saved {}", saved.display())
}

fn migrate(io: &mut Io<'_>) -> Result<()> {
    match config::migrate_legacy_outcome(None)? {
        MigrationOutcome::MissingSource => outln!(io.out, "no legacy config found"),
        MigrationOutcome::Migrated { target, .. } => {
            outln!(io.out, "migrated legacy config to {}", target.display())
        }
        MigrationOutcome::Deferred { .. } => fail(format!(
            "legacy config not migrated: fireworks_api_key could not be stored in the keyring; \
             {} and {} were left unchanged. Set FIREWORKS_API_KEY in the environment (or fix \
             the keyring) and rerun `lf-rs config migrate`",
            config::try_legacy_config_path()?.display(),
            config::try_config_path()?.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Arc, Mutex};

    use lf_core::secrets::{self, FailingKeyring, KeyringBackend};

    use crate::testing::run_cli;

    /// The only lf unit test that touches the environment and the
    /// process-wide keyring backend; serialize anyway in case more appear.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Sets/removes variables and the keyring override; restores both on drop.
    struct Sandbox {
        saved: Vec<(&'static str, Option<OsString>)>,
        keyring: Option<Option<Arc<dyn KeyringBackend>>>,
        dir: tempfile::TempDir,
    }

    impl Sandbox {
        fn new(keyring: Arc<dyn KeyringBackend>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let names = [
                "HOME",
                "USERPROFILE",
                "LOCALFLOW_CONFIG",
                "FIREWORKS_API_KEY",
                "LOCALFLOW_FIREWORKS_API_KEY",
            ];
            let saved = names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
            std::fs::create_dir_all(dir.path().join("home")).unwrap();
            std::env::set_var("HOME", dir.path().join("home"));
            std::env::set_var("USERPROFILE", dir.path().join("home"));
            std::env::set_var("LOCALFLOW_CONFIG", dir.path().join("config.toml"));
            std::env::remove_var("FIREWORKS_API_KEY");
            std::env::remove_var("LOCALFLOW_FIREWORKS_API_KEY");
            let previous = secrets::set_keyring_backend(Some(keyring));
            Sandbox {
                saved,
                keyring: Some(previous),
                dir,
            }
        }

        fn legacy(&self) -> std::path::PathBuf {
            self.dir.path().join("home").join(".localflow.toml")
        }

        fn target(&self) -> std::path::PathBuf {
            self.dir.path().join("config.toml")
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            if let Some(previous) = self.keyring.take() {
                secrets::set_keyring_backend(previous);
            }
            secrets::reset_keyring_warning();
            for (name, value) in self.saved.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[test]
    fn migrate_reports_deferral_when_keyring_is_unavailable() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new(Arc::new(FailingKeyring("no backend".into())));
        let legacy_text = "fireworks_api_key = \"secret\"\nmodel_size = \"tiny\"\n";
        std::fs::write(sandbox.legacy(), legacy_text).unwrap();

        // No v2 target yet.
        let outcome = run_cli(&["config", "migrate"]);
        assert_eq!(outcome.code, 1, "{}", outcome.err);
        assert!(!outcome.out.contains("migrated"), "{}", outcome.out);
        assert!(outcome.err.starts_with("Error: legacy config not migrated"));
        assert!(outcome.err.contains("rerun"), "{}", outcome.err);
        assert!(!sandbox.target().exists());
        assert_eq!(
            std::fs::read_to_string(sandbox.legacy()).unwrap(),
            legacy_text
        );

        // An older v2 target already exists: still deferred, left unchanged.
        std::env::remove_var("FIREWORKS_API_KEY");
        let existing = "config_version = 2\n\n[stt]\nmodel = \"small\"\n";
        std::fs::write(sandbox.target(), existing).unwrap();
        let outcome = run_cli(&["config", "migrate"]);
        assert_eq!(outcome.code, 1, "{}", outcome.err);
        assert!(!outcome.out.contains("migrated"), "{}", outcome.out);
        assert!(outcome.err.starts_with("Error: legacy config not migrated"));
        assert_eq!(std::fs::read_to_string(sandbox.target()).unwrap(), existing);
        assert_eq!(
            std::fs::read_to_string(sandbox.legacy()).unwrap(),
            legacy_text
        );
    }

    #[test]
    fn migrate_success_and_missing_source_are_unchanged() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new(Arc::new(FailingKeyring("must not be used".into())));
        let missing = run_cli(&["config", "migrate"]);
        assert_eq!(
            (missing.code, missing.out.as_str()),
            (0, "no legacy config found\n")
        );
        assert!(missing.err.is_empty());
        std::fs::write(sandbox.legacy(), "model_size = \"tiny\"\n").unwrap();
        let migrated = run_cli(&["config", "migrate"]);
        assert_eq!(migrated.code, 0, "{}", migrated.err);
        assert_eq!(
            migrated.out,
            format!("migrated legacy config to {}\n", sandbox.target().display())
        );
        assert!(sandbox.target().exists());
        assert!(sandbox.legacy().exists());
    }

    #[test]
    fn coerce_follows_the_current_type() {
        assert_eq!(coerce_value(&json!(true), "FALSE").unwrap(), json!(false));
        assert_eq!(coerce_value(&json!(true), "True").unwrap(), json!(true));
        assert!(coerce_value(&json!(true), "yes").is_err());
        assert_eq!(coerce_value(&json!(1), " 1_200 ").unwrap(), json!(1200));
        assert_eq!(coerce_value(&json!(1), "-7").unwrap(), json!(-7));
        assert!(coerce_value(&json!(1), "1.5").is_err());
        assert!(coerce_value(&json!(1), "").is_err());
        assert!(coerce_value(&json!(1), "1__0").is_err());
        assert_eq!(coerce_value(&json!(-50.0), "-40").unwrap(), json!(-40.0));
        assert_eq!(coerce_value(&json!(-50.0), "1e2").unwrap(), json!(100.0));
        assert_eq!(coerce_value(&json!(-50.0), ".5").unwrap(), json!(0.5));
        assert_eq!(coerce_value(&json!(-50.0), "5.").unwrap(), json!(5.0));
        assert!(coerce_value(&json!(-50.0), "abc").is_err());
        assert!(coerce_value(&json!(-50.0), "inf").is_err());
        assert!(coerce_value(&json!(-50.0), ".").is_err());
        assert_eq!(
            coerce_value(&json!(["x"]), " a, ,b ,").unwrap(),
            json!(["a", "b"])
        );
        assert_eq!(coerce_value(&json!(["x"]), "").unwrap(), json!([]));
        assert_eq!(coerce_value(&Value::Null, "").unwrap(), Value::Null);
        assert_eq!(coerce_value(&Value::Null, "en").unwrap(), json!("en"));
        assert_eq!(coerce_value(&json!("s"), "").unwrap(), json!(""));
    }

    #[test]
    fn get_text_matches_python_rendering() {
        assert_eq!(get_text(&json!(["a", "é"])), "[\"a\", \"\\u00e9\"]");
        assert_eq!(get_text(&json!(false)), "false");
        assert_eq!(get_text(&Value::Null), "");
        assert_eq!(get_text(&json!(-50.0)), "-50.0");
        assert_eq!(get_text(&json!(8756)), "8756");
        assert_eq!(get_text(&json!("llama3.2:3b")), "llama3.2:3b");
    }
}
