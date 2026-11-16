use anyhow::{bail, ensure, Result};
use lf_core::config::{apply_config_update, field_location, Config};
use serde_json::{json, Value};
use std::collections::HashSet;
use unicode_general_category::{get_general_category, GeneralCategory as G};
use unicode_normalization::UnicodeNormalization;

pub const ALLOWLIST: &[&str] = &[
    "dictation_enabled",
    "dictation_toggle_hotkey",
    "sounds_enabled",
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
    "dictation_toggle_hotkey",
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
    raw_text_problem(s).is_none()
}
pub(crate) fn category_c(c: char) -> bool {
    matches!(
        get_general_category(c),
        G::Control | G::Format | G::Surrogate | G::PrivateUse | G::Unassigned
    )
}
pub(crate) fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}
pub(crate) fn raw_text_problem(s: &str) -> Option<&'static str> {
    for c in s.chars() {
        if category_c(c)
            || matches!(
                get_general_category(c),
                G::LineSeparator | G::ParagraphSeparator
            )
        {
            return Some("has control characters or line breaks");
        }
        if get_general_category(c) == G::SpaceSeparator && c != ' ' {
            return Some("has unusual spaces");
        }
    }
    None
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
    clean_text_for(
        s,
        limit,
        if limit == 64 {
            "dictionary word"
        } else {
            "written text"
        },
    )
}
pub(crate) fn clean_text_for(s: &str, limit: usize, what: &str) -> Result<String> {
    ensure!(
        s.chars().count() <= 4 * limit,
        "{what} is longer than {limit} characters"
    );
    if let Some(problem) = raw_text_problem(s) {
        bail!("{what} {problem}");
    }
    let s: String = s.trim_matches(python_whitespace).nfc().collect();
    ensure!(!s.is_empty(), "{what} is empty");
    ensure!(
        s.chars().count() <= limit,
        "{what} is longer than {limit} characters"
    );
    ensure!(
        s.chars()
            .all(|c| letter_number_mark(c) || " -'’‘.,&+/#@_()!?:;\"“”–—·%$€£¥*".contains(c)),
        "{what} may only use letters, numbers, spaces and ordinary punctuation"
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
        .trim_matches(python_whitespace)
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
pub(crate) fn function(s: &str) -> bool {
    s.strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=20).contains(&n) && s == format!("f{n}"))
}
pub(crate) fn is_hold(s: &str) -> bool {
    MODIFIERS.contains(&s) || function(s)
}
pub fn normalize_hold_key(s: &str, platform: &str) -> Result<String> {
    ensure!(
        !s.trim_matches(python_whitespace).is_empty(),
        "needs a key name"
    );
    let name = token(s);
    ensure!(
        !name.contains('+'),
        "must be a single key to hold, not a combination"
    );
    ensure!(
        is_hold(&name),
        "{} is not a key you can hold; use a modifier (e.g. alt_r) or a function key (f13)",
        repr(s)
    );
    ensure!(
        platform != "darwin" || name != "insert",
        "{} is not available on this computer",
        repr(s)
    );
    Ok(name)
}
pub fn normalize_combo(s: &str, platform: &str) -> Result<String> {
    ensure!(
        !s.trim_matches(python_whitespace).is_empty(),
        "needs a shortcut"
    );
    let tokens: Vec<String> = s.split('+').map(token).collect();
    let mut seen = HashSet::new();
    ensure!(
        tokens.iter().all(|t| !t.is_empty()),
        "{} is not a valid shortcut",
        repr(s)
    );
    ensure!(
        tokens.iter().all(|t| seen.insert(t)),
        "{} repeats a key",
        repr(s)
    );
    let (mods, keys): (Vec<_>, Vec<_>) =
        tokens.iter().partition(|t| MODIFIERS.contains(&t.as_str()));
    ensure!(
        mods.iter().any(|m| !m.starts_with("shift")),
        "{} needs Ctrl, Option/Alt or Cmd so it can't fire while typing",
        repr(s)
    );
    ensure!(
        keys.len() == 1,
        "{} needs exactly one non-modifier key",
        repr(s)
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
    if key.chars().count() == 1 {
        ensure!(
            raw_text_ok(key)
                && !key.chars().any(char::is_whitespace)
                && !"+<> ".contains(key.as_str()),
            "{} uses an unsupported key",
            repr(s)
        );
    } else {
        ensure!(named, "{} uses an unknown key {}", repr(s), repr(key));
    }
    ensure!(
        platform != "darwin" || key != "insert",
        "{} is not available on this computer",
        repr(s)
    );
    let mut result: Vec<_> = mods.iter().map(|m| format!("<{m}>")).collect();
    result.push(if named {
        format!("<{key}>")
    } else {
        key.clone()
    });
    Ok(result.join("+"))
}
pub(crate) fn repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = quote.to_string();
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if category_c(c)
                || (matches!(
                    get_general_category(c),
                    G::SpaceSeparator | G::LineSeparator | G::ParagraphSeparator
                ) && c != ' ') =>
            {
                out.push_str(&if c as u32 <= 0xff {
                    format!("\\x{:02x}", c as u32)
                } else if c as u32 <= 0xffff {
                    format!("\\u{:04x}", c as u32)
                } else {
                    format!("\\U{:08x}", c as u32)
                });
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}
pub fn update(config: &Config, key: &str, value: &Value) -> Result<Config> {
    ensure!(ALLOWLIST.contains(&key), "setting is not allowed");
    let (section, field) = field_location(key).ok_or_else(|| anyhow::anyhow!("unknown setting"))?;
    apply_config_update(config, &json!({section: {field: value}}))
}
#[derive(Clone, Debug)]
pub struct Validator {
    pub platform: String,
    pub templates: Option<Vec<String>>,
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
            templates: None,
        }
    }
}
impl Validator {
    pub fn choices(&self, key: &str, config: &Config) -> Vec<String> {
        if key == "meeting_default_template" {
            return self.template_ids(config);
        }
        if key == "paste_method" {
            let methods: &[&str] = match self.platform.as_str() {
                "darwin" => &["auto", "clipboard", "osascript"],
                "win32" => &["auto", "clipboard", "pynput"],
                "linux" => &["auto", "clipboard", "wtype", "xdotool", "pynput"],
                _ => &["auto", "clipboard"],
            };
            return methods.iter().map(|s| (*s).into()).collect();
        }
        lf_core::config::ENUM_VALUES
            .iter()
            .find(|(name, _)| *name == key && (key == "cleanup_level" || key.starts_with("style_")))
            .map(|(_, values)| values.iter().map(|s| (*s).into()).collect())
            .unwrap_or_default()
    }
    pub fn template_ids(&self, config: &Config) -> Vec<String> {
        let discovered;
        let ids = if let Some(ids) = &self.templates {
            ids
        } else {
            let mut ids = vec!["general".into(), "one-on-one".into(), "standup".into()];
            if let Some(dir) = config
                .field_str("meeting_templates_dir")
                .filter(|s| !s.is_empty())
                .and_then(|s| lf_core::home::expand_user(std::path::Path::new(s)).ok())
            {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().is_none_or(|s| {
                            if cfg!(windows) {
                                !s.to_string_lossy().eq_ignore_ascii_case("md")
                            } else {
                                s != "md"
                            }
                        }) || path.metadata().map_or(true, |m| m.len() > 64 * 1024)
                        {
                            continue;
                        }
                        let Ok(text) = std::fs::read_to_string(&path) else {
                            continue;
                        };
                        let text = text.replace("\r\n", "\n");
                        let lines: Vec<_> = text.split('\n').collect();
                        let body = if lines.first().is_some_and(|s| s.trim() == "---") {
                            lines
                                .iter()
                                .skip(1)
                                .position(|s| s.trim() == "---")
                                .map(|i| &lines[i + 2..])
                                .unwrap_or(&lines)
                        } else {
                            &lines
                        };
                        if body
                            .iter()
                            .any(|s| s.strip_prefix("## ").is_some_and(|s| !s.trim().is_empty()))
                        {
                            ids.push(
                                path.file_stem()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_lowercase(),
                            );
                        }
                    }
                }
            }
            ids.sort();
            ids.dedup();
            discovered = ids;
            &discovered
        };
        ids.iter()
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
        ensure!(
            ALLOWLIST.contains(&key),
            "'{}' is not a setting LocalFlow can change here",
            super::safe_text(key, 40)
        );
        if ["dictation_enabled", "dictation_toggle_hotkey"].contains(&key) {
            ensure!(
                config.field(key).is_some(),
                "setting is not supported by this config schema"
            );
        }
        if let Some(s) = value.as_str() {
            ensure!(s.chars().count() <= 64, "value is too long");
            if let Some(problem) = raw_text_problem(s) {
                bail!("value {problem}");
            }
        }
        ensure!(
            value.is_string() || value.is_boolean() || value.is_null(),
            "value must be text or on/off"
        );
        let text = value
            .as_str()
            .unwrap_or("")
            .trim_matches(python_whitespace)
            .to_lowercase();
        let off = value.is_null()
            || (value.is_string() && ["", "off", "none", "disabled"].contains(&text.as_str()));
        let result = if super::BOOL_KEYS.contains(&key) {
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
                ensure!(
                    key != "hotkey",
                    "dictation needs a shortcut; it can't be turned off"
                );
                json!("")
            } else if key == "command_hotkey" || (key == "hotkey" && !text.contains('+')) {
                ensure!(value.is_string(), "needs a key name");
                json!(normalize_hold_key(value.as_str().unwrap(), &self.platform)?)
            } else {
                ensure!(value.is_string(), "needs a shortcut");
                json!(normalize_combo(value.as_str().unwrap(), &self.platform)?)
            }
        } else if key == "paste_method" {
            let allowed = self.choices(key, config);
            ensure!(
                value.is_string() && allowed.contains(&text),
                "on this computer choose one of: {}",
                allowed.join(", ")
            );
            json!(text)
        } else if key == "meeting_default_template" {
            ensure!(value.is_string(), "needs a template id");
            ensure!(
                self.template_ids(config).contains(&text),
                "unknown template; choose one of: {}",
                self.template_ids(config).join(", ")
            );
            json!(text)
        } else {
            let choice = text.replace([' ', '-'], "_");
            let allowed = self.choices(key, config);
            ensure!(
                value.is_string() && allowed.contains(&choice),
                "choose one of: {}",
                allowed.join(", ")
            );
            json!(choice)
        };
        if field_location(key).is_none()
            && ["dictation_enabled", "dictation_toggle_hotkey"].contains(&key)
        {
            ensure!(
                config.field(key).is_some(),
                "setting is not supported by this config schema"
            );
        } else {
            update(config, key, &result)?;
        }
        Ok(result)
    }
    pub fn checked_current(&self, key: &str, config: &Config) -> Option<Value> {
        let current = config.field(key)?;
        self.value(key, current, config)
            .ok()
            .filter(|v| v == current)
    }
    pub fn conflict_details(&self, config: &Config, changed: &[String]) -> Vec<(String, String)> {
        let mut problems = Vec::new();
        for key in changed.iter().filter(|k| SHORTCUTS.contains(&k.as_str())) {
            let value = config.field_str(key).unwrap_or("");
            let Some(id) = identity(value, &self.platform) else {
                continue;
            };
            for other in SHORTCUTS.iter().filter(|k| **k != key.as_str()) {
                if identity(config.field_str(other).unwrap_or(""), &self.platform).as_ref()
                    == Some(&id)
                {
                    problems.push((
                        key.clone(),
                        format!(
                            "{}: {} is already the {}",
                            super::label(key),
                            super::format_shortcut(value),
                            super::label(other)
                        ),
                    ));
                }
            }
        }
        problems
    }
    pub fn conflicts(&self, config: &Config, changed: &[String]) -> Result<()> {
        let problems = self.conflict_details(config, changed);
        ensure!(
            problems.is_empty(),
            "{}",
            problems
                .iter()
                .map(|(_, r)| r.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        );
        Ok(())
    }
}
fn identity(s: &str, platform: &str) -> Option<String> {
    if s.trim_matches(python_whitespace).is_empty() {
        return None;
    }
    if !s.contains('+') {
        let mut t = s.trim_matches(python_whitespace).to_lowercase();
        if let Some(inner) = t.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
            t = inner.into();
        }
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
        return Some(format!("hold:{t}"));
    }
    let mut parts = Vec::new();
    for part in s.split('+') {
        if part.chars().count() == 1 {
            parts.push(part.to_lowercase());
        } else {
            let name = part
                .strip_prefix('<')
                .and_then(|s| s.strip_suffix('>'))
                .filter(|s| !s.is_empty())?;
            let mut name = name.to_lowercase();
            if platform == "darwin" && name == "alt_gr" {
                name = "alt_r".into();
            }
            parts.push(if MODIFIERS.contains(&name.as_str()) {
                name.strip_suffix("_l")
                    .or_else(|| name.strip_suffix("_r"))
                    .unwrap_or(&name)
                    .into()
            } else {
                name
            });
        }
    }
    parts.sort();
    parts.dedup();
    Some(format!("combo:{}", parts.join("+")))
}
