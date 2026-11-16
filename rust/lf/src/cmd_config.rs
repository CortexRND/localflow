//! `config path|show|get|set|migrate` (localflow/cli.py `config` group).

use anyhow::Result;
use lf_core::config;
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
    match config::migrate_legacy(None)? {
        None => outln!(io.out, "no legacy config found"),
        Some(_) => outln!(
            io.out,
            "migrated legacy config to {}",
            config::try_config_path()?.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
