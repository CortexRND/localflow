use anyhow::{bail, ensure, Result};
use lf_core::config::{apply_config_update, field_location, Config};
use serde_json::{json, Value};
use std::collections::HashSet;
use unicode_general_category::{get_general_category, GeneralCategory as G};
use unicode_normalization::UnicodeNormalization;

pub const ALLOWLIST: &[&str] = &[
    "sounds_enabled",
    "dictation_enabled",
    "cleanup_enabled",
    "cleanup_level",
    "styles_enabled",
    "style_work",
    "style_email",
    "style_personal",
    "style_other",
    "hotkey",
    "command_hotkey",
    "polish_hotkey",
    "prompt_hotkey",
    "paste_last_hotkey",
    "paste_method",
    "spoken_symbols",
    "spoken_punctuation",
    "hot_phrases",
    "meeting_watch",
    "meeting_auto_start",
    "capture_system_audio",
    "meeting_default_template",
    "work_prompts",
];
pub const SHORTCUTS: &[&str] = &[
    "hotkey",
    "command_hotkey",
    "polish_hotkey",
    "prompt_hotkey",
    "paste_last_hotkey",
];
const MODIFIERS: &[&str] = &[
    "alt", "alt_l", "alt_r", "alt_gr", "ctrl", "ctrl_l", "ctrl_r", "cmd", "cmd_l", "cmd_r",
    "shift", "shift_l", "shift_r",
];

pub fn raw_text_ok(s: &str) -> bool {
    s.chars().all(|c| {
        !matches!(
            get_general_category(c),
            G::Control
                | G::Format
                | G::Surrogate
                | G::PrivateUse
                | G::Unassigned
                | G::LineSeparator
                | G::ParagraphSeparator
        ) && (!c.is_whitespace() || c == ' ')
    })
}
fn letter_number_mark(c: char) -> bool {
    matches!(
        get_general_category(c),
        G::UppercaseLetter
            | G::LowercaseLetter
            | G::TitlecaseLetter
            | G::ModifierLetter
            | G::OtherLetter
            | G::DecimalNumber
            | G::LetterNumber
            | G::OtherNumber
            | G::NonspacingMark
            | G::SpacingMark
    )
}
pub fn clean_text(s: &str, limit: usize) -> Result<String> {
    ensure!(
        s.chars().count() <= 4 * limit && raw_text_ok(s),
        "invalid text"
    );
    let s: String = s.trim().nfc().collect();
    ensure!(
        !s.is_empty() && s.chars().count() <= limit,
        "empty or overlong text"
    );
    ensure!(
        s.chars()
            .all(|c| letter_number_mark(c) || " -'’‘.,&+/#@_()!?:;\"“”–—·%$€£¥*".contains(c)),
        "text must use ordinary punctuation"
    );
    Ok(s)
}
pub fn safe_text(s: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut size = 0;
    for c in s.nfc() {
        let part = if letter_number_mark(c)
            || " .,;:!?'\"()[]-_+/&@#%*=<>|~`$€£¥^“”‘’—–…·".contains(c)
        {
            c.to_string()
        } else if (c as u32) <= 0xffff {
            format!("\\u{:04x}", c as u32)
        } else {
            format!("\\U{:08x}", c as u32)
        };
        if size + part.chars().count() > limit {
            out.push('…');
            break;
        }
        size += part.chars().count();
        out.push_str(&part);
    }
    out
}
fn token(s: &str) -> String {
    let s = s
        .trim()
        .to_lowercase()
        .trim_matches(['<', '>'])
        .replace([' ', '-'], "_");
    match s.as_str() {
        "option" | "opt" => "alt",
        "command" => "cmd",
        "control" | "ctl" => "ctrl",
        "right_option" | "right_alt" => "alt_r",
        "left_option" | "left_alt" => "alt_l",
        "right_command" | "right_cmd" => "cmd_r",
        "left_command" | "left_cmd" => "cmd_l",
        "right_ctrl" | "right_control" => "ctrl_r",
        "left_ctrl" | "left_control" => "ctrl_l",
        "right_shift" => "shift_r",
        "left_shift" => "shift_l",
        _ => &s,
    }
    .to_string()
}
fn function(s: &str) -> bool {
    s.strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=20).contains(&n) && s == format!("f{n}"))
}
pub fn normalize_hold_key(s: &str, platform: &str) -> Result<String> {
    let s = token(s);
    ensure!(
        MODIFIERS.contains(&s.as_str()) || function(&s),
        "not a key you can hold"
    );
    ensure!(platform != "darwin" || s != "insert", "key not available");
    Ok(s)
}
pub fn normalize_combo(s: &str, platform: &str) -> Result<String> {
    let tokens: Vec<String> = s.split('+').map(token).collect();
    let mut seen = HashSet::new();
    ensure!(
        tokens.iter().all(|t| !t.is_empty() && seen.insert(t)),
        "invalid or repeated key"
    );
    let (mods, keys): (Vec<_>, Vec<_>) =
        tokens.iter().partition(|t| MODIFIERS.contains(&t.as_str()));
    ensure!(
        mods.iter().any(|m| !m.starts_with("shift")) && keys.len() == 1,
        "shortcut needs Ctrl, Option/Alt or Cmd and one key"
    );
    let key = keys[0];
    let named = function(key)
        || [
            "space",
            "tab",
            "enter",
            "backspace",
            "delete",
            "home",
            "end",
            "page_up",
            "page_down",
            "up",
            "down",
            "left",
            "right",
            "insert",
        ]
        .contains(&key.as_str());
    ensure!(
        named || (key.chars().count() == 1 && raw_text_ok(key) && !"+<> ".contains(key.as_str())),
        "unknown shortcut key"
    );
    ensure!(platform != "darwin" || key != "insert", "key not available");
    let mut result: Vec<_> = mods.iter().map(|m| format!("<{m}>")).collect();
    result.push(if named {
        format!("<{key}>")
    } else {
        key.clone()
    });
    Ok(result.join("+"))
}
pub fn update(config: &Config, key: &str, value: &Value) -> Result<Config> {
    ensure!(ALLOWLIST.contains(&key), "setting is not allowed");
    let (section, field) = field_location(key).ok_or_else(|| anyhow::anyhow!("unknown setting"))?;
    apply_config_update(config, &json!({section: {field: value}}))
}
#[derive(Clone, Debug)]
pub struct Validator {
    pub platform: String,
    pub templates: Vec<String>,
}
impl Default for Validator {
    fn default() -> Self {
        Self {
            platform: if cfg!(target_os = "macos") {
                "darwin"
            } else if cfg!(windows) {
                "win32"
            } else {
                "linux"
            }
            .into(),
            templates: vec!["general".into()],
        }
    }
}
impl Validator {
    pub fn choices(&self, key: &str) -> Vec<String> {
        if key == "meeting_default_template" {
            return self.template_ids();
        }
        if key == "paste_method" {
            let methods: &[&str] = match self.platform.as_str() {
                "darwin" => &["auto", "clipboard", "osascript"],
                "win32" => &["auto", "clipboard", "pynput"],
                _ => &["auto", "clipboard", "wtype", "xdotool", "pynput"],
            };
            return methods.iter().map(|s| (*s).into()).collect();
        }
        lf_core::config::ENUM_VALUES
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, values)| values.iter().map(|s| (*s).into()).collect())
            .unwrap_or_default()
    }
    pub fn template_ids(&self) -> Vec<String> {
        self.templates
            .iter()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 48
                    && s.bytes()
                        .next()
                        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    && s.bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))
            })
            .cloned()
            .collect()
    }
    pub fn value(&self, key: &str, value: &Value, config: &Config) -> Result<Value> {
        ensure!(ALLOWLIST.contains(&key), "setting is not allowed");
        if let Some(s) = value.as_str() {
            ensure!(
                s.chars().count() <= 64 && raw_text_ok(s),
                "invalid setting value"
            );
        }
        ensure!(
            value.is_string() || value.is_boolean() || value.is_null(),
            "value must be text or on/off"
        );
        let text = value.as_str().unwrap_or("").trim().to_lowercase();
        let off = value.is_null()
            || (value.is_string() && ["", "off", "none", "disabled"].contains(&text.as_str()));
        let result = if config.field(key).is_some_and(Value::is_boolean) {
            if value.is_boolean() {
                value.clone()
            } else if ["true", "on", "yes", "enabled", "enable", "1"].contains(&text.as_str()) {
                json!(true)
            } else if ["false", "off", "no", "disabled", "disable", "0"].contains(&text.as_str()) {
                json!(false)
            } else {
                bail!("must be on or off")
            }
        } else if SHORTCUTS.contains(&key) {
            if off {
                ensure!(key != "hotkey", "dictation shortcut cannot be off");
                json!("")
            } else if key == "command_hotkey" || (key == "hotkey" && !text.contains('+')) {
                json!(normalize_hold_key(&text, &self.platform)?)
            } else {
                json!(normalize_combo(&text, &self.platform)?)
            }
        } else if key == "paste_method" {
            let allowed: &[&str] = match self.platform.as_str() {
                "darwin" => &["auto", "clipboard", "osascript"],
                "win32" => &["auto", "clipboard", "pynput"],
                _ => &["auto", "clipboard", "wtype", "xdotool", "pynput"],
            };
            ensure!(
                allowed.contains(&text.as_str()),
                "paste method not available"
            );
            json!(text)
        } else if key == "meeting_default_template" {
            ensure!(self.template_ids().contains(&text), "unknown template");
            json!(text)
        } else {
            json!(text.replace([' ', '-'], "_"))
        };
        update(config, key, &result)?;
        Ok(result)
    }
    pub fn conflicts(&self, config: &Config, changed: &[String]) -> Result<()> {
        for key in changed.iter().filter(|k| SHORTCUTS.contains(&k.as_str())) {
            let value = config.field_str(key).unwrap_or("");
            if value.is_empty() {
                continue;
            }
            for other in SHORTCUTS.iter().filter(|k| **k != key.as_str()) {
                let rhs = config.field_str(other).unwrap_or("");
                ensure!(
                    rhs.is_empty()
                        || identity(value, &self.platform) != identity(rhs, &self.platform),
                    "shortcut conflicts with another shortcut"
                );
            }
        }
        Ok(())
    }
}
fn identity(s: &str, platform: &str) -> String {
    if !s.contains('+') {
        let mut t = token(s);
        if platform == "darwin" {
            t = match t.as_str() {
                "alt_l" => "alt",
                "ctrl_l" => "ctrl",
                "cmd_l" => "cmd",
                "shift_l" => "shift",
                "alt_gr" => "alt_r",
                _ => &t,
            }
            .into();
        }
        return format!("hold:{t}");
    }
    let mut parts: Vec<_> = s
        .split('+')
        .map(token)
        .map(|s| {
            if MODIFIERS.contains(&s.as_str()) {
                s.strip_suffix("_l")
                    .or_else(|| s.strip_suffix("_r"))
                    .unwrap_or(&s)
                    .to_string()
            } else {
                s
            }
        })
        .collect();
    parts.sort();
    parts.dedup();
    format!("combo:{}", parts.join("+"))
}
