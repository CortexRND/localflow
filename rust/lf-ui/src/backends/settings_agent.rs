//! Settings agent: the data contracts, validation rules and the
//! [`SettingsAgent`] trait the Hub Settings page talks to, plus an in-memory
//! [`FakeSettingsAgent`].
//!
//! Port of the pure parts of `localflow/settings_agent.py`: the allowlist,
//! labels, [`Change`]/[`Proposal`]/[`ApplyResult`]/[`UndoResult`]/
//! [`ManualSetting`], `safe_text`, display formatting, shortcut/value
//! validation ([`Validator`]), the model tool-call parser and change
//! derivation ([`parse_call`], [`changes_from_calls`]), the deterministic
//! warnings ([`warnings_for`]) and the apply/undo planning ([`replan`],
//! [`undo_plan`]). All of it is deterministic and does no I/O.
//!
//! Not ported (belongs to a later, real implementation of the trait): the
//! Ollama HTTP client and system prompt, reading/writing the config file,
//! dictionary file and the JSON undo record on disk, and the built-in
//! meeting-template loader (the [`Validator`] takes the template-id source as
//! a closure instead).

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use lf_core::config::{apply_config_update, field_location, Config};
use lf_providers::pyfmt::{py_isspace, py_repr, py_strip};
use regex::Regex;
use serde_json::{json, Map, Value};
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;

// ------------------------------------------------------------- constants ---

/// The exact config fields the agent may change.
pub const ALLOWLIST: [&str; 22] = [
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
pub const TOOLS: [&str; 3] = ["set_setting", "add_dictionary_word", "add_replacement"];

pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
pub const MAX_REQUEST_CHARS: usize = 500;
pub const MAX_TOOL_CALLS: usize = 10;
pub const MAX_UNDO_ENTRIES: usize = 12;
pub const MAX_WORD_CHARS: usize = 64;
pub const MAX_REPLACEMENT_CHARS: usize = 100;
/// A `set_setting` value (shortcut, choice, on/off).
pub const MAX_VALUE_CHARS: usize = 64;
/// A whole JSON-encoded arguments string.
pub const MAX_ARGUMENT_CHARS: usize = 4096;
/// One rendered preview/undo/error line.
pub const MAX_DISPLAY_CHARS: usize = 300;
pub const MAX_MODEL_NAME_CHARS: usize = 120;

/// `LABELS[key]` from Python; `None` outside the allowlist.
pub fn label(key: &str) -> Option<&'static str> {
    Some(match key {
        "sounds_enabled" => "Sounds",
        "cleanup_enabled" => "Cleanup",
        "cleanup_level" => "Cleanup level",
        "styles_enabled" => "Writing styles",
        "style_work" => "Work style",
        "style_email" => "Email style",
        "style_personal" => "Personal style",
        "style_other" => "Other apps style",
        "hotkey" => "Dictation shortcut",
        "command_hotkey" => "Command key",
        "polish_hotkey" => "Polish shortcut",
        "prompt_hotkey" => "Prompt shortcut",
        "paste_last_hotkey" => "Paste last shortcut",
        "paste_method" => "Paste method",
        "spoken_symbols" => "Spoken symbols",
        "spoken_punctuation" => "Spoken punctuation",
        "hot_phrases" => "Hot phrases",
        "meeting_watch" => "Meeting detection",
        "meeting_auto_start" => "Auto-start meetings",
        "capture_system_audio" => "Capture system audio",
        "meeting_default_template" => "Default meeting template",
        "work_prompts" => "Work prompts",
        _ => return None,
    })
}

fn label_of(key: &str) -> &str {
    label(key).unwrap_or(key)
}

/// `NOTES[key]`: when a change is felt (`""` for "immediately").
pub fn note(key: &str) -> &'static str {
    match key {
        "capture_system_audio" | "meeting_default_template" => "From the next meeting",
        _ => "",
    }
}

/// Shown read-only on the Settings page.
pub const CURRENT_KEYS: [&str; 8] = [
    "sounds_enabled",
    "cleanup_enabled",
    "cleanup_level",
    "hotkey",
    "command_hotkey",
    "paste_method",
    "hot_phrases",
    "meeting_watch",
];

/// The manual controls: every allowlist key exactly once, grouped as shown.
pub const MANUAL_GROUPS: [(&str, &[&str]); 4] = [
    (
        "Dictation",
        &[
            "sounds_enabled",
            "paste_method",
            "spoken_symbols",
            "spoken_punctuation",
            "hot_phrases",
        ],
    ),
    (
        "Cleanup & style",
        &[
            "cleanup_enabled",
            "cleanup_level",
            "styles_enabled",
            "style_work",
            "style_email",
            "style_personal",
            "style_other",
        ],
    ),
    (
        "Shortcuts",
        &[
            "hotkey",
            "command_hotkey",
            "polish_hotkey",
            "prompt_hotkey",
            "paste_last_hotkey",
        ],
    ),
    (
        "Meetings",
        &[
            "meeting_watch",
            "meeting_auto_start",
            "capture_system_audio",
            "meeting_default_template",
            "work_prompts",
        ],
    ),
];

pub const BOOL_KEYS: [&str; 10] = [
    "sounds_enabled",
    "cleanup_enabled",
    "styles_enabled",
    "spoken_symbols",
    "spoken_punctuation",
    "hot_phrases",
    "meeting_watch",
    "meeting_auto_start",
    "capture_system_audio",
    "work_prompts",
];
pub const STYLES: [&str; 4] = ["formal", "casual", "very_casual", "excited"];
pub const SHORTCUT_KEYS: [&str; 5] = [
    "hotkey",
    "command_hotkey",
    "polish_hotkey",
    "prompt_hotkey",
    "paste_last_hotkey",
];

fn enum_values(key: &str) -> Option<&'static [&'static str]> {
    match key {
        "cleanup_level" => Some(&["none", "light", "medium"]),
        "style_work" | "style_email" | "style_personal" | "style_other" => Some(&STYLES),
        _ => None,
    }
}

/// What each platform's paste backend really accepts.
pub fn paste_methods_by_platform(platform: &str) -> &'static [&'static str] {
    match platform {
        "darwin" => &["auto", "clipboard", "osascript"],
        "linux" => &["auto", "clipboard", "wtype", "xdotool", "pynput"],
        "win32" => &["auto", "clipboard", "pynput"],
        _ => &["auto", "clipboard"],
    }
}

const TRUE_WORDS: [&str; 6] = ["true", "on", "yes", "enabled", "enable", "1"];
const FALSE_WORDS: [&str; 6] = ["false", "off", "no", "disabled", "disable", "0"];

const MODIFIERS: [&str; 13] = [
    "alt", "alt_l", "alt_r", "alt_gr", "ctrl", "ctrl_l", "ctrl_r", "cmd", "cmd_l", "cmd_r",
    "shift", "shift_l", "shift_r",
];
const SHIFT_ONLY: [&str; 3] = ["shift", "shift_l", "shift_r"];
const NAMED_KEYS: [&str; 14] = [
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

fn is_function_key(name: &str) -> bool {
    name.strip_prefix('f')
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|n| (1..=20).contains(&n) && n.to_string() == name[1..])
}

/// Keys that can be held (dictation hold key, command key).
pub fn is_hold_key(name: &str) -> bool {
    MODIFIERS.contains(&name) || is_function_key(name)
}

fn is_combo_named(name: &str) -> bool {
    NAMED_KEYS.contains(&name) || is_function_key(name)
}

fn token_alias(token: &str) -> &str {
    match token {
        "option" | "opt" => "alt",
        "command" => "cmd",
        "control" | "ctl" => "ctrl",
        "right_option" => "alt_r",
        "left_option" => "alt_l",
        "right_command" | "right_cmd" => "cmd_r",
        "left_command" | "left_cmd" => "cmd_l",
        "right_alt" => "alt_r",
        "left_alt" => "alt_l",
        "right_ctrl" | "right_control" => "ctrl_r",
        "left_ctrl" | "left_control" => "ctrl_l",
        "right_shift" => "shift_r",
        "left_shift" => "shift_l",
        other => other,
    }
}

// ----------------------------------------------------------------- errors ---

/// A user-facing failure; the message is safe to show as is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsAgentError(pub String);

impl fmt::Display for SettingsAgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SettingsAgentError {}

/// One rejected setting value or tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingError(pub String);

impl fmt::Display for SettingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SettingError {}

fn serr<T>(message: impl Into<String>) -> Result<T, SettingError> {
    Err(SettingError(message.into()))
}

fn agent_err(message: impl Into<String>) -> SettingsAgentError {
    SettingsAgentError(message.into())
}

// ------------------------------------------------------------- data types ---

pub const KIND_SETTING: &str = "setting";
pub const KIND_WORD: &str = "word";
pub const KIND_REPLACEMENT: &str = "replacement";

/// One previewed change. `label`, `old` and `new` are for display only.
///
/// `kind` is `"setting"`, `"word"` or `"replacement"`. For settings `key` is
/// the config field and `before`/`value` are the raw old/new values; for a
/// word `value` is the word (a JSON string); for a replacement `value` is a
/// two-element JSON array `[spoken, written]` (Python's tuple).
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub label: String,
    pub old: String,
    pub new: String,
    pub kind: String,
    pub key: String,
    pub before: Value,
    pub value: Value,
    pub note: String,
}

impl Change {
    /// The display line, rebuilt from kind/key/value through the same
    /// validation as apply; `label`, `old`, `new` and `note` are never shown
    /// as given, so a forged or stored `Change` can't spoof the preview.
    pub fn line(&self) -> String {
        change_line(self)
    }

    /// A caution to show beside this change, or `""`. See [`warnings_for`].
    pub fn warning(&self, request: &str) -> String {
        change_warning(self, request)
    }

    /// `(spoken, written)` of a replacement change.
    pub fn replacement_pair(&self) -> Option<(&str, &str)> {
        match &self.value {
            Value::Array(items) if items.len() == 2 => {
                Some((items[0].as_str()?, items[1].as_str()?))
            }
            _ => None,
        }
    }
}

/// One manual control on the Settings page.
#[derive(Clone, Debug, PartialEq)]
pub struct ManualSetting {
    pub key: String,
    pub group: String,
    pub label: String,
    /// `"switch"`, `"choice"` or `"shortcut"`.
    pub kind: String,
    /// Validated current value; `None` when invalid.
    pub value: Option<Value>,
    pub valid: bool,
    pub display: String,
    /// `(value, title)` pairs of a choice control.
    pub choices: Vec<(String, String)>,
}

/// `"switch"`, `"shortcut"` or `"choice"`.
pub fn control_kind(key: &str) -> &'static str {
    if BOOL_KEYS.contains(&key) {
        "switch"
    } else if SHORTCUT_KEYS.contains(&key) {
        "shortcut"
    } else {
        "choice"
    }
}

/// The previewed changes plus the model used and the rejected calls.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub request: String,
    pub model: String,
    pub changes: Vec<Change>,
    pub rejected: Vec<String>,
}

impl Proposal {
    pub fn new(request: &str, model: &str, changes: Vec<Change>, rejected: Vec<String>) -> Self {
        Proposal {
            request: request.to_string(),
            model: model.to_string(),
            changes,
            rejected,
        }
    }

    /// Computed (never stored) so it can't be forged; see [`warnings_for`].
    pub fn warnings(&self) -> Vec<String> {
        warnings_for(&self.changes, &self.request)
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApplyResult {
    pub applied: Vec<Change>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoResult {
    pub reverted: Vec<String>,
    pub skipped: Vec<String>,
    pub message: String,
}

// ---------------------------------------------------------- python helpers ---

fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(value)` for JSON-shaped values.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn nfc(text: &str) -> String {
    text.nfc().collect()
}

/// Python `str.capitalize()` on an ASCII-or-Unicode string.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(|c| c.to_lowercase()))
            .collect(),
        None => String::new(),
    }
}

fn casefold(text: &str) -> String {
    caseless::default_case_fold_str(text)
}

fn lower(text: &str) -> String {
    text.to_lowercase()
}

fn char_prefix(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Two-letter Unicode general category (`unicodedata.category`).
pub fn category(ch: char) -> &'static str {
    use GeneralCategory::*;
    match get_general_category(ch) {
        UppercaseLetter => "Lu",
        LowercaseLetter => "Ll",
        TitlecaseLetter => "Lt",
        ModifierLetter => "Lm",
        OtherLetter => "Lo",
        NonspacingMark => "Mn",
        SpacingMark => "Mc",
        EnclosingMark => "Me",
        DecimalNumber => "Nd",
        LetterNumber => "Nl",
        OtherNumber => "No",
        ConnectorPunctuation => "Pc",
        DashPunctuation => "Pd",
        OpenPunctuation => "Ps",
        ClosePunctuation => "Pe",
        InitialPunctuation => "Pi",
        FinalPunctuation => "Pf",
        OtherPunctuation => "Po",
        MathSymbol => "Sm",
        CurrencySymbol => "Sc",
        ModifierSymbol => "Sk",
        OtherSymbol => "So",
        SpaceSeparator => "Zs",
        LineSeparator => "Zl",
        ParagraphSeparator => "Zp",
        Control => "Cc",
        Format => "Cf",
        Surrogate => "Cs",
        PrivateUse => "Co",
        Unassigned => "Cn",
        // The enum is non-exhaustive; treat anything new as unassigned.
        #[allow(unreachable_patterns)]
        _ => "Cn",
    }
}

// ---------------------------------------------------------------- display ---

const DISPLAY_PUNCTUATION: &str = " .,;:!?'\"()[]-_+/&@#%*=<>|~`$€£¥^";
const DISPLAY_TYPOGRAPHY: &str = "“”‘’—–…·";

fn escape_char(ch: char) -> String {
    let code = ch as u32;
    if code <= 0xFFFF {
        format!("\\u{code:04x}")
    } else {
        format!("\\U{code:08x}")
    }
}

fn display_char_ok(ch: char) -> bool {
    if DISPLAY_PUNCTUATION.contains(ch) || DISPLAY_TYPOGRAPHY.contains(ch) {
        return true;
    }
    let cat = category(ch);
    matches!(cat.as_bytes()[0], b'L' | b'N') || cat == "Mn" || cat == "Mc"
}

/// One display line from untrusted text: escaped, single-line, bounded.
///
/// NFC-normalised; every character that is not a letter, number, mark or
/// ordinary punctuation is shown as a visible `\uXXXX` escape; the result is
/// at most `limit` characters long, ending in `…` when cut. The arrows and
/// curly quotes LocalFlow itself draws are added *around* these pieces, so
/// they can't be forged from inside one.
pub fn safe_text(value: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut size = 0usize;
    for ch in value.nfc() {
        let piece = if display_char_ok(ch) {
            ch.to_string()
        } else {
            escape_char(ch)
        };
        let piece_len = piece.chars().count();
        if size + piece_len > limit {
            out.push('…');
            break;
        }
        out.push_str(&piece);
        size += piece_len;
    }
    out
}

/// [`safe_text`] with the default `MAX_DISPLAY_CHARS` limit.
pub fn safe_text_default(value: &str) -> String {
    safe_text(value, MAX_DISPLAY_CHARS)
}

fn title(value: &str) -> String {
    capitalize(&value.replace('_', " "))
}

fn shortcut_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "alt" => "Option",
        "alt_l" => "Left Option",
        "alt_r" => "Right Option",
        "alt_gr" => "AltGr",
        "ctrl" => "Ctrl",
        "ctrl_l" => "Left Ctrl",
        "ctrl_r" => "Right Ctrl",
        "cmd" => "Cmd",
        "cmd_l" => "Left Cmd",
        "cmd_r" => "Right Cmd",
        "shift" => "Shift",
        "shift_l" => "Left Shift",
        "shift_r" => "Right Shift",
        _ => return None,
    })
}

/// A saved shortcut as shown ("Hold Right Option", "Ctrl+Option+1", "Off").
pub fn format_shortcut(value: &str) -> String {
    safe_text(&format_shortcut_raw(value), 80)
}

fn format_shortcut_raw(value: &str) -> String {
    if value.is_empty() {
        return "Off".to_string();
    }
    let mut parts = Vec::new();
    for token in value.split('+') {
        let name = token.trim_matches(|c| c == '<' || c == '>');
        if let Some(shown) = shortcut_name(name) {
            parts.push(shown.to_string());
        } else if name.chars().count() == 1 {
            parts.push(name.to_uppercase());
        } else {
            // Function keys ("f13") come out as "F13" through `title` too.
            parts.push(title(name));
        }
    }
    let text = parts.join("+");
    if !value.contains('+') && is_hold_key(value) {
        return format!("Hold {text}");
    }
    text
}

fn editable_name(name: &str) -> &str {
    match name {
        "alt" => "option",
        "alt_l" => "left_option",
        "alt_r" => "right_option",
        "cmd" => "command",
        "cmd_l" => "left_command",
        "cmd_r" => "right_command",
        "ctrl_l" => "left_ctrl",
        "ctrl_r" => "right_ctrl",
        "shift_l" => "left_shift",
        "shift_r" => "right_shift",
        other => other,
    }
}

/// A saved shortcut as text a person can edit and the validator reads back
/// to the same value (`"<ctrl>+<alt>+1"` -> `"ctrl+option+1"`, `"alt_r"` ->
/// `"right_option"`); `""` for off. Escaped like any display text.
pub fn editable_shortcut(value: &Value) -> String {
    let Some(text) = value.as_str().filter(|s| !s.is_empty()) else {
        return String::new();
    };
    let parts: Vec<String> = text
        .split('+')
        .map(|token| {
            let chars: Vec<char> = token.chars().collect();
            let name: String =
                if chars.len() > 2 && chars[0] == '<' && chars[chars.len() - 1] == '>' {
                    chars[1..chars.len() - 1].iter().collect()
                } else {
                    token.to_string()
                };
            editable_name(&name).to_string()
        })
        .collect();
    safe_text(&parts.join("+"), MAX_VALUE_CHARS)
}

/// A value as the page shows it; stored junk is escaped, never trusted.
pub fn display_value(key: &str, value: &Value) -> String {
    if BOOL_KEYS.contains(&key) {
        return if py_truthy(value) { "On" } else { "Off" }.to_string();
    }
    if SHORTCUT_KEYS.contains(&key) {
        let text = if py_truthy(value) {
            py_str(value)
        } else {
            String::new()
        };
        return format_shortcut(&text);
    }
    if key == "paste_method" {
        let text = py_str(value);
        let shown = match text.as_str() {
            "auto" => "Automatic".to_string(),
            "osascript" => "AppleScript".to_string(),
            other => title(other),
        };
        return safe_text(&shown, 80);
    }
    if value.is_null() || value.as_str() == Some("") {
        return "—".to_string();
    }
    safe_text(&title(&py_str(value)), 80)
}

// ------------------------------------------------------------ change lines ---

pub const INVALID_LINE: &str = "A change that isn't valid (preview again)";

/// The display line of a change, rebuilt from validated parts.
pub fn change_line(change: &Change) -> String {
    let invalid = || INVALID_LINE.to_string();
    match change.kind.as_str() {
        KIND_SETTING => {
            let key = change.key.as_str();
            let Some(name) = label(key).filter(|_| ALLOWLIST.contains(&key)) else {
                return invalid();
            };
            let text = format!(
                "{name}: {} → {}",
                display_value(key, &change.before),
                display_value(key, &change.value)
            );
            let extra = note(key);
            if extra.is_empty() {
                text
            } else {
                format!("{text} ({extra})")
            }
        }
        KIND_WORD => match word_change(&change.value) {
            Ok(checked) => {
                let word = checked.value.as_str().unwrap_or("");
                format!("Dictionary: — → add “{}”", safe_text_default(word))
            }
            Err(_) => invalid(),
        },
        KIND_REPLACEMENT => {
            let Some((spoken, written)) = change.replacement_pair() else {
                return invalid();
            };
            match replacement_change(&json!(spoken), &json!(written)) {
                Ok(checked) => match checked.replacement_pair() {
                    Some((source, target)) => format!(
                        "Replacement: “{}” → “{}”",
                        safe_text_default(source),
                        safe_text_default(target)
                    ),
                    None => invalid(),
                },
                Err(_) => invalid(),
            }
        }
        _ => invalid(),
    }
}

/// Words a request can use to mean a setting (besides its own label/key).
fn intent_words(key: &str) -> &'static [&'static str] {
    match key {
        "sounds_enabled" => &["sound", "beep", "chime", "audio cue"],
        "cleanup_enabled" | "cleanup_level" => &["cleanup", "clean up", "clean-up"],
        "styles_enabled" => &["writing style", "styles"],
        "style_work" => &[
            "work style",
            "work message",
            "work app",
            "work tone",
            "for work",
            "at work",
        ],
        "style_email" => &["email", "e-mail"],
        "style_personal" => &["personal"],
        "style_other" => &["other app", "other style", "everything else"],
        "hotkey" => &["dictat", "push to talk", "push-to-talk"],
        "command_hotkey" => &["command key", "command mode", "command shortcut"],
        "polish_hotkey" => &["polish"],
        "prompt_hotkey" => &["prompt shortcut", "prompt key", "prompt hotkey"],
        "paste_last_hotkey" => &["paste last", "paste the last", "paste-last"],
        "paste_method" => &["paste method", "pasting", "paste with", "paste using"],
        "spoken_symbols" => &["symbol"],
        "spoken_punctuation" => &["punctuation"],
        "hot_phrases" => &["hot phrase", "hot-phrase", "hotphrase"],
        "meeting_watch" => &[
            "meeting detection",
            "detect meeting",
            "detecting meeting",
            "watch for meeting",
        ],
        "meeting_auto_start" => &[
            "auto-start",
            "auto start",
            "autostart",
            "automatically start",
            "start meetings automatically",
            "start automatically",
        ],
        "capture_system_audio" => &["system audio", "capture audio", "other participants"],
        "meeting_default_template" => &["template"],
        "work_prompts" => &["work prompt"],
        _ => &[],
    }
}

fn all_shortcuts_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(?:all|every|each)\s+(?:of\s+)?(?:the\s+|my\s+)?(?:keyboard\s+)?(?:hot\s?keys?|shortcuts?|key\s+bindings?)\b",
        )
        .expect("static regex")
    })
}

fn is_word_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// `(?<!\w)phrase(?!\w)` search of the case-folded, space-normalised phrase.
fn phrase_in(phrase: &str, text: &str) -> bool {
    let wanted = casefold(phrase)
        .split(py_isspace)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if wanted.is_empty() {
        return false;
    }
    let mut from = 0usize;
    while let Some(found) = text[from..].find(&wanted) {
        let start = from + found;
        let end = start + wanted.len();
        let before_ok = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let after_ok = text[end..].chars().next().is_none_or(|c| !is_word_char(c));
        if before_ok && after_ok {
            return true;
        }
        // Overlapping occurrences count: advance by one character.
        from = start + text[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Deterministic, deliberately narrow check that the request names this
/// change (its label, its key, or a listed synonym). Not semantic
/// understanding: it only keeps the model from adding unrelated changes.
fn mentions(change: &Change, request: &str) -> bool {
    let text = casefold(request)
        .split(py_isspace)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return true; // nothing to compare against (e.g. a manual change)
    }
    match change.kind.as_str() {
        KIND_SETTING => {
            let key = change.key.as_str();
            let Some(name) = label(key).filter(|_| ALLOWLIST.contains(&key)) else {
                return false;
            };
            if SHORTCUT_KEYS.contains(&key) && all_shortcuts_regex().is_match(&text) {
                return true;
            }
            let mut words: Vec<String> = intent_words(key).iter().map(|w| w.to_string()).collect();
            words.push(casefold(name));
            words.push(key.replace('_', " "));
            words.push(key.to_string());
            words.iter().any(|word| text.contains(word.as_str()))
        }
        // The exact word / spoken phrase, as a whole phrase ("no" is not
        // found inside "know").
        KIND_WORD => change
            .value
            .as_str()
            .is_some_and(|word| phrase_in(word, &text)),
        KIND_REPLACEMENT => change
            .replacement_pair()
            .is_some_and(|(spoken, _)| phrase_in(spoken, &text)),
        _ => false,
    }
}

/// Why a valid but unasked-for change was left out (display-safe).
fn unrequested(change: &Change) -> String {
    let what = if change.kind == KIND_SETTING {
        label(&change.key).unwrap_or("That setting").to_string()
    } else if change.kind == KIND_WORD {
        format!(
            "Adding “{}”",
            safe_text(change.value.as_str().unwrap_or(""), 64)
        )
    } else {
        format!(
            "Replacing “{}”",
            safe_text(change.replacement_pair().map_or("", |p| p.0), 64)
        )
    };
    format!("{what} wasn't in your request, so it was left out; ask for it by name")
}

/// Deterministic caution for one previewed change (`""` when none).
///
/// Every shortcut change and every switch turned off is flagged, and so is
/// any change the request doesn't seem to mention. Computed from the
/// change's validated parts only, never from its display fields.
pub fn change_warning(change: &Change, request: &str) -> String {
    if change_line(change) == INVALID_LINE {
        return "Not valid: preview again.".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let is_setting = change.kind == KIND_SETTING;
    if is_setting && SHORTCUT_KEYS.contains(&change.key.as_str()) {
        parts.push("changes a keyboard shortcut".to_string());
    }
    // Conservatively every switch counts as a safety/privacy switch.
    if is_setting && BOOL_KEYS.contains(&change.key.as_str()) && change.value == Value::Bool(false)
    {
        parts.push(format!("turns {} off", label_of(&change.key)));
    }
    if is_setting && change.key == "cleanup_level" && change.value == json!("none") {
        parts.push("turns Cleanup off".to_string()); // level "none" disables cleanup too
    }
    if !mentions(change, request) {
        parts.push("your request doesn't seem to mention this".to_string());
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("Check: {}.", parts.join("; "))
}

/// One warning line per change that needs one, naming the change.
pub fn warnings_for(changes: &[Change], request: &str) -> Vec<String> {
    changes
        .iter()
        .filter_map(|change| {
            let warning = change_warning(change, request);
            (!warning.is_empty()).then(|| format!("{} — {warning}", change_line(change)))
        })
        .collect()
}

// -------------------------------------------------------------- validation ---

const WORD_PUNCTUATION: &str = " -'’‘.,&+/#@_()!?:;\"“”–—·%$€£¥*";

fn word_char_ok(ch: char) -> bool {
    if WORD_PUNCTUATION.contains(ch) {
        return true;
    }
    let cat = category(ch);
    matches!(cat.as_bytes()[0], b'L' | b'N') || cat == "Mn" || cat == "Mc"
}

/// Why a raw (unstripped) tool argument is refused, checked before any strip
/// so a leading/trailing newline or control character can't hide.
fn raw_text_problem(value: &str) -> Option<&'static str> {
    for ch in value.chars() {
        let cat = category(ch);
        if cat.starts_with('C') || cat == "Zl" || cat == "Zp" {
            return Some("has control characters or line breaks");
        }
        if cat == "Zs" && ch != ' ' {
            return Some("has unusual spaces");
        }
    }
    None
}

fn clean_text(value: &Value, limit: usize, what: &str) -> Result<String, SettingError> {
    let Some(text) = value.as_str() else {
        return serr(format!("{what} must be text"));
    };
    if text.chars().count() > 4 * limit {
        return serr(format!("{what} is longer than {limit} characters"));
    }
    if let Some(problem) = raw_text_problem(text) {
        return serr(format!("{what} {problem}"));
    }
    let normalized = nfc(text);
    let stripped = py_strip(&normalized).to_string();
    if stripped.is_empty() {
        return serr(format!("{what} is empty"));
    }
    if stripped.chars().any(|ch| category(ch).starts_with('C')) {
        return serr(format!("{what} has control characters"));
    }
    if stripped.chars().count() > limit {
        return serr(format!("{what} is longer than {limit} characters"));
    }
    if !stripped.chars().all(word_char_ok) {
        return serr(format!(
            "{what} may only use letters, numbers, spaces and ordinary punctuation"
        ));
    }
    Ok(stripped)
}

/// A model-supplied value, quoted, escaped and cut short for a message.
fn quote(value: &Value, limit: usize) -> String {
    match value.as_str() {
        Some(text) => format!("'{}'", safe_text(text, limit)),
        None => py_type_name(value).to_string(),
    }
}

fn token(raw: &str) -> String {
    let cleaned = lower(py_strip(raw))
        .trim_matches(|c| c == '<' || c == '>')
        .replace([' ', '-'], "_");
    token_alias(&cleaned).to_string()
}

fn unsupported_on(platform: &str, name: &str) -> bool {
    platform == "darwin" && name == "insert"
}

/// The platform this build runs on, as Python's `sys.platform` family.
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "win32"
    } else {
        "linux"
    }
}

/// A bare pynput Key name KeyboardHub can hold (modifier or F1-F20).
pub fn normalize_hold_key(value: &Value, platform: &str) -> Result<String, SettingError> {
    let Some(text) = value.as_str().filter(|s| !py_strip(s).is_empty()) else {
        return serr("needs a key name");
    };
    let name = token(text);
    if name.contains('+') {
        return serr("must be a single key to hold, not a combination");
    }
    if !is_hold_key(&name) {
        return serr(format!(
            "{} is not a key you can hold; use a modifier (e.g. alt_r) or a function key (f13)",
            py_repr(text)
        ));
    }
    if unsupported_on(platform, &name) {
        return serr(format!(
            "{} is not available on this computer",
            py_repr(text)
        ));
    }
    Ok(name)
}

/// A combo in pynput `HotKey.parse` grammar: `<mod>+...+<key>` or `+c`.
pub fn normalize_combo(value: &Value, platform: &str) -> Result<String, SettingError> {
    let Some(text) = value.as_str().filter(|s| !py_strip(s).is_empty()) else {
        return serr("needs a shortcut");
    };
    let tokens: Vec<String> = text.split('+').map(token).collect();
    if tokens.iter().any(String::is_empty) {
        return serr(format!("{} is not a valid shortcut", py_repr(text)));
    }
    let unique: HashSet<&String> = tokens.iter().collect();
    if unique.len() != tokens.len() {
        return serr(format!("{} repeats a key", py_repr(text)));
    }
    let modifiers: Vec<&String> = tokens
        .iter()
        .filter(|t| MODIFIERS.contains(&t.as_str()))
        .collect();
    let keys: Vec<&String> = tokens
        .iter()
        .filter(|t| !MODIFIERS.contains(&t.as_str()))
        .collect();
    if !modifiers.iter().any(|t| !SHIFT_ONLY.contains(&t.as_str())) {
        return serr(format!(
            "{} needs Ctrl, Option/Alt or Cmd so it can't fire while typing",
            py_repr(text)
        ));
    }
    if keys.len() != 1 {
        return serr(format!(
            "{} needs exactly one non-modifier key",
            py_repr(text)
        ));
    }
    let key = keys[0];
    let key_part = if key.chars().count() == 1 {
        let ch = key.chars().next().unwrap_or(' ');
        if !is_printable(ch) || "+<> ".contains(ch) {
            return serr(format!("{} uses an unsupported key", py_repr(text)));
        }
        key.clone()
    } else if is_combo_named(key) {
        format!("<{key}>")
    } else {
        return serr(format!(
            "{} uses an unknown key {}",
            py_repr(text),
            py_repr(key)
        ));
    };
    if modifiers.iter().any(|m| unsupported_on(platform, m)) || unsupported_on(platform, key) {
        return serr(format!(
            "{} is not available on this computer",
            py_repr(text)
        ));
    }
    let mut parts: Vec<String> = modifiers.iter().map(|m| format!("<{m}>")).collect();
    parts.push(key_part);
    Ok(parts.join("+"))
}

/// Python `str.isprintable` for one character.
fn is_printable(ch: char) -> bool {
    ch == ' '
        || !matches!(
            category(ch),
            "Cc" | "Cf" | "Cs" | "Co" | "Cn" | "Zl" | "Zp" | "Zs"
        )
}

fn is_off(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => {
            matches!(
                lower(py_strip(s)).as_str(),
                "" | "off" | "none" | "disabled"
            )
        }
        _ => false,
    }
}

fn validate_bool(value: &Value) -> Result<Value, SettingError> {
    if let Value::Bool(b) = value {
        return Ok(Value::Bool(*b));
    }
    if let Some(text) = value.as_str() {
        let word = lower(py_strip(text));
        if TRUE_WORDS.contains(&word.as_str()) {
            return Ok(Value::Bool(true));
        }
        if FALSE_WORDS.contains(&word.as_str()) {
            return Ok(Value::Bool(false));
        }
    }
    serr("must be on or off")
}

fn combo_modifier_fold(name: &str) -> &str {
    match name {
        "alt_l" | "alt_r" => "alt",
        "ctrl_l" | "ctrl_r" => "ctrl",
        "cmd_l" | "cmd_r" => "cmd",
        "shift_l" | "shift_r" => "shift",
        other => other,
    }
}

fn darwin_key_alias(name: &str) -> &str {
    match name {
        "alt_l" => "alt",
        "ctrl_l" => "ctrl",
        "cmd_l" => "cmd",
        "shift_l" => "shift",
        "alt_gr" => "alt_r",
        other => other,
    }
}

/// What KeyboardHub would match for a saved shortcut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShortcutIdentity {
    Hold(String),
    Combo(BTreeSet<String>),
}

/// What KeyboardHub would match for a saved shortcut; `None` if off or
/// unparseable (KeyboardHub disables those, so they can't clash).
///
/// Combos compare as a *set* of keys with letter case folded and left/right
/// modifier variants folded to the generic one (pynput `Listener.canonical`);
/// holds compare the raw key, so sides stay distinct, except names that are
/// the same Key on darwin.
pub fn shortcut_identity(value: &Value, platform: &str) -> Option<ShortcutIdentity> {
    let text = value.as_str().filter(|s| !py_strip(s).is_empty())?;
    if !text.contains('+') {
        let mut name = lower(py_strip(text));
        let chars: Vec<char> = name.chars().collect();
        if chars.first() == Some(&'<') && chars.last() == Some(&'>') {
            // Python: name[1:-1] (a lone "<" would slice to "").
            name = if chars.len() >= 2 {
                chars[1..chars.len() - 1].iter().collect()
            } else {
                String::new()
            };
        }
        if platform == "darwin" {
            name = darwin_key_alias(&name).to_string();
        }
        return Some(ShortcutIdentity::Hold(name));
    }
    let mut parts = BTreeSet::new();
    for part in text.split('+') {
        let chars: Vec<char> = part.chars().collect();
        if chars.len() == 1 {
            parts.insert(lower(part));
        } else if chars.len() > 2 && chars[0] == '<' && chars[chars.len() - 1] == '>' {
            let mut name = lower(&chars[1..chars.len() - 1].iter().collect::<String>());
            if platform == "darwin" && name == "alt_gr" {
                name = "alt_r".to_string(); // same Key as alt_r there
            }
            parts.insert(combo_modifier_fold(&name).to_string());
        } else {
            return None;
        }
    }
    Some(ShortcutIdentity::Combo(parts))
}

/// Source of meeting template ids (the Python default loads them from the
/// templates directory; that loader is not part of this port).
pub type TemplatesFn = Box<dyn Fn(&Config) -> Vec<String> + Send + Sync>;

fn template_id_ok(id: &str) -> bool {
    let chars: Vec<char> = id.chars().collect();
    let ok_first = chars
        .first()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    ok_first
        && chars.len() <= 48
        && chars
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
}

/// Turns raw `(key, value)` pairs into checked config values.
pub struct Validator {
    pub platform: String,
    templates: TemplatesFn,
}

impl Validator {
    /// `platform` is `"darwin"`, `"linux"` or `"win32"` ([`current_platform`]).
    pub fn new(platform: &str, templates: TemplatesFn) -> Self {
        Validator {
            platform: platform.to_string(),
            templates,
        }
    }

    /// A validator for the running OS with a fixed template-id list.
    pub fn with_templates(platform: Option<&str>, ids: &[&str]) -> Self {
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        Validator::new(
            platform.unwrap_or(current_platform()),
            Box::new(move |_| ids.clone()),
        )
    }

    pub fn paste_methods(&self) -> &'static [&'static str] {
        paste_methods_by_platform(&self.platform)
    }

    /// Template ids only (never names or contents), and only those in a plain
    /// charset: a custom template's file name can't carry text into the page.
    pub fn template_ids(&self, config: &Config) -> Vec<String> {
        (self.templates)(config)
            .into_iter()
            .filter(|id| template_id_ok(id))
            .collect()
    }

    /// The checked value for `(key, value)`; see the Python `Validator.value`.
    pub fn value(&self, key: &str, value: &Value, config: &Config) -> Result<Value, SettingError> {
        if !ALLOWLIST.contains(&key) {
            return serr(format!(
                "{} is not a setting LocalFlow can change here",
                quote(&json!(key), 40)
            ));
        }
        match value {
            Value::String(text) => {
                if text.chars().count() > MAX_VALUE_CHARS {
                    return serr("value is too long");
                }
                if let Some(problem) = raw_text_problem(text) {
                    return serr(format!("value {problem}"));
                }
            }
            Value::Null | Value::Bool(_) => {}
            _ => return serr("value must be text or on/off"),
        }
        if BOOL_KEYS.contains(&key) {
            return validate_bool(value);
        }
        if let Some(choices) = enum_values(key) {
            let Some(text) = value.as_str() else {
                return serr(format!("choose one of: {}", choices.join(", ")));
            };
            let choice = lower(py_strip(text)).replace([' ', '-'], "_");
            if !choices.contains(&choice.as_str()) {
                return serr(format!("choose one of: {}", choices.join(", ")));
            }
            return Ok(Value::String(choice));
        }
        match key {
            "paste_method" => {
                let allowed = self.paste_methods();
                let choice = value.as_str().map(|s| lower(py_strip(s)));
                match choice {
                    Some(choice) if allowed.contains(&choice.as_str()) => Ok(Value::String(choice)),
                    _ => serr(format!(
                        "on this computer choose one of: {}",
                        allowed.join(", ")
                    )),
                }
            }
            "meeting_default_template" => {
                let Some(text) = value.as_str() else {
                    return serr("needs a template id");
                };
                let choice = lower(py_strip(text));
                let known = self.template_ids(config);
                if !known.contains(&choice) {
                    return serr(format!(
                        "unknown template; choose one of: {}",
                        known.join(", ")
                    ));
                }
                Ok(Value::String(choice))
            }
            "hotkey" => {
                if is_off(value) {
                    return serr("dictation needs a shortcut; it can't be turned off");
                }
                let text = value.as_str().unwrap_or("");
                if text.contains('+') {
                    normalize_combo(value, &self.platform).map(Value::String)
                } else {
                    normalize_hold_key(value, &self.platform).map(Value::String)
                }
            }
            "command_hotkey" => {
                if is_off(value) {
                    Ok(Value::String(String::new()))
                } else {
                    normalize_hold_key(value, &self.platform).map(Value::String)
                }
            }
            // polish / prompt / paste last: a combo, or off.
            _ => {
                if is_off(value) {
                    Ok(Value::String(String::new()))
                } else {
                    normalize_combo(value, &self.platform).map(Value::String)
                }
            }
        }
    }

    /// The values a choice setting accepts here (empty for others).
    pub fn choices(&self, key: &str, config: &Config) -> Vec<String> {
        if let Some(values) = enum_values(key) {
            return values.iter().map(|s| s.to_string()).collect();
        }
        match key {
            "paste_method" => self.paste_methods().iter().map(|s| s.to_string()).collect(),
            "meeting_default_template" => self.template_ids(config),
            _ => Vec::new(),
        }
    }

    /// The stored value when it passes validation unchanged, else `None`.
    ///
    /// The config file is untrusted too: a hand-edited or tampered value is
    /// never sent to the model or shown as if it were a real choice.
    pub fn checked_current(&self, key: &str, config: &Config) -> Option<Value> {
        let value = config.field(key).cloned().unwrap_or(Value::Null);
        let checked = self.value(key, &value, config).ok()?;
        (checked == value).then_some(checked)
    }

    /// `(key, reason)` for each shortcut clash a `changed` key causes.
    ///
    /// KeyboardHub silently disables the later of two equal shortcuts, so a
    /// clash is refused up front. Equal means the same chord or hold key as
    /// KeyboardHub sees it, not the same spelling.
    pub fn conflicts<S: AsRef<str>>(
        &self,
        config: &Config,
        changed: &[S],
    ) -> Vec<(String, String)> {
        let field = |key: &str| config.field(key).cloned().unwrap_or(Value::Null);
        let identities: Vec<(&str, Option<ShortcutIdentity>)> = SHORTCUT_KEYS
            .iter()
            .map(|k| (*k, shortcut_identity(&field(k), &self.platform)))
            .collect();
        let mut problems = Vec::new();
        for key in changed {
            let key = key.as_ref();
            if !SHORTCUT_KEYS.contains(&key) {
                continue;
            }
            let identity = identities
                .iter()
                .find(|(k, _)| *k == key)
                .and_then(|(_, i)| i.as_ref());
            let Some(identity) = identity else { continue };
            let value = field(key);
            for (other, other_identity) in &identities {
                if *other != key && other_identity.as_ref() == Some(identity) {
                    problems.push((
                        key.to_string(),
                        format!(
                            "{}: {} is already the {}",
                            label_of(key),
                            format_shortcut(value.as_str().unwrap_or("")),
                            label_of(other)
                        ),
                    ));
                }
            }
        }
        problems
    }

    /// One manual change derived from a freshly loaded config, validated like
    /// a model's call. `Ok(None)` when the value is unchanged.
    pub fn setting_change(
        &self,
        key: &str,
        value: &Value,
        config: &Config,
    ) -> Result<Option<Change>, SettingError> {
        if !ALLOWLIST.contains(&key) {
            return serr(format!(
                "{} is not a setting LocalFlow can change here",
                quote(&json!(key), 40)
            ));
        }
        let checked = self
            .value(key, value, config)
            .map_err(|e| SettingError(format!("{}: {}", label_of(key), e.0)))?;
        let before = config.field(key).cloned().unwrap_or(Value::Null);
        if checked == before {
            return Ok(None);
        }
        Ok(Some(setting_change(key, before, checked)))
    }
}

/// A setting change for `key` (display fields rebuilt from the values).
pub fn setting_change(key: &str, before: Value, value: Value) -> Change {
    Change {
        label: label_of(key).to_string(),
        old: display_value(key, &before),
        new: display_value(key, &value),
        kind: KIND_SETTING.to_string(),
        key: key.to_string(),
        before,
        value,
        note: note(key).to_string(),
    }
}

/// A dictionary-word change, validated.
pub fn word_change(word: &Value) -> Result<Change, SettingError> {
    let value = clean_text(word, MAX_WORD_CHARS, "dictionary word")?;
    Ok(Change {
        label: "Dictionary".to_string(),
        old: "—".to_string(),
        new: format!("add “{value}”"),
        kind: KIND_WORD.to_string(),
        key: String::new(),
        before: Value::Null,
        value: Value::String(value),
        note: String::new(),
    })
}

/// A replacement change, validated.
pub fn replacement_change(spoken: &Value, written: &Value) -> Result<Change, SettingError> {
    let source = clean_text(spoken, MAX_REPLACEMENT_CHARS, "spoken phrase")?;
    let target = clean_text(written, MAX_REPLACEMENT_CHARS, "written text")?;
    if casefold(&source) == casefold(&target) {
        return serr("a replacement must change the text");
    }
    Ok(Change {
        label: "Replacement".to_string(),
        old: format!("“{source}”"),
        new: format!("“{target}”"),
        kind: KIND_REPLACEMENT.to_string(),
        key: String::new(),
        before: Value::Null,
        value: json!([source, target]),
        note: String::new(),
    })
}

// ------------------------------------------------------------ tool calls ---

/// The request text, control characters turned into spaces, bounded.
pub fn clean_request(request: &str) -> Result<String, SettingsAgentError> {
    let mapped: String = request
        .chars()
        .map(|ch| {
            if category(ch).starts_with('C') {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let text = py_strip(&mapped).to_string();
    if text.is_empty() {
        return Err(agent_err("Tell LocalFlow what to change."));
    }
    if text.chars().count() > MAX_REQUEST_CHARS {
        return Err(agent_err(format!(
            "Keep the request under {MAX_REQUEST_CHARS} characters."
        )));
    }
    Ok(text)
}

const CALL_KEYS: [&str; 4] = ["function", "id", "type", "index"];
const FUNCTION_ENVELOPE_KEYS: [&str; 3] = ["name", "arguments", "index"];

/// A parsed, shape-checked tool call: `(tool name, arguments)`.
pub fn parse_call(call: &Value) -> Result<(String, Map<String, Value>), SettingError> {
    let function = call.get("function").and_then(Value::as_object);
    let (Some(function), Some(call_obj)) = (function, call.as_object()) else {
        return serr("the model sent a malformed tool call");
    };
    if !call_obj.keys().all(|k| CALL_KEYS.contains(&k.as_str()))
        || !function
            .keys()
            .all(|k| FUNCTION_ENVELOPE_KEYS.contains(&k.as_str()))
    {
        return serr("the model sent a tool call with unexpected fields");
    }
    let name_value = function.get("name").cloned().unwrap_or(Value::Null);
    let name = match name_value.as_str() {
        Some(n) if TOOLS.contains(&n) => n.to_string(),
        other => {
            let shown = if other.is_some() {
                quote(&name_value, 40)
            } else {
                "?".to_string()
            };
            return serr(format!("the model asked for an unknown tool {shown}"));
        }
    };
    let raw_args = function
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let args = match raw_args {
        Value::String(text) => {
            if text.chars().count() > MAX_ARGUMENT_CHARS {
                return serr("the model sent oversized tool arguments");
            }
            match serde_json::from_str::<Value>(&text) {
                Ok(parsed) => parsed,
                Err(_) => return serr(format!("the model sent malformed arguments for {name}")),
            }
        }
        other => other,
    };
    let Value::Object(args) = args else {
        return serr(format!("the model sent malformed arguments for {name}"));
    };
    // Each tool's exact arguments and the types each may have.
    let expected: &[(&str, bool)] = match name.as_str() {
        "set_setting" => &[("key", false), ("value", true)], // (arg, may also be bool)
        "add_dictionary_word" => &[("word", false)],
        _ => &[("spoken", false), ("written", false)],
    };
    let names_match =
        args.len() == expected.len() && expected.iter().all(|(arg, _)| args.contains_key(*arg));
    if !names_match {
        return serr(format!("the model sent the wrong arguments for {name}"));
    }
    for (arg, value) in &args {
        let allows_bool = expected.iter().any(|(a, b)| a == arg && *b);
        let type_ok = value.is_string() || (allows_bool && value.is_boolean());
        if !type_ok {
            return serr(format!(
                "the model sent a {arg} of the wrong type for {name}"
            ));
        }
        if let Some(text) = value.as_str() {
            if text.chars().count() > MAX_REPLACEMENT_CHARS * 2 {
                return serr(format!("the model sent an overlong {arg} for {name}"));
            }
            if let Some(problem) = raw_text_problem(text) {
                return serr(format!("the {arg} for {name} {problem}"));
            }
        }
    }
    Ok((name, args))
}

/// Config with `values` (flat field -> value) applied through the shared
/// config validation (`apply_config_update`).
pub fn apply_settings(
    config: &Config,
    values: &[(String, Value)],
) -> Result<Config, SettingsAgentError> {
    let mut update = Map::new();
    for (key, value) in values {
        let Some((section, file_key)) = field_location(key) else {
            return Err(agent_err(format!(
                "{} isn't a known setting.",
                label_of(key)
            )));
        };
        let entry = update
            .entry(section.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(table) = entry {
            table.insert(file_key.to_string(), value.clone());
        }
    }
    apply_config_update(config, &Value::Object(update))
        .map_err(|e| agent_err(safe_text_default(&e.to_string())))
}

fn merged(config: &Config, changes: &[&Change]) -> Result<Config, SettingsAgentError> {
    let values: Vec<(String, Value)> = changes
        .iter()
        .map(|c| (c.key.clone(), c.value.clone()))
        .collect();
    apply_settings(config, &values)
}

/// Dictionary entries the derivation checks duplicates against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DictionaryView {
    pub words: Vec<String>,
    /// `(spoken, written)` pairs.
    pub replacements: Vec<(String, String)>,
}

/// Validated changes and rejection reasons for the model's calls.
///
/// A valid change the request doesn't name is refused, not just flagged: the
/// model may only do what was plainly asked. Shortcut clashes are judged on
/// the whole batch.
pub fn changes_from_calls(
    validator: &Validator,
    calls: &[Value],
    config: &Config,
    request: &str,
    dictionary: &DictionaryView,
) -> Result<(Vec<Change>, Vec<String>), SettingsAgentError> {
    let mut changes: Vec<Change> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut seen_words: HashSet<String> = HashSet::new();
    let mut seen_spoken: HashSet<String> = HashSet::new();
    let words: HashSet<String> = dictionary.words.iter().map(|w| casefold(w)).collect();
    let spoken: HashSet<String> = dictionary
        .replacements
        .iter()
        .map(|(s, _)| casefold(s))
        .collect();

    for call in calls {
        let step: Result<Option<Change>, SettingError> = (|| {
            let (name, args) = parse_call(call)?;
            match name.as_str() {
                "set_setting" => {
                    let key = args.get("key").and_then(Value::as_str).unwrap_or("");
                    if seen_keys.contains(key) {
                        return serr(format!("{key} was set twice; keeping the first"));
                    }
                    let value = args.get("value").cloned().unwrap_or(Value::Null);
                    let change = validator.setting_change(key, &value, config)?;
                    seen_keys.insert(key.to_string());
                    match change {
                        Some(change) => Ok(Some(change)),
                        None => {
                            let current = config.field(key).cloned().unwrap_or(Value::Null);
                            rejected.push(format!(
                                "{} is already {}",
                                label_of(key),
                                display_value(key, &current)
                            ));
                            Ok(None)
                        }
                    }
                }
                "add_dictionary_word" => {
                    let change = word_change(args.get("word").unwrap_or(&Value::Null))?;
                    let word = change.value.as_str().unwrap_or("").to_string();
                    let folded = casefold(&word);
                    if words.contains(&folded) {
                        rejected.push(format!("“{word}” is already in your dictionary"));
                        return Ok(None);
                    }
                    if !seen_words.insert(folded) {
                        return Ok(None);
                    }
                    Ok(Some(change))
                }
                _ => {
                    let change = replacement_change(
                        args.get("spoken").unwrap_or(&Value::Null),
                        args.get("written").unwrap_or(&Value::Null),
                    )?;
                    let source = change
                        .replacement_pair()
                        .map(|p| p.0.to_string())
                        .unwrap_or_default();
                    let folded = casefold(&source);
                    if spoken.contains(&folded) || seen_spoken.contains(&folded) {
                        rejected.push(format!("“{source}” already has a replacement"));
                        return Ok(None);
                    }
                    seen_spoken.insert(folded);
                    Ok(Some(change))
                }
            }
        })();
        let change = match step {
            Ok(Some(change)) => change,
            Ok(None) => continue,
            Err(error) => {
                rejected.push(error.0);
                continue;
            }
        };
        if !request.is_empty() && !mentions(&change, request) {
            rejected.push(unrequested(&change));
            continue;
        }
        changes.push(change);
    }

    let settings: Vec<&Change> = changes.iter().filter(|c| c.kind == KIND_SETTING).collect();
    if !settings.is_empty() {
        let merged_config = merged(config, &settings)?;
        let keys: Vec<String> = settings.iter().map(|c| c.key.clone()).collect();
        let clashes = validator.conflicts(&merged_config, &keys);
        if !clashes.is_empty() {
            let bad: HashSet<String> = clashes.iter().map(|(k, _)| k.clone()).collect();
            rejected.extend(clashes.into_iter().map(|(_, reason)| reason));
            changes.retain(|c| c.kind != KIND_SETTING || !bad.contains(&c.key));
        }
    }
    Ok((changes, rejected))
}

/// What an apply will write, re-derived from the changes' raw parts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// `(key, before, after)` in batch order.
    pub settings: Vec<(String, Value, Value)>,
    pub words: Vec<String>,
    pub replacements: Vec<(String, String)>,
}

/// Re-derive every change from its raw parts; reject tampering/staleness.
pub fn replan(
    validator: &Validator,
    changes: &[Change],
    config: &Config,
    dictionary: &DictionaryView,
) -> Result<Plan, SettingsAgentError> {
    if changes.is_empty() {
        return Err(agent_err("There's nothing to apply."));
    }
    if changes.len() > MAX_TOOL_CALLS {
        return Err(agent_err("Too many changes in one batch."));
    }
    let invalid = || agent_err("This preview isn't valid; preview again.");
    let mut plan = Plan::default();
    let mut words: HashSet<String> = dictionary.words.iter().map(|w| casefold(w)).collect();
    let mut spoken: HashSet<String> = dictionary
        .replacements
        .iter()
        .map(|(s, _)| casefold(s))
        .collect();
    let mut problems: Vec<String> = Vec::new();
    for change in changes {
        match change.kind.as_str() {
            KIND_SETTING => {
                let key = change.key.as_str();
                if !ALLOWLIST.contains(&key) || plan.settings.iter().any(|(k, _, _)| k == key) {
                    return Err(invalid());
                }
                let checked = validator
                    .value(key, &change.value, config)
                    .map_err(|e| agent_err(e.0))?;
                if setting_change(key, change.before.clone(), checked.clone()) != *change {
                    return Err(invalid());
                }
                let current = config.field(key).cloned().unwrap_or(Value::Null);
                if current != change.before {
                    problems.push(format!(
                        "{} changed to {} since the preview",
                        change.label,
                        display_value(key, &current)
                    ));
                    continue;
                }
                plan.settings
                    .push((key.to_string(), change.before.clone(), checked));
            }
            KIND_WORD => {
                let expected = word_change(&change.value).map_err(|e| agent_err(e.0))?;
                if expected != *change {
                    return Err(invalid());
                }
                let word = change.value.as_str().unwrap_or("").to_string();
                if words.contains(&casefold(&word)) {
                    problems.push(format!("“{word}” is already in your dictionary"));
                    continue;
                }
                words.insert(casefold(&word));
                plan.words.push(word);
            }
            KIND_REPLACEMENT => {
                let Some((source, target)) = change.replacement_pair() else {
                    return Err(invalid());
                };
                let expected = replacement_change(&json!(source), &json!(target))
                    .map_err(|e| agent_err(e.0))?;
                if expected != *change {
                    return Err(invalid());
                }
                if spoken.contains(&casefold(source)) {
                    problems.push(format!("“{source}” already has a replacement"));
                    continue;
                }
                spoken.insert(casefold(source));
                plan.replacements
                    .push((source.to_string(), target.to_string()));
            }
            _ => return Err(invalid()),
        }
    }
    if !problems.is_empty() {
        return Err(agent_err(format!(
            "Nothing was changed: {}. Preview again.",
            problems.join("; ")
        )));
    }
    if !plan.settings.is_empty() {
        let values: Vec<(String, Value)> = plan
            .settings
            .iter()
            .map(|(k, _, after)| (k.clone(), after.clone()))
            .collect();
        let merged_config = apply_settings(config, &values)?;
        let keys: Vec<&str> = plan.settings.iter().map(|(k, _, _)| k.as_str()).collect();
        let clashes = validator.conflicts(&merged_config, &keys);
        if !clashes.is_empty() {
            let reasons: Vec<String> = clashes.into_iter().map(|(_, r)| r).collect();
            return Err(agent_err(format!(
                "Nothing was changed: {}.",
                reasons.join("; ")
            )));
        }
    }
    Ok(plan)
}

// ----------------------------------------------------------------- undo ---

/// The one-batch undo record (typed; the real agent stores it as JSON).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UndoRecord {
    /// `(key, before, after)`.
    pub settings: Vec<(String, Value, Value)>,
    pub words: Vec<String>,
    pub replacements: Vec<(String, String)>,
}

impl UndoRecord {
    pub fn is_empty(&self) -> bool {
        self.settings.is_empty() && self.words.is_empty() && self.replacements.is_empty()
    }

    /// Display lines for the recorded batch.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .settings
            .iter()
            .map(|(key, before, after)| {
                format!(
                    "{}: {} → {}",
                    label_of(key),
                    display_value(key, after),
                    display_value(key, before)
                )
            })
            .collect();
        lines.extend(
            self.words
                .iter()
                .map(|w| format!("Remove word “{}”", safe_text_default(w))),
        );
        lines.extend(self.replacements.iter().map(|(s, w)| {
            format!(
                "Remove replacement “{}” → “{}”",
                safe_text_default(s),
                safe_text_default(w)
            )
        }));
        lines
    }
}

fn count_message(count: usize) -> String {
    format!(
        "Applied {count} change{}.",
        if count != 1 { "s" } else { "" }
    )
}

// ----------------------------------------------------------------- trait ---

/// What the Hub needs from the Settings agent (the methods
/// `hub_settings_mac.py` calls: `load`, `manual_settings`, `pending_undo`,
/// `propose`, `set_setting`, `apply`, `undo`, and the `model` attribute;
/// `current_settings` backs the read-only current list).
///
/// Methods take `&self` and the trait is `Send + Sync`: previews run on a
/// worker thread while the UI thread keeps the page state.
pub trait SettingsAgent: Send + Sync {
    /// The current config, or "Couldn't read your settings (…)".
    fn load(&self) -> Result<Config, SettingsAgentError>;
    /// `(label, shown value)` for [`CURRENT_KEYS`].
    fn current_settings(
        &self,
        config: Option<&Config>,
    ) -> Result<Vec<(String, String)>, SettingsAgentError>;
    /// Every allowlist setting for the manual controls, grouped and in page
    /// order, with validated current values and choices.
    fn manual_settings(
        &self,
        config: Option<&Config>,
    ) -> Result<Vec<ManualSetting>, SettingsAgentError>;
    /// Display lines for the recorded undo batch, `None` when there is none.
    fn pending_undo(&self) -> Option<Vec<String>>;
    /// Ask the local model; return the previewed changes. Writes nothing.
    fn propose(&self, request: &str) -> Result<Proposal, SettingsAgentError>;
    /// One manual change through the agent's own rules.
    fn set_setting(&self, key: &str, value: &Value) -> Result<ApplyResult, SettingsAgentError>;
    /// Revalidate and write a previewed batch, then record its undo.
    fn apply(&self, changes: &[Change]) -> Result<ApplyResult, SettingsAgentError>;
    /// Revert the last applied batch where nothing newer replaced it.
    fn undo(&self) -> Result<UndoResult, SettingsAgentError>;
    /// The model used by the last `propose` (set even when it failed).
    fn model(&self) -> Option<String>;
}

impl<T: SettingsAgent + ?Sized> SettingsAgent for Arc<T> {
    fn load(&self) -> Result<Config, SettingsAgentError> {
        (**self).load()
    }
    fn current_settings(
        &self,
        config: Option<&Config>,
    ) -> Result<Vec<(String, String)>, SettingsAgentError> {
        (**self).current_settings(config)
    }
    fn manual_settings(
        &self,
        config: Option<&Config>,
    ) -> Result<Vec<ManualSetting>, SettingsAgentError> {
        (**self).manual_settings(config)
    }
    fn pending_undo(&self) -> Option<Vec<String>> {
        (**self).pending_undo()
    }
    fn propose(&self, request: &str) -> Result<Proposal, SettingsAgentError> {
        (**self).propose(request)
    }
    fn set_setting(&self, key: &str, value: &Value) -> Result<ApplyResult, SettingsAgentError> {
        (**self).set_setting(key, value)
    }
    fn apply(&self, changes: &[Change]) -> Result<ApplyResult, SettingsAgentError> {
        (**self).apply(changes)
    }
    fn undo(&self) -> Result<UndoResult, SettingsAgentError> {
        (**self).undo()
    }
    fn model(&self) -> Option<String> {
        (**self).model()
    }
}

/// `settings_agent_factory` of the Hub: a new agent handle per use.
pub type SettingsAgentFactory = Box<dyn Fn() -> Box<dyn SettingsAgent> + Send + Sync>;

/// [`SettingsAgent::current_settings`] from any config.
pub fn current_settings_for(config: &Config) -> Vec<(String, String)> {
    CURRENT_KEYS
        .iter()
        .map(|key| {
            let value = config.field(key).cloned().unwrap_or(Value::Null);
            (label_of(key).to_string(), display_value(key, &value))
        })
        .collect()
}

/// [`SettingsAgent::manual_settings`] from any config and validator.
pub fn manual_settings_for(validator: &Validator, config: &Config) -> Vec<ManualSetting> {
    let mut rows = Vec::new();
    for (group, keys) in MANUAL_GROUPS {
        for key in keys {
            let current = validator.checked_current(key, config);
            let valid = current.is_some();
            let choices = validator
                .choices(key, config)
                .into_iter()
                .map(|choice| {
                    let shown = display_value(key, &Value::String(choice.clone()));
                    (choice, shown)
                })
                .collect();
            let display = match &current {
                Some(value) => display_value(key, value),
                None => "Not set".to_string(),
            };
            rows.push(ManualSetting {
                key: (*key).to_string(),
                group: group.to_string(),
                label: label_of(key).to_string(),
                kind: control_kind(key).to_string(),
                value: current,
                valid,
                display,
                choices,
            });
        }
    }
    rows
}

// ------------------------------------------------------------------ fake ---

/// One scripted answer of the fake model.
#[derive(Clone, Debug)]
pub enum ScriptedAnswer {
    /// Raw tool calls, run through [`parse_call`]/[`changes_from_calls`]
    /// exactly like a real model's.
    Calls { model: String, calls: Vec<Value> },
    /// The model (or Ollama) failed with this user-facing message.
    Error {
        model: Option<String>,
        message: String,
    },
}

#[derive(Default)]
struct FakeState {
    config: Option<Config>,
    load_error: Option<String>,
    save_error: Option<String>,
    words: Vec<String>,
    replacements: Vec<(String, String)>,
    undo: Option<UndoRecord>,
    model: Option<String>,
    script: VecDeque<ScriptedAnswer>,
    log: Vec<String>,
}

/// In-memory [`SettingsAgent`]: the real validation, derivation, apply and
/// undo rules over an in-memory config, dictionary and undo record. Nothing
/// touches the disk, the keychain or the network. The "model" is a script of
/// answers ([`FakeSettingsAgent::script_calls`]); with no script left,
/// `propose` fails with a clear message instead of inventing changes.
pub struct FakeSettingsAgent {
    validator: Validator,
    state: Mutex<FakeState>,
}

impl FakeSettingsAgent {
    /// A fake over the default config, on `platform` (`None` = this OS) with
    /// the given meeting template ids.
    pub fn new(platform: Option<&str>, templates: &[&str]) -> Self {
        FakeSettingsAgent::with_config(Config::default(), platform, templates)
    }

    pub fn with_config(config: Config, platform: Option<&str>, templates: &[&str]) -> Self {
        FakeSettingsAgent {
            validator: Validator::with_templates(platform, templates),
            state: Mutex::new(FakeState {
                config: Some(config),
                ..FakeState::default()
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn validator(&self) -> &Validator {
        &self.validator
    }

    /// Queue a model answer made of raw tool calls.
    pub fn script_calls(&self, model: &str, calls: Vec<Value>) {
        self.state().script.push_back(ScriptedAnswer::Calls {
            model: model.to_string(),
            calls,
        });
    }

    /// Queue a model failure.
    pub fn script_error(&self, model: Option<&str>, message: &str) {
        self.state().script.push_back(ScriptedAnswer::Error {
            model: model.map(str::to_string),
            message: message.to_string(),
        });
    }

    /// Make `load` fail ("Couldn't read your settings (…)"); `None` heals it.
    pub fn set_load_error(&self, message: Option<&str>) {
        self.state().load_error = message.map(str::to_string);
    }

    /// Make the next config writes fail; `None` heals it.
    pub fn set_save_error(&self, message: Option<&str>) {
        self.state().save_error = message.map(str::to_string);
    }

    /// Replace the stored config (as another process editing the file would).
    pub fn set_config(&self, config: Config) {
        self.state().config = Some(config);
    }

    pub fn config(&self) -> Config {
        self.state().config.clone().unwrap_or_default()
    }

    pub fn words(&self) -> Vec<String> {
        self.state().words.clone()
    }

    pub fn replacements(&self) -> Vec<(String, String)> {
        self.state().replacements.clone()
    }

    /// Seed dictionary entries (as if added earlier).
    pub fn seed_dictionary(&self, words: &[&str], replacements: &[(&str, &str)]) {
        let mut state = self.state();
        state.words.extend(words.iter().map(|w| w.to_string()));
        state.replacements.extend(
            replacements
                .iter()
                .map(|(s, w)| (s.to_string(), w.to_string())),
        );
    }

    /// The public entry points used so far (`"set_setting:key"`, `"apply"`, …).
    pub fn calls(&self) -> Vec<String> {
        self.state().log.clone()
    }

    fn dictionary_view(state: &FakeState) -> DictionaryView {
        DictionaryView {
            words: state.words.clone(),
            replacements: state.replacements.clone(),
        }
    }

    fn load_locked(state: &FakeState) -> Result<Config, SettingsAgentError> {
        if let Some(message) = &state.load_error {
            return Err(agent_err(format!(
                "Couldn't read your settings ({message})."
            )));
        }
        Ok(state.config.clone().unwrap_or_default())
    }
}

impl SettingsAgent for FakeSettingsAgent {
    fn load(&self) -> Result<Config, SettingsAgentError> {
        Self::load_locked(&self.state())
    }

    fn current_settings(
        &self,
        config: Option<&Config>,
    ) -> Result<Vec<(String, String)>, SettingsAgentError> {
        let config = match config {
            Some(config) => config.clone(),
            None => self.load()?,
        };
        Ok(current_settings_for(&config))
    }

    fn manual_settings(
        &self,
        config: Option<&Config>,
    ) -> Result<Vec<ManualSetting>, SettingsAgentError> {
        let config = match config {
            Some(config) => config.clone(),
            None => self.load()?,
        };
        Ok(manual_settings_for(&self.validator, &config))
    }

    fn pending_undo(&self) -> Option<Vec<String>> {
        let state = self.state();
        Self::load_locked(&state).ok()?;
        state
            .undo
            .as_ref()
            .filter(|r| !r.is_empty())
            .map(UndoRecord::lines)
    }

    fn propose(&self, request: &str) -> Result<Proposal, SettingsAgentError> {
        let text = clean_request(request)?;
        let mut state = self.state();
        state.log.push("propose".to_string());
        let config = Self::load_locked(&state)?;
        let Some(answer) = state.script.pop_front() else {
            return Err(agent_err(
                "The fake settings agent has no scripted model answer for this request.",
            ));
        };
        let (model, calls) = match answer {
            ScriptedAnswer::Error { model, message } => {
                state.model = model;
                return Err(agent_err(message));
            }
            ScriptedAnswer::Calls { model, calls } => (model, calls),
        };
        state.model = Some(model.clone());
        if calls.is_empty() {
            return Err(agent_err(
                "The local model didn't suggest any setting change. Try naming the setting, e.g. “turn off sounds”.",
            ));
        }
        if calls.len() > MAX_TOOL_CALLS {
            return Err(agent_err(format!(
                "The local model asked for more than {MAX_TOOL_CALLS} changes at once; ask for fewer."
            )));
        }
        let dictionary = Self::dictionary_view(&state);
        let (changes, rejected) =
            changes_from_calls(&self.validator, &calls, &config, &text, &dictionary)?;
        let rejected: Vec<String> = rejected.iter().map(|r| safe_text_default(r)).collect();
        if changes.is_empty() {
            let reasons = if rejected.is_empty() {
                "no usable changes".to_string()
            } else {
                rejected.join("; ")
            };
            return Err(agent_err(format!("Nothing to change: {reasons}")));
        }
        Ok(Proposal {
            request: text,
            model,
            changes,
            rejected,
        })
    }

    fn set_setting(&self, key: &str, value: &Value) -> Result<ApplyResult, SettingsAgentError> {
        let change = {
            let mut state = self.state();
            state.log.push(format!("set_setting:{key}"));
            let config = Self::load_locked(&state)?;
            self.validator
                .setting_change(key, value, &config)
                .map_err(|e| agent_err(safe_text_default(&e.0)))?
        };
        match change {
            None => Ok(ApplyResult {
                applied: Vec::new(),
                message: "Nothing needed changing.".to_string(),
            }),
            Some(change) => self.apply(&[change]),
        }
    }

    fn apply(&self, changes: &[Change]) -> Result<ApplyResult, SettingsAgentError> {
        let mut state = self.state();
        state.log.push("apply".to_string());
        let config = Self::load_locked(&state)?;
        let plan = replan(
            &self.validator,
            changes,
            &config,
            &Self::dictionary_view(&state),
        )?;
        if let Some(message) = &state.save_error {
            if !plan.settings.is_empty() {
                return Err(agent_err(format!(
                    "Nothing was changed: couldn't save ({message})."
                )));
            }
        }
        if !plan.settings.is_empty() {
            let values: Vec<(String, Value)> = plan
                .settings
                .iter()
                .map(|(k, _, after)| (k.clone(), after.clone()))
                .collect();
            state.config = Some(apply_settings(&config, &values)?);
        }
        let mut added_words = Vec::new();
        for word in &plan.words {
            if !state.words.iter().any(|w| casefold(w) == casefold(word)) {
                state.words.push(word.clone());
                added_words.push(word.clone());
            }
        }
        let mut added_replacements = Vec::new();
        for (source, target) in &plan.replacements {
            if !state
                .replacements
                .iter()
                .any(|(s, _)| casefold(s) == casefold(source))
            {
                state.replacements.push((source.clone(), target.clone()));
                added_replacements.push((source.clone(), target.clone()));
            }
        }
        let record = UndoRecord {
            settings: plan.settings.clone(),
            words: added_words.clone(),
            replacements: added_replacements.clone(),
        };
        // Nothing actually written (another writer added the same entries
        // first): keep the previous batch undoable.
        if !record.is_empty() {
            state.undo = Some(record);
        }
        let applied: Vec<Change> = changes
            .iter()
            .filter(|change| match change.kind.as_str() {
                KIND_SETTING => plan.settings.iter().any(|(k, _, _)| *k == change.key),
                KIND_WORD => change
                    .value
                    .as_str()
                    .is_some_and(|w| added_words.iter().any(|a| a == w)),
                KIND_REPLACEMENT => change
                    .replacement_pair()
                    .is_some_and(|(s, t)| added_replacements.iter().any(|(a, b)| a == s && b == t)),
                _ => false,
            })
            .cloned()
            .collect();
        if applied.is_empty() {
            return Ok(ApplyResult {
                applied,
                message: "Nothing needed changing.".to_string(),
            });
        }
        let message = count_message(applied.len());
        Ok(ApplyResult { applied, message })
    }

    fn undo(&self) -> Result<UndoResult, SettingsAgentError> {
        let mut state = self.state();
        state.log.push("undo".to_string());
        let config = Self::load_locked(&state)?;
        let Some(record) = state.undo.clone().filter(|r| !r.is_empty()) else {
            return Err(agent_err("There's no change to undo."));
        };
        let mut reverted: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut revert: Vec<(String, Value)> = Vec::new();
        for (key, before, after) in &record.settings {
            let current = config.field(key).cloned().unwrap_or(Value::Null);
            if current == *after {
                revert.push((key.clone(), before.clone()));
            } else {
                skipped.push(format!(
                    "{} (changed to {} since)",
                    label_of(key),
                    display_value(key, &current)
                ));
            }
        }
        let mut new_config = None;
        if !revert.is_empty() {
            let merged_config = apply_settings(&config, &revert)?;
            let keys: Vec<&str> = revert.iter().map(|(k, _)| k.as_str()).collect();
            let clashes = self.validator.conflicts(&merged_config, &keys);
            if !clashes.is_empty() {
                let reasons: Vec<String> = clashes.into_iter().map(|(_, r)| r).collect();
                return Err(agent_err(format!(
                    "Can't undo without a shortcut clash: {}.",
                    reasons.join("; ")
                )));
            }
            if let Some(message) = &state.save_error {
                return Err(agent_err(format!(
                    "Undo stopped part way ({message}). Already undone: nothing. The undo record was kept."
                )));
            }
            for (key, value) in &revert {
                reverted.push(format!("{}: {}", label_of(key), display_value(key, value)));
            }
            new_config = Some(merged_config);
        }
        if let Some(updated) = new_config {
            state.config = Some(updated);
        }
        for word in &record.words {
            let found = state.words.iter().position(|w| w == word);
            match found {
                Some(index) => {
                    state.words.remove(index);
                    reverted.push(format!("Removed word “{word}”"));
                }
                None => skipped.push(format!("word “{word}” (edited or removed since)")),
            }
        }
        for (source, target) in &record.replacements {
            let found = state
                .replacements
                .iter()
                .position(|(s, t)| s == source && t == target);
            match found {
                Some(index) => {
                    state.replacements.remove(index);
                    reverted.push(format!("Removed replacement “{source}”"));
                }
                None => skipped.push(format!("replacement “{source}” (edited or removed since)")),
            }
        }
        state.undo = None;
        if reverted.is_empty() {
            return Err(agent_err(format!(
                "Nothing to undo: everything was changed again since. {}",
                skipped.join("; ")
            )));
        }
        let mut message = "Undid the last change.".to_string();
        if !skipped.is_empty() {
            message.push_str(&format!(" Left alone: {}.", skipped.join("; ")));
        }
        Ok(UndoResult {
            reverted,
            skipped,
            message,
        })
    }

    fn model(&self) -> Option<String> {
        self.state().model.clone()
    }
}

/// Convenience for tests and callers: a JSON tool call as Ollama sends it.
pub fn tool_call(name: &str, arguments: Value) -> Value {
    json!({"function": {"name": name, "arguments": arguments}})
}

#[allow(dead_code)]
fn truncate_chars(text: &str, limit: usize) -> String {
    char_prefix(text, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn darwin() -> Validator {
        Validator::with_templates(Some("darwin"), &["general", "standup"])
    }

    fn base() -> Config {
        let mut config = Config::defaults_for(true);
        config.set_field("hotkey", json!("alt_l"));
        config.set_field("command_hotkey", json!("alt_r"));
        config.set_field("polish_hotkey", json!("<ctrl>+<alt>+1"));
        config.set_field("prompt_hotkey", json!("<ctrl>+<alt>+2"));
        config.set_field("paste_last_hotkey", json!("<ctrl>+<shift>+v"));
        config
    }

    fn with(mut config: Config, pairs: &[(&str, &str)]) -> Config {
        for (k, v) in pairs {
            config.set_field(k, json!(v));
        }
        config
    }

    fn clash_keys(v: &Validator, config: &Config, changed: &[&str]) -> BTreeSet<String> {
        v.conflicts(config, changed)
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    // ---- safe_text / display -------------------------------------------

    #[test]
    fn safe_text_escapes_controls_bidi_and_arrows() {
        assert_eq!(safe_text_default("a\nb"), "a\\u000ab");
        assert_eq!(safe_text_default("x\u{202e}y"), "x\\u202ey");
        assert_eq!(safe_text_default("A → B"), "A \\u2192 B");
        assert_eq!(safe_text_default("😀"), "\\U0001f600");
        assert_eq!(safe_text_default("“quoted” — ok…"), "“quoted” — ok…");
    }

    #[test]
    fn safe_text_normalises_bounds_and_keeps_unicode_letters() {
        assert_eq!(safe_text_default("e\u{301}"), "é"); // NFC
        assert_eq!(safe_text_default("日本語"), "日本語");
        assert_eq!(safe_text("abcdef", 3), "abc…");
        // The escape is never split: limit counts the whole piece.
        assert_eq!(safe_text("ab\n", 3), "ab…");
        assert_eq!(safe_text("", 5), "");
    }

    #[test]
    fn shortcut_formatting_and_editing_round_trip() {
        assert_eq!(format_shortcut("alt_r"), "Hold Right Option");
        assert_eq!(format_shortcut(""), "Off");
        assert_eq!(format_shortcut("<ctrl>+<alt>+1"), "Ctrl+Option+1");
        assert_eq!(format_shortcut("<ctrl>+<alt>+<f5>"), "Ctrl+Option+F5");
        assert_eq!(format_shortcut("f13"), "Hold F13");
        assert_eq!(format_shortcut("<cmd>+<shift>+<space>"), "Cmd+Shift+Space");
        let v = darwin();
        let config = Config::defaults_for(true);
        for value in [
            "alt_l",
            "alt_r",
            "cmd_r",
            "ctrl_r",
            "f13",
            "<ctrl>+<alt>+1",
            "<ctrl>+<cmd>+v",
            "<cmd>+<shift>+<space>",
            "<ctrl>+<alt>+<f5>",
        ] {
            let text = editable_shortcut(&json!(value));
            let key = if value.contains('+') || is_hold_key(value) {
                "hotkey"
            } else {
                "polish_hotkey"
            };
            assert_eq!(
                v.value(key, &json!(text), &config).unwrap(),
                json!(value),
                "{value}"
            );
        }
        assert_eq!(editable_shortcut(&json!("")), "");
        assert_eq!(editable_shortcut(&Value::Null), "");
        assert_eq!(editable_shortcut(&json!("alt_r")), "right_option");
    }

    #[test]
    fn display_values() {
        assert_eq!(display_value("sounds_enabled", &json!(true)), "On");
        assert_eq!(display_value("sounds_enabled", &json!(false)), "Off");
        assert_eq!(
            display_value("paste_method", &json!("osascript")),
            "AppleScript"
        );
        assert_eq!(display_value("paste_method", &json!("auto")), "Automatic");
        assert_eq!(
            display_value("style_work", &json!("very_casual")),
            "Very casual"
        );
        assert_eq!(display_value("cleanup_level", &json!("")), "—");
        assert_eq!(display_value("cleanup_level", &Value::Null), "—");
        assert_eq!(display_value("cleanup_level", &json!("a\nb")), "A\\u000ab");
        assert_eq!(display_value("hotkey", &Value::Null), "Off");
    }

    // ---- validation -----------------------------------------------------

    #[test]
    fn manual_groups_cover_every_allowlist_key_exactly_once() {
        let keys: Vec<&str> = MANUAL_GROUPS
            .iter()
            .flat_map(|(_, k)| k.iter().copied())
            .collect();
        let unique: BTreeSet<&str> = keys.iter().copied().collect();
        assert_eq!(keys.len(), 22);
        assert_eq!(unique, ALLOWLIST.iter().copied().collect());
        assert_eq!(
            MANUAL_GROUPS.iter().map(|(g, _)| *g).collect::<Vec<_>>(),
            ["Dictation", "Cleanup & style", "Shortcuts", "Meetings"]
        );
        let choice: BTreeSet<&str> = keys
            .iter()
            .copied()
            .filter(|k| control_kind(k) == "choice")
            .collect();
        assert_eq!(
            choice,
            BTreeSet::from([
                "cleanup_level",
                "style_work",
                "style_email",
                "style_personal",
                "style_other",
                "paste_method",
                "meeting_default_template"
            ])
        );
        assert!(keys.iter().all(|k| label(k).is_some()));
    }

    #[test]
    fn bool_enum_and_choice_values() {
        let v = darwin();
        let c = base();
        assert_eq!(
            v.value("sounds_enabled", &json!("Off"), &c).unwrap(),
            json!(false)
        );
        assert_eq!(
            v.value("sounds_enabled", &json!(" yes "), &c).unwrap(),
            json!(true)
        );
        assert_eq!(
            v.value("sounds_enabled", &json!("maybe"), &c)
                .unwrap_err()
                .0,
            "must be on or off"
        );
        assert_eq!(
            v.value("sounds_enabled", &json!(3), &c).unwrap_err().0,
            "value must be text or on/off"
        );
        assert_eq!(
            v.value("style_work", &json!("Very Casual"), &c).unwrap(),
            json!("very_casual")
        );
        assert_eq!(
            v.value("style_work", &json!("shouty"), &c).unwrap_err().0,
            "choose one of: formal, casual, very_casual, excited"
        );
        assert_eq!(
            v.value("paste_method", &json!("OsaScript"), &c).unwrap(),
            json!("osascript")
        );
        assert!(v
            .value("paste_method", &json!("xdotool"), &c)
            .unwrap_err()
            .0
            .starts_with("on this computer choose one of: auto, clipboard, osascript"));
        assert_eq!(
            v.value("meeting_default_template", &json!("Standup"), &c)
                .unwrap(),
            json!("standup")
        );
        assert_eq!(
            v.value("meeting_default_template", &json!("nope"), &c)
                .unwrap_err()
                .0,
            "unknown template; choose one of: general, standup"
        );
        assert!(v
            .value("server_port", &json!("1"), &c)
            .unwrap_err()
            .0
            .contains("is not a setting"));
        assert!(v
            .value("sounds_enabled", &json!("x".repeat(65)), &c)
            .unwrap_err()
            .0
            .contains("too long"));
        assert!(v
            .value("hotkey", &json!("alt_r\n"), &c)
            .unwrap_err()
            .0
            .contains("control characters"));
        // Template ids outside the plain charset are filtered out.
        let odd = Validator::with_templates(Some("linux"), &["ok", "Bad Id", "also-ok_1", ""]);
        assert_eq!(odd.template_ids(&c), vec!["ok", "also-ok_1"]);
    }

    #[test]
    fn shortcut_values_and_errors() {
        let v = darwin();
        let c = base();
        assert_eq!(
            v.value("hotkey", &json!("right option"), &c).unwrap(),
            json!("alt_r")
        );
        assert_eq!(v.value("hotkey", &json!("F13"), &c).unwrap(), json!("f13"));
        assert!(v
            .value("hotkey", &json!("a"), &c)
            .unwrap_err()
            .0
            .contains("is not a key you can hold"));
        assert_eq!(
            v.value("hotkey", &json!("off"), &c).unwrap_err().0,
            "dictation needs a shortcut; it can't be turned off"
        );
        assert_eq!(
            v.value("command_hotkey", &json!("none"), &c).unwrap(),
            json!("")
        );
        assert_eq!(
            v.value("command_hotkey", &json!("ctrl+a"), &c)
                .unwrap_err()
                .0,
            "must be a single key to hold, not a combination"
        );
        assert_eq!(
            v.value("polish_hotkey", &json!("ctrl+option+1"), &c)
                .unwrap(),
            json!("<ctrl>+<alt>+1")
        );
        assert_eq!(v.value("polish_hotkey", &json!(""), &c).unwrap(), json!(""));
        assert!(v
            .value("polish_hotkey", &json!("shift+a"), &c)
            .unwrap_err()
            .0
            .contains("needs Ctrl"));
        assert!(v
            .value("polish_hotkey", &json!("ctrl+a+b"), &c)
            .unwrap_err()
            .0
            .contains("exactly one non-modifier"));
        assert!(v
            .value("polish_hotkey", &json!("ctrl+ctrl+a"), &c)
            .unwrap_err()
            .0
            .contains("repeats a key"));
        assert!(v
            .value("polish_hotkey", &json!("ctrl+"), &c)
            .unwrap_err()
            .0
            .contains("not a valid shortcut"));
        assert!(v
            .value("polish_hotkey", &json!("ctrl+banana"), &c)
            .unwrap_err()
            .0
            .contains("unknown key 'banana'"));
        // `insert` is missing from pynput's darwin Key class only.
        assert!(v
            .value("polish_hotkey", &json!("ctrl+insert"), &c)
            .unwrap_err()
            .0
            .contains("not available"));
        let linux = Validator::with_templates(Some("linux"), &[]);
        assert_eq!(
            linux
                .value("polish_hotkey", &json!("ctrl+insert"), &c)
                .unwrap(),
            json!("<ctrl>+<insert>")
        );
        assert_eq!(
            linux.value("polish_hotkey", &json!("ctrl+é"), &c).unwrap(),
            json!("<ctrl>+é")
        );
        // Non-string shortcut values.
        assert_eq!(
            v.value("hotkey", &json!(true), &c).unwrap_err().0,
            "needs a key name"
        );
        assert_eq!(
            v.value("polish_hotkey", &json!(true), &c).unwrap_err().0,
            "needs a shortcut"
        );
    }

    #[test]
    fn checked_current_rejects_tampered_values() {
        let v = darwin();
        let c = with(base(), &[("style_work", "Weird"), ("hotkey", "alt_l")]);
        assert_eq!(v.checked_current("style_work", &c), None);
        assert_eq!(v.checked_current("hotkey", &c), Some(json!("alt_l")));
        // A non-normalised but acceptable spelling is not "checked unchanged".
        let c = with(base(), &[("polish_hotkey", "<CTRL>+<alt>+1")]);
        assert_eq!(v.checked_current("polish_hotkey", &c), None);
        let rows = manual_settings_for(&v, &with(base(), &[("style_work", "Weird")]));
        let row = rows.iter().find(|r| r.key == "style_work").unwrap();
        assert!(!row.valid && row.value.is_none() && row.display == "Not set");
        assert_eq!(row.choices.len(), 4);
        assert_eq!(
            row.choices[2],
            ("very_casual".to_string(), "Very casual".to_string())
        );
    }

    // ---- shortcut clashes (test_review_29_shortcuts) ----------------------

    #[test]
    fn defaults_do_not_clash_and_reordered_chords_do() {
        let v = darwin();
        assert!(v.conflicts(&base(), &SHORTCUT_KEYS).is_empty());
        let value = v
            .value("polish_hotkey", &json!("alt+ctrl+2"), &base())
            .unwrap();
        assert_eq!(value, json!("<alt>+<ctrl>+2"));
        let config = with(base(), &[("polish_hotkey", "<alt>+<ctrl>+2")]);
        let problems = v.conflicts(&config, &["polish_hotkey"]);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].1.contains("already the") && problems[0].1.contains("Prompt"));
        let config = with(
            base(),
            &[
                ("polish_hotkey", "<ctrl>+<alt>+7"),
                ("prompt_hotkey", "<alt>+<ctrl>+7"),
            ],
        );
        assert_eq!(
            clash_keys(&v, &config, &["polish_hotkey", "prompt_hotkey"]),
            BTreeSet::from(["polish_hotkey".to_string(), "prompt_hotkey".to_string()])
        );
    }

    #[test]
    fn equivalent_spellings_clash_on_darwin() {
        let v = darwin();
        for existing in [
            "<CTRL>+<ALT>+2",
            "<ctrl>+<ALT>+2",
            "<alt>+<ctrl>+2",
            "<ctrl_l>+<alt>+2",
            "<ctrl>+<alt_l>+2",
        ] {
            let config = with(
                base(),
                &[
                    ("prompt_hotkey", existing),
                    ("polish_hotkey", "<ctrl>+<alt>+2"),
                ],
            );
            assert_eq!(
                clash_keys(&v, &config, &["polish_hotkey"]).len(),
                1,
                "{existing}"
            );
        }
        let config = with(
            base(),
            &[
                ("polish_hotkey", "<ctrl>+<alt>+K"),
                ("prompt_hotkey", "<ctrl>+<alt>+k"),
            ],
        );
        assert_eq!(clash_keys(&v, &config, &["polish_hotkey"]).len(), 1);
        let config = with(
            base(),
            &[
                ("hotkey", "<cmd>+<shift>+d"),
                ("polish_hotkey", "<shift>+<cmd>+d"),
            ],
        );
        assert_eq!(clash_keys(&v, &config, &["polish_hotkey"]).len(), 1);
    }

    #[test]
    fn distinct_chords_and_holds_stay_allowed() {
        let v = darwin();
        for (l, r) in [
            ("<ctrl>+<alt>+1", "<ctrl>+<alt>+<shift>+1"),
            ("<ctrl>+<alt>+1", "<cmd>+<alt>+1"),
            ("<ctrl>+<alt>+1", "<ctrl>+<alt>+<f1>"),
            ("<ctrl>+<alt>+1", "<ctrl>+<alt>+2"),
            ("<ctrl>+<alt>+<up>", "<ctrl>+<alt>+<down>"),
        ] {
            let config = with(base(), &[("polish_hotkey", l), ("prompt_hotkey", r)]);
            assert!(
                v.conflicts(&config, &["polish_hotkey", "prompt_hotkey"])
                    .is_empty(),
                "{l} {r}"
            );
        }
        for platform in ["darwin", "linux", "win32"] {
            let v = Validator::with_templates(Some(platform), &[]);
            for (l, r) in [
                ("alt_l", "alt_r"),
                ("ctrl_l", "ctrl_r"),
                ("cmd_l", "cmd_r"),
                ("shift_l", "shift_r"),
            ] {
                let config = with(base(), &[("hotkey", l), ("command_hotkey", r)]);
                assert!(
                    v.conflicts(&config, &["hotkey", "command_hotkey"])
                        .is_empty(),
                    "{platform} {l}"
                );
            }
        }
        // A hold key is never the same shortcut as a combo that uses it.
        for (hold, combo) in [
            ("ctrl_r", "<ctrl>+<alt>+1"),
            ("alt_r", "<alt>+<ctrl>+1"),
            ("f13", "<ctrl>+<f13>"),
        ] {
            let config = with(
                base(),
                &[("command_hotkey", hold), ("polish_hotkey", combo)],
            );
            assert!(v
                .conflicts(&config, &["command_hotkey", "polish_hotkey"])
                .is_empty());
        }
    }

    #[test]
    fn identical_holds_and_darwin_aliases_clash() {
        let v = darwin();
        let config = with(base(), &[("hotkey", "alt_r"), ("command_hotkey", "alt_r")]);
        assert_eq!(clash_keys(&v, &config, &["command_hotkey"]).len(), 1);
        for (hold, same) in [
            ("alt", "alt_l"),
            ("ctrl", "ctrl_l"),
            ("cmd", "cmd_l"),
            ("alt_r", "alt_gr"),
        ] {
            let config = with(base(), &[("hotkey", hold), ("command_hotkey", same)]);
            assert_eq!(
                clash_keys(&v, &config, &["command_hotkey"]).len(),
                1,
                "{hold}/{same}"
            );
        }
        let linux = Validator::with_templates(Some("linux"), &[]);
        let config = with(base(), &[("hotkey", "alt"), ("command_hotkey", "alt_l")]);
        assert!(linux.conflicts(&config, &["command_hotkey"]).is_empty());
    }

    #[test]
    fn off_and_unparseable_shortcuts_never_clash() {
        let v = darwin();
        let config = with(
            base(),
            &[
                ("command_hotkey", ""),
                ("polish_hotkey", ""),
                ("prompt_hotkey", ""),
            ],
        );
        assert!(v.conflicts(&config, &SHORTCUT_KEYS).is_empty());
        let config = with(
            base(),
            &[
                ("prompt_hotkey", "<ctrl>++"),
                ("polish_hotkey", "<ctrl>+<alt>+1"),
            ],
        );
        assert!(v.conflicts(&config, &["polish_hotkey"]).is_empty());
    }

    // ---- lines and warnings -------------------------------------------

    #[test]
    fn change_lines_are_rebuilt_not_trusted() {
        let sounds = setting_change("sounds_enabled", json!(true), json!(false));
        assert_eq!(sounds.line(), "Sounds: On → Off");
        let template = setting_change(
            "meeting_default_template",
            json!("general"),
            json!("standup"),
        );
        assert_eq!(
            template.line(),
            "Default meeting template: General → Standup (From the next meeting)"
        );
        let mut forged = sounds.clone();
        forged.label = "Everything fine ✓".into();
        forged.new = "ignored".into();
        assert_eq!(forged.line(), "Sounds: On → Off");
        forged.key = "server_port".into();
        assert_eq!(forged.line(), INVALID_LINE);
        let word = word_change(&json!("Kubernetes")).unwrap();
        assert_eq!(word.line(), "Dictionary: — → add “Kubernetes”");
        let repl = replacement_change(&json!("kay eight ess"), &json!("k8s")).unwrap();
        assert_eq!(repl.line(), "Replacement: “kay eight ess” → “k8s”");
        let mut bad = repl.clone();
        bad.value = json!(["only one"]);
        assert_eq!(bad.line(), INVALID_LINE);
        let mut odd = word.clone();
        odd.kind = "mystery".into();
        assert_eq!(odd.line(), INVALID_LINE);
        assert_eq!(change_warning(&odd, "x"), "Not valid: preview again.");
    }

    #[test]
    fn warnings_flag_shortcuts_switches_off_and_unmentioned() {
        let off = setting_change("sounds_enabled", json!(true), json!(false));
        assert_eq!(
            warnings_for(std::slice::from_ref(&off), "turn off sounds"),
            vec!["Sounds: On → Off — Check: turns Sounds off.".to_string()]
        );
        let on = setting_change("sounds_enabled", json!(false), json!(true));
        assert!(warnings_for(std::slice::from_ref(&on), "turn on the sounds").is_empty());
        assert_eq!(
            warnings_for(&[on], "something else"),
            vec![
                "Sounds: Off → On — Check: your request doesn't seem to mention this.".to_string()
            ]
        );
        let level = setting_change("cleanup_level", json!("light"), json!("none"));
        assert_eq!(
            level.warning("set cleanup level to none"),
            "Check: turns Cleanup off."
        );
        let hot = setting_change(
            "polish_hotkey",
            json!("<ctrl>+<alt>+1"),
            json!("<ctrl>+<alt>+9"),
        );
        assert_eq!(
            hot.warning("change polish"),
            "Check: changes a keyboard shortcut."
        );
        assert_eq!(hot.warning(""), "Check: changes a keyboard shortcut.");
        let cmd = setting_change("command_hotkey", json!("alt_r"), json!("f13"));
        assert!(!cmd.warning("change all shortcuts").contains("doesn't seem"));
        assert!(cmd.warning("change the sounds").contains("doesn't seem"));
    }

    #[test]
    fn dictionary_mentions_need_the_whole_phrase() {
        let know = word_change(&json!("no")).unwrap();
        assert!(!mentions(&know, "i know it"));
        assert!(mentions(&know, "add no to my dictionary"));
        assert!(mentions(&know, "add NO"));
        let aa = word_change(&json!("aa")).unwrap();
        assert!(mentions(&aa, "xaaa aa"));
        assert!(!mentions(&aa, "xaaa"));
        let uni = word_change(&json!("Ünïcødé")).unwrap();
        assert!(mentions(&uni, "learn ünïcødé please"));
        assert!(!mentions(&uni, "xünïcødé"));
    }

    // ---- words, replacements, calls -----------------------------------

    #[test]
    fn dictionary_text_rules() {
        assert!(word_change(&json!("")).unwrap_err().0.contains("is empty"));
        assert!(word_change(&json!("  "))
            .unwrap_err()
            .0
            .contains("is empty"));
        assert!(word_change(&json!(5))
            .unwrap_err()
            .0
            .contains("must be text"));
        assert!(word_change(&json!("a\nb"))
            .unwrap_err()
            .0
            .contains("control characters or line breaks"));
        assert!(word_change(&json!("a\u{a0}b"))
            .unwrap_err()
            .0
            .contains("unusual spaces"));
        assert!(word_change(&json!("a → b"))
            .unwrap_err()
            .0
            .contains("may only use letters"));
        assert!(word_change(&json!("x".repeat(65)))
            .unwrap_err()
            .0
            .contains("longer than 64"));
        assert!(word_change(&json!("x".repeat(257)))
            .unwrap_err()
            .0
            .contains("longer than 64"));
        assert_eq!(word_change(&json!("  Zoë ")).unwrap().value, json!("Zoë"));
        assert_eq!(word_change(&json!("e\u{301}")).unwrap().value, json!("é"));
        assert!(word_change(&json!("日本語 ✓"))
            .unwrap_err()
            .0
            .contains("may only use"));
        assert!(replacement_change(&json!("Same"), &json!("same"))
            .unwrap_err()
            .0
            .contains("must change the text"));
        assert!(replacement_change(&json!("a"), &json!(""))
            .unwrap_err()
            .0
            .starts_with("written text"));
        let ok = replacement_change(&json!("ü".repeat(100)), &json!("b")).unwrap();
        assert_eq!(ok.replacement_pair().unwrap().0.chars().count(), 100);
    }

    #[test]
    fn parse_call_accepts_and_refuses() {
        let (name, args) = parse_call(&tool_call(
            "set_setting",
            json!({"key": "sounds_enabled", "value": false}),
        ))
        .unwrap();
        assert_eq!(name, "set_setting");
        assert_eq!(args["value"], json!(false));
        // Arguments as a JSON string, like Ollama's OpenAI-style output.
        let (_, args) = parse_call(&tool_call(
            "add_dictionary_word",
            json!("{\"word\": \"Kubernetes\"}"),
        ))
        .unwrap();
        assert_eq!(args["word"], json!("Kubernetes"));
        let err = |v: Value| parse_call(&v).unwrap_err().0;
        assert_eq!(err(json!("nope")), "the model sent a malformed tool call");
        assert_eq!(
            err(json!({"function": 3})),
            "the model sent a malformed tool call"
        );
        assert_eq!(
            err(json!({"function": {"name": "x"}, "evil": 1})),
            "the model sent a tool call with unexpected fields"
        );
        assert_eq!(
            err(tool_call("rm_rf", json!({}))),
            "the model asked for an unknown tool 'rm_rf'"
        );
        assert_eq!(
            err(json!({"function": {"arguments": {}}})),
            "the model asked for an unknown tool ?"
        );
        assert_eq!(
            err(tool_call("set_setting", json!("{not json"))),
            "the model sent malformed arguments for set_setting"
        );
        assert_eq!(
            err(tool_call("set_setting", json!("[1]"))),
            "the model sent malformed arguments for set_setting"
        );
        assert_eq!(
            err(tool_call("set_setting", json!("x".repeat(4097)))),
            "the model sent oversized tool arguments"
        );
        assert_eq!(
            err(tool_call("set_setting", json!({"key": "a"}))),
            "the model sent the wrong arguments for set_setting"
        );
        assert_eq!(
            err(tool_call("set_setting", json!({"key": "a", "value": 3}))),
            "the model sent a value of the wrong type for set_setting"
        );
        assert_eq!(
            err(tool_call("add_dictionary_word", json!({"word": true}))),
            "the model sent a word of the wrong type for add_dictionary_word"
        );
        assert_eq!(
            err(tool_call(
                "add_dictionary_word",
                json!({"word": "x".repeat(201)})
            )),
            "the model sent an overlong word for add_dictionary_word"
        );
        assert!(
            err(tool_call("add_dictionary_word", json!({"word": "a\nb"})))
                .contains("has control characters or line breaks")
        );
        assert_eq!(
            parse_call(&tool_call("add_dictionary_word", json!({})))
                .unwrap_err()
                .0,
            "the model sent the wrong arguments for add_dictionary_word"
        );
    }

    fn view() -> DictionaryView {
        DictionaryView::default()
    }

    #[test]
    fn changes_from_calls_filters_unrequested_duplicates_and_clashes() {
        let v = darwin();
        let config = base();
        let calls = vec![
            tool_call(
                "set_setting",
                json!({"key": "sounds_enabled", "value": "off"}),
            ),
            tool_call(
                "set_setting",
                json!({"key": "sounds_enabled", "value": "on"}),
            ),
            tool_call(
                "set_setting",
                json!({"key": "cleanup_enabled", "value": true}),
            ),
            tool_call(
                "set_setting",
                json!({"key": "style_work", "value": "excited"}),
            ),
            tool_call("set_setting", json!({"key": "server_port", "value": "1"})),
            tool_call("add_dictionary_word", json!({"word": "Kubernetes"})),
            tool_call("add_dictionary_word", json!({"word": "kubernetes"})),
            tool_call(
                "add_replacement",
                json!({"spoken": "kay eight", "written": "k8s"}),
            ),
            tool_call("bogus", json!({})),
        ];
        let (changes, rejected) = changes_from_calls(
            &v,
            &calls,
            &config,
            "turn off sounds, learn Kubernetes",
            &view(),
        )
        .unwrap();
        let lines: Vec<String> = changes.iter().map(Change::line).collect();
        assert_eq!(
            lines,
            ["Sounds: On → Off", "Dictionary: — → add “Kubernetes”"]
        );
        assert_eq!(
            rejected,
            vec![
                "sounds_enabled was set twice; keeping the first".to_string(),
                "Cleanup is already On".to_string(),
                "Work style wasn't in your request, so it was left out; ask for it by name".to_string(),
                "'server_port' is not a setting LocalFlow can change here".to_string(),
                "Replacing “kay eight” wasn't in your request, so it was left out; ask for it by name".to_string(),
                "the model asked for an unknown tool 'bogus'".to_string(),
            ]
        );
        // The existing dictionary is consulted.
        let dict = DictionaryView {
            words: vec!["KUBERNETES".into()],
            replacements: vec![("Kay Eight".into(), "k8s".into())],
        };
        let calls = vec![
            tool_call("add_dictionary_word", json!({"word": "kubernetes"})),
            tool_call(
                "add_replacement",
                json!({"spoken": "kay eight", "written": "k8s"}),
            ),
        ];
        let (changes, rejected) =
            changes_from_calls(&v, &calls, &config, "kubernetes and kay eight", &dict).unwrap();
        assert!(changes.is_empty());
        assert_eq!(
            rejected,
            vec![
                "“kubernetes” is already in your dictionary",
                "“kay eight” already has a replacement"
            ]
        );
    }

    #[test]
    fn a_batch_clash_drops_only_the_clashing_setting() {
        let v = darwin();
        let calls = vec![
            tool_call(
                "set_setting",
                json!({"key": "polish_hotkey", "value": "ctrl+alt+2"}),
            ),
            tool_call(
                "set_setting",
                json!({"key": "sounds_enabled", "value": "off"}),
            ),
        ];
        let (changes, rejected) = changes_from_calls(
            &v,
            &calls,
            &base(),
            "change polish shortcut and sounds",
            &view(),
        )
        .unwrap();
        assert_eq!(
            changes.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
            ["sounds_enabled"]
        );
        assert_eq!(rejected.len(), 1);
        assert!(
            rejected[0].contains("Polish shortcut: Ctrl+Option+2 is already the Prompt shortcut")
        );
    }

    // ---- replan / apply / undo via the fake ------------------------------

    fn fake() -> FakeSettingsAgent {
        let mut config = base();
        config.set_field("sounds_enabled", json!(true));
        FakeSettingsAgent::with_config(config, Some("darwin"), &["general", "standup"])
    }

    #[test]
    fn fake_propose_apply_and_undo_round_trip() {
        let agent = fake();
        agent.script_calls(
            "gemma4:e4b",
            vec![
                tool_call(
                    "set_setting",
                    json!({"key": "sounds_enabled", "value": "off"}),
                ),
                tool_call("add_dictionary_word", json!({"word": "Kubernetes"})),
            ],
        );
        let proposal = agent.propose("turn off sounds and add Kubernetes").unwrap();
        assert_eq!(proposal.model, "gemma4:e4b");
        assert_eq!(agent.model().as_deref(), Some("gemma4:e4b"));
        assert_eq!(proposal.changes.len(), 2);
        assert_eq!(
            proposal.warnings(),
            vec!["Sounds: On → Off — Check: turns Sounds off."]
        );
        assert!(agent.pending_undo().is_none());
        // Nothing is written by a preview.
        assert_eq!(agent.config().field_bool("sounds_enabled"), Some(true));
        let result = agent.apply(&proposal.changes).unwrap();
        assert_eq!(result.message, "Applied 2 changes.");
        assert_eq!(result.applied.len(), 2);
        assert_eq!(agent.config().field_bool("sounds_enabled"), Some(false));
        assert_eq!(agent.words(), vec!["Kubernetes"]);
        assert_eq!(
            agent.pending_undo().unwrap(),
            vec![
                "Sounds: On → Off"
                    .to_string()
                    .replace("On → Off", "Off → On"),
                "Remove word “Kubernetes”".to_string()
            ]
        );
        let undone = agent.undo().unwrap();
        assert_eq!(undone.message, "Undid the last change.");
        assert_eq!(
            undone.reverted,
            vec!["Sounds: On", "Removed word “Kubernetes”"]
        );
        assert_eq!(agent.config().field_bool("sounds_enabled"), Some(true));
        assert!(agent.words().is_empty());
        assert_eq!(agent.undo().unwrap_err().0, "There's no change to undo.");
        assert_eq!(agent.calls(), ["propose", "apply", "undo", "undo"]);
    }

    #[test]
    fn apply_refuses_stale_forged_and_empty_batches() {
        let agent = fake();
        assert_eq!(agent.apply(&[]).unwrap_err().0, "There's nothing to apply.");
        let change = setting_change("sounds_enabled", json!(true), json!(false));
        // Settings moved since the preview.
        let mut moved = agent.config();
        moved.set_field("sounds_enabled", json!(false));
        agent.set_config(moved);
        assert_eq!(
            agent.apply(std::slice::from_ref(&change)).unwrap_err().0,
            "Nothing was changed: Sounds changed to Off since the preview. Preview again."
        );
        // A forged display field fails the re-derivation.
        let mut forged = setting_change("sounds_enabled", json!(false), json!(true));
        forged.new = "Off".into();
        assert_eq!(
            agent.apply(&[forged]).unwrap_err().0,
            "This preview isn't valid; preview again."
        );
        let mut unknown = change.clone();
        unknown.key = "server_port".into();
        assert_eq!(
            agent.apply(&[unknown]).unwrap_err().0,
            "This preview isn't valid; preview again."
        );
        // Same key twice in one batch.
        let on = setting_change("sounds_enabled", json!(false), json!(true));
        assert_eq!(
            agent.apply(&[on.clone(), on]).unwrap_err().0,
            "This preview isn't valid; preview again."
        );
        // More than MAX_TOOL_CALLS changes.
        let many: Vec<Change> = (0..11)
            .map(|i| word_change(&json!(format!("w{i}"))).unwrap())
            .collect();
        assert_eq!(
            agent.apply(&many).unwrap_err().0,
            "Too many changes in one batch."
        );
    }

    #[test]
    fn apply_dictionary_conflicts_and_save_failures() {
        let agent = fake();
        agent.seed_dictionary(&["Zed"], &[("kay", "K")]);
        let word = word_change(&json!("zed")).unwrap();
        assert_eq!(
            agent.apply(&[word]).unwrap_err().0,
            "Nothing was changed: “zed” is already in your dictionary. Preview again."
        );
        let repl = replacement_change(&json!("KAY"), &json!("k")).unwrap();
        assert!(agent
            .apply(&[repl])
            .unwrap_err()
            .0
            .contains("already has a replacement"));
        agent.set_save_error(Some("disk full"));
        let change = setting_change("sounds_enabled", json!(true), json!(false));
        assert_eq!(
            agent.apply(&[change]).unwrap_err().0,
            "Nothing was changed: couldn't save (disk full)."
        );
        assert_eq!(agent.config().field_bool("sounds_enabled"), Some(true));
        assert!(agent.pending_undo().is_none());
    }

    #[test]
    fn manual_set_setting_uses_the_agent_rules_and_records_undo() {
        let agent = fake();
        let result = agent.set_setting("sounds_enabled", &json!(false)).unwrap();
        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.applied[0].line(), "Sounds: On → Off");
        assert_eq!(agent.pending_undo().unwrap(), vec!["Sounds: Off → On"]);
        let again = agent.set_setting("sounds_enabled", &json!("off")).unwrap();
        assert!(again.applied.is_empty());
        assert_eq!(again.message, "Nothing needed changing.");
        assert_eq!(
            agent.set_setting("hotkey", &json!("a")).unwrap_err().0,
            "Dictation shortcut: 'a' is not a key you can hold; use a modifier (e.g. alt_r) or a function key (f13)"
        );
        // Clash with an existing shortcut: refused, nothing written.
        let err = agent
            .set_setting("polish_hotkey", &json!("alt+ctrl+2"))
            .unwrap_err();
        assert!(err.0.starts_with("Nothing was changed: Polish shortcut: Option+Ctrl+2 is already the Prompt shortcut"), "{err}");
        assert_eq!(
            agent.set_setting("nope", &json!(1)).unwrap_err().0,
            "'nope' is not a setting LocalFlow can change here"
        );
        // Escaped, bounded error text.
        let long = "x".repeat(64);
        let err = agent
            .set_setting("cleanup_level", &json!(long))
            .unwrap_err();
        assert!(err.0.starts_with("Cleanup level: choose one of"));
    }

    #[test]
    fn undo_leaves_newer_changes_alone() {
        let agent = fake();
        agent.set_setting("sounds_enabled", &json!(false)).unwrap();
        let mut moved = agent.config();
        moved.set_field("sounds_enabled", json!(true));
        agent.set_config(moved);
        let err = agent.undo().unwrap_err();
        assert_eq!(
            err.0,
            "Nothing to undo: everything was changed again since. Sounds (changed to On since)"
        );
        // Partly newer: word removed manually, setting reverted.
        let agent = fake();
        let word = word_change(&json!("Zed")).unwrap();
        let on = setting_change("sounds_enabled", json!(true), json!(false));
        agent.apply(&[on, word]).unwrap();
        agent.state().words.clear();
        let undone = agent.undo().unwrap();
        assert_eq!(undone.reverted, vec!["Sounds: On"]);
        assert_eq!(undone.skipped, vec!["word “Zed” (edited or removed since)"]);
        assert_eq!(
            undone.message,
            "Undid the last change. Left alone: word “Zed” (edited or removed since)."
        );
    }

    #[test]
    fn undo_refuses_a_shortcut_clash() {
        let agent = fake();
        agent
            .set_setting("polish_hotkey", &json!("ctrl+alt+9"))
            .unwrap();
        let mut config = agent.config();
        config.set_field("prompt_hotkey", json!("<ctrl>+<alt>+1")); // now equals the old polish value
        agent.set_config(config);
        let err = agent.undo().unwrap_err();
        assert!(
            err.0.starts_with("Can't undo without a shortcut clash: "),
            "{err}"
        );
    }

    #[test]
    fn propose_errors_are_friendly() {
        let agent = fake();
        assert_eq!(
            agent.propose("   ").unwrap_err().0,
            "Tell LocalFlow what to change."
        );
        assert_eq!(
            agent.propose(&"x".repeat(501)).unwrap_err().0,
            "Keep the request under 500 characters."
        );
        assert!(agent
            .propose("turn off sounds")
            .unwrap_err()
            .0
            .contains("no scripted model answer"));
        agent.script_calls("m", vec![]);
        assert!(agent
            .propose("turn off sounds")
            .unwrap_err()
            .0
            .starts_with("The local model didn't suggest"));
        agent.script_calls(
            "m",
            (0..11)
                .map(|_| tool_call("add_dictionary_word", json!({"word": "a"})))
                .collect(),
        );
        assert!(agent
            .propose("add a")
            .unwrap_err()
            .0
            .contains("more than 10 changes"));
        agent.script_calls(
            "m",
            vec![tool_call(
                "set_setting",
                json!({"key": "style_work", "value": "excited"}),
            )],
        );
        assert_eq!(
            agent.propose("turn off sounds").unwrap_err().0,
            "Nothing to change: Work style wasn't in your request, so it was left out; ask for it by name"
        );
        agent.script_error(
            Some("gemma4"),
            "Couldn't reach Ollama at http://localhost:11434.",
        );
        assert_eq!(
            agent.propose("x").unwrap_err().0,
            "Couldn't reach Ollama at http://localhost:11434."
        );
        assert_eq!(agent.model().as_deref(), Some("gemma4"));
        // Control characters in a request become spaces.
        assert_eq!(clean_request(" a\u{0}b\n").unwrap(), "a b");
    }

    #[test]
    fn load_errors_surface_and_pending_undo_is_none() {
        let agent = fake();
        agent.set_load_error(Some("bad toml"));
        assert_eq!(
            agent.load().unwrap_err().0,
            "Couldn't read your settings (bad toml)."
        );
        assert!(agent.manual_settings(None).is_err());
        assert!(agent.pending_undo().is_none());
        agent.set_load_error(None);
        assert_eq!(agent.manual_settings(None).unwrap().len(), 22);
        let current = agent.current_settings(None).unwrap();
        assert_eq!(current[0], ("Sounds".to_string(), "On".to_string()));
        assert_eq!(
            current[3],
            (
                "Dictation shortcut".to_string(),
                "Hold Left Option".to_string()
            )
        );
        assert_eq!(current.len(), CURRENT_KEYS.len());
    }

    #[test]
    fn unicode_values_are_counted_by_characters() {
        // 64 two-byte characters is within the 64-character value limit.
        let v = darwin();
        let long_ok = "é".repeat(64);
        let err = v
            .value("cleanup_level", &json!(long_ok), &base())
            .unwrap_err();
        assert!(err.0.starts_with("choose one of"));
        let too_long = "é".repeat(65);
        assert_eq!(
            v.value("cleanup_level", &json!(too_long), &base())
                .unwrap_err()
                .0,
            "value is too long"
        );
        // Escapes are limited in characters, not bytes.
        assert_eq!(safe_text("日本語日本語", 4), "日本語日…");
    }
}
