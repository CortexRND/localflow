//! Settings agent: the [`SettingsAgent`] trait the Hub Settings page talks to,
//! the UI data contracts it exchanges, the display helpers the page renders
//! with, and a scripted in-memory [`FakeSettingsAgent`].
//!
//! Port of the *presentation* side of `localflow/settings_agent.py`: the
//! allowlist, labels and notes, [`Change`]/[`Proposal`]/[`ApplyResult`]/
//! [`UndoResult`]/[`ManualSetting`], [`safe_text`], [`display_value`],
//! [`format_shortcut`]/[`editable_shortcut`], [`change_line`] and the
//! deterministic warnings ([`warnings_for`]).
//!
//! Deliberately **not** here (it belongs to a real implementation of the trait,
//! later): the value/shortcut validator and clash rules, model tool-call
//! parsing, the apply/replan engine and the persistent undo record, the Ollama
//! client and system prompt, and the config/dictionary file I/O. The fake does
//! none of that either: it returns exactly what a test scripted and records
//! what it was asked, so it can never be mistaken for a second backend.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use lf_core::config::Config;
use lf_providers::pyfmt::{py_isspace, py_strip};
use regex::Regex;
use serde_json::{json, Value};
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;

// ------------------------------------------------------------- constants ---

/// The exact config fields the agent may change.
pub const ALLOWLIST: [&str; 24] = [
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

/// A `set_setting` value (shortcut, choice, on/off): the bound on editable text.
pub const MAX_VALUE_CHARS: usize = 64;
/// One rendered preview/undo/error line.
pub const MAX_DISPLAY_CHARS: usize = 300;

/// `LABELS[key]` from Python; `None` outside the allowlist.
pub fn label(key: &str) -> Option<&'static str> {
    Some(match key {
        "dictation_enabled" => "Dictation",
        "dictation_toggle_hotkey" => "Toggle dictation shortcut",
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
            "dictation_enabled",
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
            "dictation_toggle_hotkey",
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

/// Keys shown as an On/Off switch.
pub const BOOL_KEYS: [&str; 11] = [
    "dictation_enabled",
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
/// The writing styles a style popup offers.
pub const STYLES: [&str; 4] = ["formal", "casual", "very_casual", "excited"];
/// Keys edited as a shortcut text field.
pub const SHORTCUT_KEYS: [&str; 6] = [
    "dictation_toggle_hotkey",
    "hotkey",
    "command_hotkey",
    "polish_hotkey",
    "prompt_hotkey",
    "paste_last_hotkey",
];

const MODIFIERS: [&str; 13] = [
    "alt", "alt_l", "alt_r", "alt_gr", "ctrl", "ctrl_l", "ctrl_r", "cmd", "cmd_l", "cmd_r",
    "shift", "shift_l", "shift_r",
];

fn is_function_key(name: &str) -> bool {
    name.strip_prefix('f')
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|n| (1..=20).contains(&n) && n.to_string() == name[1..])
}

/// Keys that can be held (dictation hold key, command key); only used to word
/// a saved shortcut as "Hold …".
pub fn is_hold_key(name: &str) -> bool {
    MODIFIERS.contains(&name) || is_function_key(name)
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
    /// A setting change for `key`, display fields built from the values.
    /// Carries **no** validation: whether `value` is acceptable is the real
    /// agent's decision, not this data contract's.
    pub fn setting(key: &str, before: Value, value: Value) -> Change {
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

    /// An "add dictionary word" change (no validation, see [`Change::setting`]).
    pub fn word(word: &str) -> Change {
        Change {
            label: "Dictionary".to_string(),
            old: "—".to_string(),
            new: format!("add “{word}”"),
            kind: KIND_WORD.to_string(),
            key: String::new(),
            before: Value::Null,
            value: Value::String(word.to_string()),
            note: String::new(),
        }
    }

    /// An "add replacement" change (no validation, see [`Change::setting`]).
    pub fn replacement(spoken: &str, written: &str) -> Change {
        Change {
            label: "Replacement".to_string(),
            old: format!("“{spoken}”"),
            new: format!("“{written}”"),
            kind: KIND_REPLACEMENT.to_string(),
            key: String::new(),
            before: Value::Null,
            value: json!([spoken, written]),
            note: String::new(),
        }
    }

    /// The display line, rebuilt from kind/key/value; `label`, `old`, `new`
    /// and `note` are never shown as given, so a forged or stored `Change`
    /// can't spoof the preview.
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

/// What an apply (or one manual change) reports back.
#[derive(Clone, Debug, PartialEq)]
pub struct ApplyResult {
    pub applied: Vec<Change>,
    pub message: String,
}

/// What an undo reports back.
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
        "space" => "Space",
        "enter" => "Enter",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "page_up" => "Page Up",
        "page_down" => "Page Down",
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

/// A saved shortcut as text a person can edit (`"<ctrl>+<alt>+1"` ->
/// `"ctrl+option+1"`, `"alt_r"` -> `"right_option"`); `""` for off. Escaped
/// like any display text. Reading the text back is the real agent's job.
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

/// The display line of a change, rebuilt from its kind, key and values.
///
/// A line is only drawn for an allowlisted setting, a non-blank word or a
/// replacement whose two sides are non-blank and differ; anything else is
/// [`INVALID_LINE`]. (The full text validation lives in the real agent.)
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
        KIND_WORD => match change.value.as_str().filter(|w| !py_strip(w).is_empty()) {
            Some(word) => format!("Dictionary: — → add “{}”", safe_text_default(word)),
            None => invalid(),
        },
        KIND_REPLACEMENT => match change.replacement_pair() {
            Some((spoken, written))
                if !py_strip(spoken).is_empty()
                    && !py_strip(written).is_empty()
                    && casefold(spoken) != casefold(written) =>
            {
                format!(
                    "Replacement: “{}” → “{}”",
                    safe_text_default(spoken),
                    safe_text_default(written)
                )
            }
            _ => invalid(),
        },
        _ => invalid(),
    }
}

/// Words a request can use to mean a setting (besides its own label/key).
fn intent_words(key: &str) -> &'static [&'static str] {
    match key {
        "dictation_enabled" => &["dictation", "dictating", "push to talk", "push-to-talk"],
        "dictation_toggle_hotkey" => &["toggle dictation", "dictation toggle"],
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

/// Deterministic caution for one previewed change (`""` when none).
///
/// Every shortcut change and every switch turned off is flagged, and so is
/// any change the request doesn't seem to mention. Computed from the
/// change's own kind/key/values only, never from its display fields.
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

/// `(label, shown value)` for [`CURRENT_KEYS`] of any config (display only).
pub fn current_settings_for(config: &Config) -> Vec<(String, String)> {
    CURRENT_KEYS
        .iter()
        .map(|key| {
            let value = config.field(key).cloned().unwrap_or(Value::Null);
            (label_of(key).to_string(), display_value(key, &value))
        })
        .collect()
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

// ------------------------------------------------------------------ fake ---

/// Fixture rows for the manual controls: every [`MANUAL_GROUPS`] key in page
/// order, with the value read from `config` and display strings from the
/// display helpers. Every row is marked valid and the choices are fixed
/// tables (darwin paste methods, the two sample meeting templates): this is
/// *data for a UI to lay out*, not validation. Pass it to
/// [`FakeSettingsAgent::with_rows`] and edit rows to script invalid values.
pub fn sample_manual_rows(config: &Config) -> Vec<ManualSetting> {
    let mut rows = Vec::new();
    for (group, keys) in MANUAL_GROUPS {
        for key in keys {
            let choices: &[&str] = match *key {
                "cleanup_level" => &["none", "light", "medium"],
                "style_work" | "style_email" | "style_personal" | "style_other" => &STYLES,
                "paste_method" => &["auto", "clipboard", "osascript"],
                "meeting_default_template" => &["general", "standup"],
                _ => &[],
            };
            let value = config.field(key).cloned().unwrap_or(Value::Null);
            rows.push(ManualSetting {
                key: (*key).to_string(),
                group: group.to_string(),
                label: label_of(key).to_string(),
                kind: control_kind(key).to_string(),
                display: display_value(key, &value),
                value: Some(value),
                valid: true,
                choices: choices
                    .iter()
                    .map(|choice| {
                        (
                            (*choice).to_string(),
                            display_value(key, &Value::String((*choice).to_string())),
                        )
                    })
                    .collect(),
            });
        }
    }
    rows
}

/// One scripted answer to [`SettingsAgent::propose`].
#[derive(Clone, Debug)]
pub enum ScriptedPropose {
    /// The model answered with this preview (its `model` becomes
    /// [`SettingsAgent::model`]).
    Proposal(Proposal),
    /// The model (or Ollama) failed with this user-facing message.
    Error {
        model: Option<String>,
        message: String,
    },
}

/// A call the fake was asked to answer (the calls that ask the agent to do
/// something; plain reads are not recorded).
#[derive(Clone, Debug, PartialEq)]
pub enum FakeCall {
    Propose(String),
    SetSetting { key: String, value: Value },
    Apply(Vec<Change>),
    Undo,
}

type Scripted<T> = VecDeque<Result<T, SettingsAgentError>>;

struct FakeState {
    config: Config,
    load_error: Option<String>,
    rows: Vec<ManualSetting>,
    current: Option<Vec<(String, String)>>,
    undo_lines: Option<Vec<String>>,
    model: Option<String>,
    propose: VecDeque<ScriptedPropose>,
    set_setting: Scripted<ApplyResult>,
    apply: Scripted<ApplyResult>,
    undo: Scripted<UndoResult>,
    log: Vec<FakeCall>,
}

/// A scripted, in-memory [`SettingsAgent`] for tests and snapshots.
///
/// It implements **no** settings logic: no validation, no tool-call parsing,
/// no apply/undo bookkeeping. Every asking call (`propose`, `set_setting`,
/// `apply`, `undo`) pops the next answer a test queued for it and records the
/// call ([`FakeSettingsAgent::calls`]); with nothing queued it fails with a
/// message that says so, rather than inventing a result. The reads (`load`,
/// `manual_settings`, `current_settings`, `pending_undo`) return the scripted
/// config, rows and undo lines.
pub struct FakeSettingsAgent {
    state: Mutex<FakeState>,
}

impl Default for FakeSettingsAgent {
    fn default() -> Self {
        FakeSettingsAgent::new()
    }
}

impl FakeSettingsAgent {
    /// A fake over the darwin default config with [`sample_manual_rows`].
    pub fn new() -> Self {
        let config = Config::defaults_for(true);
        let rows = sample_manual_rows(&config);
        FakeSettingsAgent::with_rows(config, rows)
    }

    /// A fake serving `config` from `load` and exactly `rows` from
    /// `manual_settings`.
    pub fn with_rows(config: Config, rows: Vec<ManualSetting>) -> Self {
        FakeSettingsAgent {
            state: Mutex::new(FakeState {
                config,
                load_error: None,
                rows,
                current: None,
                undo_lines: None,
                model: None,
                propose: VecDeque::new(),
                set_setting: VecDeque::new(),
                apply: VecDeque::new(),
                undo: VecDeque::new(),
                log: Vec::new(),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Queue the answer of the next `propose`.
    pub fn script_propose(&self, answer: ScriptedPropose) {
        self.state().propose.push_back(answer);
    }

    /// Queue a successful preview.
    pub fn script_proposal(&self, proposal: Proposal) {
        self.script_propose(ScriptedPropose::Proposal(proposal));
    }

    /// Queue a model failure.
    pub fn script_error(&self, model: Option<&str>, message: &str) {
        self.script_propose(ScriptedPropose::Error {
            model: model.map(str::to_string),
            message: message.to_string(),
        });
    }

    /// Queue the answer of the next `set_setting`.
    pub fn script_set_setting(&self, result: Result<ApplyResult, SettingsAgentError>) {
        self.state().set_setting.push_back(result);
    }

    /// Queue the answer of the next `apply`.
    pub fn script_apply(&self, result: Result<ApplyResult, SettingsAgentError>) {
        self.state().apply.push_back(result);
    }

    /// Queue the answer of the next `undo`.
    pub fn script_undo(&self, result: Result<UndoResult, SettingsAgentError>) {
        self.state().undo.push_back(result);
    }

    /// Make `load` fail with "Couldn't read your settings (…)."; `None` heals.
    pub fn set_load_error(&self, message: Option<&str>) {
        self.state().load_error = message.map(str::to_string);
    }

    /// The config `load` returns.
    pub fn set_config(&self, config: Config) {
        self.state().config = config;
    }

    /// The rows `manual_settings` returns.
    pub fn set_rows(&self, rows: Vec<ManualSetting>) {
        self.state().rows = rows;
    }

    /// The `(label, value)` list `current_settings` returns (default: derived
    /// from the scripted config for display).
    pub fn set_current_settings(&self, current: Option<Vec<(String, String)>>) {
        self.state().current = current;
    }

    /// What `pending_undo` returns.
    pub fn set_undo_lines(&self, lines: Option<Vec<String>>) {
        self.state().undo_lines = lines;
    }

    /// The asking calls answered so far, in order.
    pub fn calls(&self) -> Vec<FakeCall> {
        self.state().log.clone()
    }

    fn load_locked(state: &FakeState) -> Result<Config, SettingsAgentError> {
        if let Some(message) = &state.load_error {
            return Err(agent_err(format!(
                "Couldn't read your settings ({message})."
            )));
        }
        Ok(state.config.clone())
    }

    fn unscripted<T>(what: &str) -> Result<T, SettingsAgentError> {
        Err(agent_err(format!(
            "The fake settings agent has no scripted {what} answer."
        )))
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
        let state = self.state();
        let loaded = Self::load_locked(&state)?;
        Ok(state
            .current
            .clone()
            .unwrap_or_else(|| current_settings_for(config.unwrap_or(&loaded))))
    }

    fn manual_settings(
        &self,
        _config: Option<&Config>,
    ) -> Result<Vec<ManualSetting>, SettingsAgentError> {
        let state = self.state();
        Self::load_locked(&state)?;
        Ok(state.rows.clone())
    }

    fn pending_undo(&self) -> Option<Vec<String>> {
        let state = self.state();
        Self::load_locked(&state).ok()?;
        state.undo_lines.clone().filter(|lines| !lines.is_empty())
    }

    fn propose(&self, request: &str) -> Result<Proposal, SettingsAgentError> {
        let mut state = self.state();
        state.log.push(FakeCall::Propose(request.to_string()));
        match state.propose.pop_front() {
            None => Self::unscripted("model").map_err(|_| {
                agent_err("The fake settings agent has no scripted model answer for this request.")
            }),
            Some(ScriptedPropose::Error { model, message }) => {
                state.model = model;
                Err(agent_err(message))
            }
            Some(ScriptedPropose::Proposal(proposal)) => {
                state.model = Some(proposal.model.clone());
                Ok(proposal)
            }
        }
    }

    fn set_setting(&self, key: &str, value: &Value) -> Result<ApplyResult, SettingsAgentError> {
        let mut state = self.state();
        state.log.push(FakeCall::SetSetting {
            key: key.to_string(),
            value: value.clone(),
        });
        state
            .set_setting
            .pop_front()
            .unwrap_or_else(|| Self::unscripted("set_setting"))
    }

    fn apply(&self, changes: &[Change]) -> Result<ApplyResult, SettingsAgentError> {
        let mut state = self.state();
        state.log.push(FakeCall::Apply(changes.to_vec()));
        state
            .apply
            .pop_front()
            .unwrap_or_else(|| Self::unscripted("apply"))
    }

    fn undo(&self) -> Result<UndoResult, SettingsAgentError> {
        let mut state = self.state();
        state.log.push(FakeCall::Undo);
        state
            .undo
            .pop_front()
            .unwrap_or_else(|| Self::unscripted("undo"))
    }

    fn model(&self) -> Option<String> {
        self.state().model.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

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
    fn safe_text_limits_count_characters_not_bytes() {
        assert_eq!(safe_text("日本語日本語", 4), "日本語日…");
        assert_eq!(safe_text(&"é".repeat(64), MAX_VALUE_CHARS), "é".repeat(64));
        assert_eq!(
            safe_text(&"é".repeat(65), MAX_VALUE_CHARS),
            format!("{}…", "é".repeat(64))
        );
    }

    #[test]
    fn shortcut_formatting_and_editing() {
        assert_eq!(format_shortcut("alt_r"), "Hold Right Option");
        assert_eq!(format_shortcut(""), "Off");
        assert_eq!(format_shortcut("<ctrl>+<alt>+1"), "Ctrl+Option+1");
        assert_eq!(format_shortcut("<ctrl>+<alt>+<f5>"), "Ctrl+Option+F5");
        assert_eq!(format_shortcut("f13"), "Hold F13");
        assert_eq!(format_shortcut("<cmd>+<shift>+<space>"), "Cmd+Shift+Space");
        for (saved, editable) in [
            ("alt_l", "left_option"),
            ("alt_r", "right_option"),
            ("cmd_r", "right_command"),
            ("ctrl_r", "right_ctrl"),
            ("f13", "f13"),
            ("<ctrl>+<alt>+1", "ctrl+option+1"),
            ("<ctrl>+<cmd>+v", "ctrl+command+v"),
            ("<cmd>+<shift>+<space>", "command+shift+space"),
            ("<ctrl>+<alt>+<f5>", "ctrl+option+f5"),
        ] {
            assert_eq!(editable_shortcut(&json!(saved)), editable, "{saved}");
        }
        assert_eq!(editable_shortcut(&json!("")), "");
        assert_eq!(editable_shortcut(&Value::Null), "");
        assert_eq!(editable_shortcut(&json!(5)), "");
        // Stored junk is escaped, never trusted.
        assert_eq!(editable_shortcut(&json!("a\nb")), "a\\u000ab");
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

    #[test]
    fn manual_groups_cover_every_allowlist_key_exactly_once() {
        let keys: Vec<&str> = MANUAL_GROUPS
            .iter()
            .flat_map(|(_, k)| k.iter().copied())
            .collect();
        let unique: BTreeSet<&str> = keys.iter().copied().collect();
        assert_eq!(keys.len(), 24);
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
        assert!(CURRENT_KEYS.iter().all(|k| ALLOWLIST.contains(k)));
    }

    #[test]
    fn current_settings_are_labelled_display_values() {
        let current = current_settings_for(&Config::defaults_for(true));
        assert_eq!(current.len(), CURRENT_KEYS.len());
        assert_eq!(current[0], ("Sounds".to_string(), "On".to_string()));
    }

    // ---- change lines / warnings -----------------------------------------

    #[test]
    fn change_lines_are_rebuilt_not_trusted() {
        let sounds = Change::setting("sounds_enabled", json!(true), json!(false));
        assert_eq!(sounds.line(), "Sounds: On → Off");
        let template = Change::setting(
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
        let word = Change::word("Kubernetes");
        assert_eq!(word.line(), "Dictionary: — → add “Kubernetes”");
        let repl = Change::replacement("kay eight ess", "k8s");
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
    fn dictation_switch_and_toggle_shortcut_have_lines_and_warnings() {
        let off = Change::setting("dictation_enabled", json!(true), json!(false));
        assert_eq!(off.line(), "Dictation: On → Off");
        assert_eq!(control_kind("dictation_enabled"), "switch");
        assert_eq!(
            off.warning("Turn off dictation."),
            "Check: turns Dictation off."
        );
        assert_eq!(
            off.warning("Make it quieter"),
            "Check: turns Dictation off; your request doesn't seem to mention this."
        );
        let on = Change::setting("dictation_enabled", json!(false), json!(true));
        assert_eq!(on.line(), "Dictation: Off → On");
        assert_eq!(on.warning("Turn dictation back on"), "");

        let toggle = Change::setting(
            "dictation_toggle_hotkey",
            json!(""),
            json!("<ctrl>+<alt>+d"),
        );
        assert_eq!(control_kind("dictation_toggle_hotkey"), "shortcut");
        assert_eq!(
            toggle.line(),
            format!(
                "Toggle dictation shortcut: Off → {}",
                display_value("hotkey", &json!("<ctrl>+<alt>+d"))
            )
        );
        assert_ne!(toggle.line(), INVALID_LINE);
        assert_eq!(
            toggle.warning("Set the toggle dictation shortcut to ctrl+alt+d"),
            "Check: changes a keyboard shortcut."
        );
        assert_eq!(
            toggle.warning("Change all my shortcuts"),
            "Check: changes a keyboard shortcut."
        );
    }

    #[test]
    fn change_lines_escape_text_and_refuse_blank_or_unchanged_entries() {
        assert_eq!(
            Change::word("a\nb → c").line(),
            "Dictionary: — → add “a\\u000ab \\u2192 c”"
        );
        assert_eq!(Change::word("   ").line(), INVALID_LINE);
        assert_eq!(Change::word("").line(), INVALID_LINE);
        assert_eq!(Change::replacement("Same", "same").line(), INVALID_LINE);
        assert_eq!(Change::replacement("a", "").line(), INVALID_LINE);
        let mut not_text = Change::word("x");
        not_text.value = json!(5);
        assert_eq!(not_text.line(), INVALID_LINE);
        // Lines are bounded like any display text.
        let long = Change::word(&"é".repeat(400)).line();
        assert!(long.chars().count() < 400);
        assert!(long.contains('…'));
    }

    #[test]
    fn warnings_flag_shortcuts_switches_off_and_unmentioned() {
        let off = Change::setting("sounds_enabled", json!(true), json!(false));
        assert_eq!(
            warnings_for(std::slice::from_ref(&off), "turn off sounds"),
            vec!["Sounds: On → Off — Check: turns Sounds off.".to_string()]
        );
        let on = Change::setting("sounds_enabled", json!(false), json!(true));
        assert!(warnings_for(std::slice::from_ref(&on), "turn on the sounds").is_empty());
        assert_eq!(
            warnings_for(&[on], "something else"),
            vec![
                "Sounds: Off → On — Check: your request doesn't seem to mention this.".to_string()
            ]
        );
        let level = Change::setting("cleanup_level", json!("light"), json!("none"));
        assert_eq!(
            level.warning("set cleanup level to none"),
            "Check: turns Cleanup off."
        );
        let hot = Change::setting(
            "polish_hotkey",
            json!("<ctrl>+<alt>+1"),
            json!("<ctrl>+<alt>+9"),
        );
        assert_eq!(
            hot.warning("change polish"),
            "Check: changes a keyboard shortcut."
        );
        assert_eq!(hot.warning(""), "Check: changes a keyboard shortcut.");
        let cmd = Change::setting("command_hotkey", json!("alt_r"), json!("f13"));
        assert!(!cmd.warning("change all shortcuts").contains("doesn't seem"));
        assert!(cmd.warning("change the sounds").contains("doesn't seem"));
    }

    #[test]
    fn dictionary_mentions_need_the_whole_phrase() {
        let know = Change::word("no");
        assert!(!mentions(&know, "i know it"));
        assert!(mentions(&know, "add no to my dictionary"));
        assert!(mentions(&know, "add NO"));
        let aa = Change::word("aa");
        assert!(mentions(&aa, "xaaa aa"));
        assert!(!mentions(&aa, "xaaa"));
        let uni = Change::word("Ünïcødé");
        assert!(mentions(&uni, "learn ünïcødé please"));
        assert!(!mentions(&uni, "xünïcødé"));
        let repl = Change::replacement("kay eight ess", "k8s");
        assert!(mentions(&repl, "say kay eight ess as k8s"));
        assert!(!mentions(&repl, "nothing relevant"));
    }

    // ---- the scripted fake ----------------------------------------------

    fn sounds_proposal(model: &str) -> Proposal {
        Proposal::new(
            "turn off sounds",
            model,
            vec![Change::setting("sounds_enabled", json!(true), json!(false))],
            vec![],
        )
    }

    #[test]
    fn sample_rows_follow_the_page_order_and_the_config() {
        let rows = FakeSettingsAgent::new().manual_settings(None).unwrap();
        assert_eq!(rows.len(), 24);
        assert_eq!(rows[0].key, "dictation_enabled");
        assert_eq!(rows[0].group, "Dictation");
        assert_eq!(rows[0].label, "Dictation");
        assert_eq!(rows[0].kind, "switch");
        assert_eq!(rows[0].display, "On");
        assert_eq!(rows[1].key, "sounds_enabled");
        let toggle = rows
            .iter()
            .find(|r| r.key == "dictation_toggle_hotkey")
            .unwrap();
        assert_eq!(toggle.group, "Shortcuts");
        assert_eq!(toggle.label, "Toggle dictation shortcut");
        assert_eq!(toggle.kind, "shortcut");
        let first_shortcut = rows.iter().position(|r| r.group == "Shortcuts").unwrap();
        assert_eq!(rows[first_shortcut].key, "dictation_toggle_hotkey");
        let level = rows.iter().find(|r| r.key == "cleanup_level").unwrap();
        assert_eq!(level.kind, "choice");
        assert_eq!(
            level.choices,
            [
                ("none".to_string(), "None".to_string()),
                ("light".to_string(), "Light".to_string()),
                ("medium".to_string(), "Medium".to_string())
            ]
        );
        assert!(rows.iter().all(|r| r.valid));
    }

    #[test]
    fn the_fake_returns_only_what_was_scripted_and_records_the_calls() {
        let fake = FakeSettingsAgent::new();
        assert_eq!(fake.model(), None);
        fake.script_proposal(sounds_proposal("gemma4:e4b"));
        let proposal = fake.propose("turn off sounds").unwrap();
        assert_eq!(proposal.changes.len(), 1);
        assert_eq!(fake.model().as_deref(), Some("gemma4:e4b"));
        // Nothing queued any more: it says so instead of inventing an answer.
        let err = fake.propose("again").unwrap_err();
        assert!(err.0.contains("no scripted model answer"));

        let change = proposal.changes[0].clone();
        assert!(fake
            .apply(std::slice::from_ref(&change))
            .unwrap_err()
            .0
            .contains("no scripted apply"));
        fake.script_apply(Ok(ApplyResult {
            applied: vec![change.clone()],
            message: "Applied 1 change.".into(),
        }));
        // The scripted result is returned as is, whatever was passed in.
        let applied = fake.apply(&[]).unwrap();
        assert_eq!(applied.applied, vec![change.clone()]);
        assert_eq!(applied.message, "Applied 1 change.");

        fake.script_set_setting(Err(agent_err("nope")));
        assert_eq!(
            fake.set_setting("hotkey", &json!("x")).unwrap_err().0,
            "nope"
        );
        assert!(fake.undo().unwrap_err().0.contains("no scripted undo"));
        fake.script_undo(Ok(UndoResult {
            reverted: vec!["Sounds: On".into()],
            skipped: vec![],
            message: "Undid the last change.".into(),
        }));
        assert_eq!(fake.undo().unwrap().reverted, ["Sounds: On"]);

        assert_eq!(
            fake.calls(),
            vec![
                FakeCall::Propose("turn off sounds".into()),
                FakeCall::Propose("again".into()),
                FakeCall::Apply(vec![change]),
                FakeCall::Apply(vec![]),
                FakeCall::SetSetting {
                    key: "hotkey".into(),
                    value: json!("x")
                },
                FakeCall::Undo,
                FakeCall::Undo,
            ]
        );
    }

    #[test]
    fn scripted_model_errors_keep_the_model_name() {
        let fake = FakeSettingsAgent::new();
        fake.script_error(Some("m"), "Couldn't reach Ollama.");
        assert_eq!(fake.propose("x").unwrap_err().0, "Couldn't reach Ollama.");
        assert_eq!(fake.model().as_deref(), Some("m"));
        fake.script_error(None, "again");
        let _ = fake.propose("x");
        assert_eq!(fake.model(), None);
    }

    #[test]
    fn load_errors_surface_and_hide_the_undo_summary() {
        let fake = FakeSettingsAgent::new();
        fake.set_undo_lines(Some(vec!["Sounds: Off → On".into()]));
        assert_eq!(
            fake.pending_undo(),
            Some(vec!["Sounds: Off → On".to_string()])
        );
        fake.set_undo_lines(Some(vec![]));
        assert_eq!(fake.pending_undo(), None);
        fake.set_undo_lines(Some(vec!["x".into()]));
        fake.set_load_error(Some("bad toml"));
        assert_eq!(
            fake.load().unwrap_err().0,
            "Couldn't read your settings (bad toml)."
        );
        assert!(fake.manual_settings(None).is_err());
        assert!(fake.current_settings(None).is_err());
        assert_eq!(fake.pending_undo(), None);
        fake.set_load_error(None);
        assert_eq!(
            fake.current_settings(None).unwrap().len(),
            CURRENT_KEYS.len()
        );
    }

    #[test]
    fn arc_handles_share_one_script() {
        let fake = Arc::new(FakeSettingsAgent::new());
        let handle: Box<dyn SettingsAgent> = Box::new(Arc::clone(&fake));
        fake.script_proposal(sounds_proposal("m"));
        assert_eq!(handle.propose("x").unwrap().model, "m");
        assert_eq!(handle.model().as_deref(), Some("m"));
        assert_eq!(fake.calls().len(), 1);
    }
}
