//! Desktop-free shortcut validation from `settings_agent.py`. Config IO keeps
//! saved spellings intact, as Python does; settings callers normalize a proposed
//! combo and check the merged config for conflicts before saving it.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use anyhow::{bail, Result};

use crate::config::{py_repr, Config};

const SHORTCUT_KEYS: &[&str] = &[
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
const NAMED_KEYS: &[&str] = &[
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
];

fn token(raw: &str) -> String {
    let token = raw
        .trim()
        .to_lowercase()
        .trim_matches(['<', '>'])
        .replace([' ', '-'], "_");
    match token.as_str() {
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
        _ => return token,
    }
    .to_string()
}

/// Python `normalize_combo`, plus the optional-shortcut off spellings used by
/// `Validator.value`. Modifier order is retained; conflict identity uses sets.
pub fn normalize_optional_combo(value: &str, platform: &str) -> Result<String> {
    // Validator checks raw text before stripping, including off spellings.
    if value.chars().count() > 64 {
        bail!("value is too long");
    }
    static RAW_PROBLEM: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?P<control>[\p{C}\p{Zl}\p{Zp}])|(?P<space>[\p{Zs}&&[^ ]])")
            .expect("static regex")
    });
    if let Some(problem) = RAW_PROBLEM.captures(value) {
        if problem.name("control").is_some() {
            bail!("value has control characters or line breaks");
        }
        bail!("value has unusual spaces");
    }
    if matches!(
        value.trim().to_lowercase().as_str(),
        "" | "off" | "none" | "disabled"
    ) {
        return Ok(String::new());
    }
    let quoted = py_repr(&serde_json::json!(value));
    let tokens: Vec<String> = value.split('+').map(token).collect();
    if tokens.iter().any(String::is_empty) {
        bail!("{quoted} is not a valid shortcut");
    }
    if tokens.iter().collect::<BTreeSet<_>>().len() != tokens.len() {
        bail!("{quoted} repeats a key");
    }
    let (modifiers, keys): (Vec<_>, Vec<_>) =
        tokens.iter().partition(|t| MODIFIERS.contains(&t.as_str()));
    if !modifiers
        .iter()
        .any(|m| !matches!(m.as_str(), "shift" | "shift_l" | "shift_r"))
    {
        bail!("{quoted} needs Ctrl, Option/Alt or Cmd so it can't fire while typing");
    }
    if keys.len() != 1 {
        bail!("{quoted} needs exactly one non-modifier key");
    }
    let key = keys[0];
    let key_part = if key.chars().count() == 1 {
        // Python isprintable excludes Unicode Other and Separator categories
        // except ASCII space, which the combo grammar also refuses.
        static UNPRINTABLE: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r"[\p{C}\p{Z}]").expect("static regex"));
        if UNPRINTABLE.is_match(key) || "+<> ".contains(key.as_str()) {
            bail!("{quoted} uses an unsupported key");
        }
        key.to_string()
    } else if NAMED_KEYS.contains(&key.as_str()) || (1..=20).any(|n| key == &format!("f{n}")) {
        format!("<{key}>")
    } else {
        bail!(
            "{quoted} uses an unknown key {}",
            py_repr(&serde_json::json!(key))
        );
    };
    if platform == "darwin" && key == "insert" {
        bail!("{quoted} is not available on this computer");
    }
    let mut parts: Vec<_> = modifiers.iter().map(|m| format!("<{m}>")).collect();
    parts.push(key_part);
    Ok(parts.join("+"))
}

#[derive(PartialEq, Eq)]
enum Identity {
    Hold(String),
    Combo(BTreeSet<String>),
}

fn identity(value: &str, platform: &str) -> Option<Identity> {
    if value.trim().is_empty() {
        return None;
    }
    if !value.contains('+') {
        let mut name = value.trim().to_lowercase();
        if name.starts_with('<') && name.ends_with('>') {
            name = name[1..name.len() - 1].to_string();
        }
        if platform == "darwin" {
            name = match name.as_str() {
                "alt_l" => "alt",
                "ctrl_l" => "ctrl",
                "cmd_l" => "cmd",
                "shift_l" => "shift",
                "alt_gr" => "alt_r",
                _ => &name,
            }
            .to_string();
        }
        return Some(Identity::Hold(name));
    }
    let mut parts = BTreeSet::new();
    for part in value.split('+') {
        let name = if part.chars().count() == 1 {
            part.to_lowercase()
        } else if part.len() > 2 && part.starts_with('<') && part.ends_with('>') {
            let mut name = part[1..part.len() - 1].to_lowercase();
            if platform == "darwin" && name == "alt_gr" {
                name = "alt_r".into();
            }
            match name.as_str() {
                "alt_l" | "alt_r" => "alt",
                "ctrl_l" | "ctrl_r" => "ctrl",
                "cmd_l" | "cmd_r" => "cmd",
                "shift_l" | "shift_r" => "shift",
                _ => &name,
            }
            .to_string()
        } else {
            return None;
        };
        parts.insert(name);
    }
    Some(Identity::Combo(parts))
}

/// `(changed field, conflicting field)` in Python `Validator.conflicts` order.
/// Case, modifier side aliases, and reordered chords compare like KeyboardHub.
/// Hold keys retain their sides except for aliases in Darwin's key backend.
pub fn shortcut_conflicts(
    config: &Config,
    changed: &[&str],
    platform: &str,
) -> Vec<(&'static str, &'static str)> {
    let identities: Vec<_> = SHORTCUT_KEYS
        .iter()
        .map(|key| config.field_str(key).and_then(|v| identity(v, platform)))
        .collect();
    let mut conflicts = Vec::new();
    for key in changed {
        let Some(index) = SHORTCUT_KEYS.iter().position(|k| k == key) else {
            continue;
        };
        if identities[index].is_none() {
            continue;
        }
        for (other, other_identity) in identities.iter().enumerate() {
            if other != index && identities[index] == *other_identity {
                conflicts.push((SHORTCUT_KEYS[index], SHORTCUT_KEYS[other]));
            }
        }
    }
    conflicts
}
