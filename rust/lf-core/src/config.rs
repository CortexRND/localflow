//! Port of `localflow/config.py` and `localflow/config_migrate.py`.
//!
//! [`Config`] stores every Python `Config` dataclass field by its flat field
//! name as a dynamic JSON value, so values the Python loader accepts without a
//! type check (for example an integer `stt.model`) survive a load/save round
//! trip exactly as they do in Python.
//!
//! TOML values JSON cannot express are kept as single-key marker objects:
//! a TOML date/time is `{"$__toml_datetime__": "<toml text>"}` and a non-finite
//! float is `{"$__float__": "nan" | "inf" | "-inf"}`. They behave like their
//! Python counterparts (`datetime` / `float`) in validation and serialization.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Map, Value};

#[cfg(not(windows))]
use crate::home::passwd_home;
pub use crate::home::python_home;
use crate::secrets;

const LOG_TARGET: &str = "localflow.config";

/// Current on-disk config format version.
pub const CONFIG_VERSION: i64 = 2;

/// `[section] key` -> Python dataclass field name, in Python `_SECTIONS` order.
pub const SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "stt",
        &[
            ("provider", "stt_backend"),
            ("model", "model_size"),
            ("language", "language"),
            ("device", "stt_device"),
        ],
    ),
    (
        "llm",
        &[
            ("provider", "llm_provider"),
            ("base_url", "llm_base_url"),
            ("model", "llm_model"),
            ("api_key_ref", "llm_key_ref"),
            ("cleanup_enabled", "cleanup_enabled"),
            ("cleanup_timeout", "cleanup_timeout"),
            ("num_ctx", "cleanup_num_ctx"),
        ],
    ),
    (
        "cleanup",
        &[
            ("level", "cleanup_level"),
            ("styles_enabled", "styles_enabled"),
            ("style_work", "style_work"),
            ("style_email", "style_email"),
            ("style_personal", "style_personal"),
            ("style_other", "style_other"),
            ("app_overrides", "app_category_overrides"),
        ],
    ),
    (
        "hotkey",
        &[
            ("key", "hotkey"),
            ("sounds", "sounds_enabled"),
            ("paste_last", "paste_last_hotkey"),
            ("command", "command_hotkey"),
            ("polish", "polish_hotkey"),
            ("prompt", "prompt_hotkey"),
            ("dictation_toggle", "dictation_toggle_hotkey"),
        ],
    ),
    ("paste", &[("method", "paste_method")]),
    (
        "dictation",
        &[
            ("enabled", "dictation_enabled"),
            ("spoken_symbols", "spoken_symbols"),
            ("spoken_punctuation", "spoken_punctuation"),
            ("silence_threshold_dbfs", "silence_threshold_dbfs"),
            ("command_names", "command_names"),
            ("skills_dirs", "skills_dirs"),
            ("sample_rate", "sample_rate"),
            ("input_device", "input_device"),
            ("max_recording_seconds", "max_recording_seconds"),
            ("pause_media", "pause_media_while_dictating"),
        ],
    ),
    (
        "server",
        &[("host", "server_host"), ("port", "server_port")],
    ),
    (
        "features",
        &[
            ("meetings", "meetings_enabled"),
            ("hot_phrases", "hot_phrases"),
        ],
    ),
    (
        "history",
        &[
            ("enabled", "history_enabled"),
            ("keep_audio_clips", "keep_audio_clips"),
        ],
    ),
    (
        "meetings",
        &[
            ("vault_path", "vault_path"),
            ("notes_folder", "notes_folder"),
            ("logs_folder", "logs_folder"),
            ("watch", "meeting_watch"),
            ("chunk_seconds", "meeting_chunk_seconds"),
            ("min_busy_seconds", "meeting_min_busy_seconds"),
            ("auto_start", "meeting_auto_start"),
            ("work_prompts", "work_prompts"),
            ("fireworks_model", "fireworks_model"),
            ("prompts_dir", "prompts_dir"),
            ("orca_repo", "orca_repo"),
            ("orca_agent", "orca_agent"),
            ("capture_system_audio", "capture_system_audio"),
            ("system_audio_device", "system_audio_device"),
            ("default_template", "meeting_default_template"),
            ("templates_dir", "meeting_templates_dir"),
        ],
    ),
    ("setup", &[("version", "setup_version")]),
];

/// `localflow.platform.base.PASTE_METHODS`.
pub const PASTE_METHODS: &[&str] = &[
    "auto",
    "clipboard",
    "wtype",
    "xdotool",
    "pynput",
    "osascript",
];

const STYLE_VALUES: &[&str] = &["formal", "casual", "very_casual", "excited"];

/// Python `_ENUM_VALUES`.
pub const ENUM_VALUES: &[(&str, &[&str])] = &[
    ("cleanup_level", &["none", "light", "medium"]),
    ("style_work", STYLE_VALUES),
    ("style_email", STYLE_VALUES),
    ("style_personal", STYLE_VALUES),
    ("style_other", STYLE_VALUES),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldType {
    Str,
    Bool,
    Int,
}

/// Python `_FIELD_TYPES` (exact-type checks, `bool` is not an `int`).
const FIELD_TYPES: &[(&str, FieldType)] = &[
    ("dictation_enabled", FieldType::Bool),
    ("dictation_toggle_hotkey", FieldType::Str),
    ("llm_key_ref", FieldType::Str),
    ("paste_last_hotkey", FieldType::Str),
    ("command_hotkey", FieldType::Str),
    ("polish_hotkey", FieldType::Str),
    ("prompt_hotkey", FieldType::Str),
    ("pause_media_while_dictating", FieldType::Bool),
    ("max_recording_seconds", FieldType::Int),
    ("history_enabled", FieldType::Bool),
    ("keep_audio_clips", FieldType::Int),
    ("capture_system_audio", FieldType::Bool),
    ("system_audio_device", FieldType::Str),
    ("meeting_default_template", FieldType::Str),
    ("meeting_templates_dir", FieldType::Str),
];

const DATETIME_MARKER: &str = "$__toml_datetime__";
const FLOAT_MARKER: &str = "$__float__";

/// Raised (inside `anyhow::Error`) for configuration problems Python reports as
/// `ConfigError`: unparsable TOML, a too-new `config_version` and rejected
/// `apply_config_update` payloads. Use `err.downcast_ref::<ConfigError>()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

fn config_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ConfigError(message.into()))
}

/// The Python `Config` dataclass: every field keyed by its flat field name.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub fields: BTreeMap<String, Value>,
}

impl Default for Config {
    /// Python `Config()` on the host OS.
    fn default() -> Self {
        Config::defaults_for(is_darwin())
    }
}

impl Config {
    /// Python `Config()` as it would be constructed with `sys.platform`
    /// equal to `"darwin"` (`darwin = true`) or anything else.
    pub fn defaults_for(darwin: bool) -> Self {
        let paste_last = if darwin {
            "<ctrl>+<cmd>+v"
        } else {
            "<ctrl>+<shift>+v"
        };
        let entries: [(&str, Value); 59] = [
            ("model_size", json!("base")),
            ("stt_backend", json!("auto")),
            ("language", Value::Null),
            ("stt_device", json!("auto")),
            ("llm_provider", json!("ollama")),
            ("llm_base_url", json!("http://localhost:11434")),
            ("llm_model", json!("llama3.2:3b")),
            ("llm_key_ref", json!("")),
            ("cleanup_enabled", json!(true)),
            ("cleanup_level", json!("light")),
            ("styles_enabled", json!(true)),
            ("style_work", json!("formal")),
            ("style_email", json!("formal")),
            ("style_personal", json!("casual")),
            ("style_other", json!("formal")),
            ("app_category_overrides", json!([])),
            ("cleanup_timeout", json!(120)),
            ("cleanup_num_ctx", json!(8192)),
            ("hotkey", json!("alt_l")),
            ("paste_last_hotkey", json!(paste_last)),
            ("command_hotkey", json!("alt_r")),
            ("polish_hotkey", json!("<ctrl>+<alt>+1")),
            ("prompt_hotkey", json!("<ctrl>+<alt>+2")),
            ("sounds_enabled", json!(true)),
            ("paste_method", json!("auto")),
            ("dictation_enabled", json!(true)),
            ("dictation_toggle_hotkey", json!("")),
            ("spoken_symbols", json!(true)),
            ("spoken_punctuation", json!(true)),
            ("silence_threshold_dbfs", json!(-50.0)),
            ("hot_phrases", json!(true)),
            ("command_names", json!([])),
            (
                "skills_dirs",
                json!(["~/.claude/skills", "~/.agents/skills"]),
            ),
            ("sample_rate", json!(16000)),
            ("max_recording_seconds", json!(1200)),
            ("pause_media_while_dictating", json!(false)),
            ("server_host", json!("127.0.0.1")),
            ("server_port", json!(8756)),
            ("meetings_enabled", json!(darwin)),
            ("vault_path", json!("~/projs")),
            ("notes_folder", json!("notes/meetings")),
            ("logs_folder", json!("notes/transcripts")),
            ("meeting_watch", json!(true)),
            ("meeting_chunk_seconds", json!(30)),
            ("meeting_min_busy_seconds", json!(12)),
            ("meeting_auto_start", json!(true)),
            ("work_prompts", json!(true)),
            (
                "fireworks_model",
                json!("accounts/fireworks/models/kimi-k3"),
            ),
            ("prompts_dir", json!("~/projs/prompts/meetings")),
            ("orca_repo", json!("")),
            ("orca_agent", json!("claude")),
            ("capture_system_audio", json!(false)),
            ("system_audio_device", json!("")),
            ("meeting_default_template", json!("general")),
            (
                "meeting_templates_dir",
                json!("~/.localflow/meeting_templates"),
            ),
            ("input_device", json!("")),
            ("setup_version", json!(0)),
            ("history_enabled", json!(true)),
            ("keep_audio_clips", json!(5)),
        ];
        Config {
            fields: entries
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
        }
    }

    // Accessor names deliberately avoid `get_*`: lf-providers' `LlmConfig`
    // trait implements `get_str`/`get_int`/`set_str` for `Config`, and an
    // inherent method of the same name would shadow it.

    /// The value of a field by flat field name (`"llm_provider"`).
    pub fn field(&self, field: &str) -> Option<&Value> {
        self.fields.get(field)
    }

    /// Set a field by flat field name, without validation (dataclass setattr).
    pub fn set_field(&mut self, field: &str, value: Value) {
        self.fields.insert(field.to_string(), value);
    }

    /// The field as a string, when it holds one.
    pub fn field_str(&self, field: &str) -> Option<&str> {
        self.field(field).and_then(Value::as_str)
    }

    /// The field as a bool, when it holds one.
    pub fn field_bool(&self, field: &str) -> Option<bool> {
        self.field(field).and_then(Value::as_bool)
    }

    /// The field as an integer, when it holds a JSON integer.
    pub fn field_i64(&self, field: &str) -> Option<i64> {
        self.field(field).and_then(Value::as_i64)
    }

    /// The field as a float (integers convert), when it holds a finite number.
    pub fn field_f64(&self, field: &str) -> Option<f64> {
        self.field(field).and_then(Value::as_f64)
    }

    /// The field as a list of strings, when every item is a string.
    pub fn field_str_list(&self, field: &str) -> Option<Vec<String>> {
        self.field(field)?
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect()
    }
}

fn is_darwin() -> bool {
    cfg!(target_os = "macos")
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `platformdirs.user_config_dir("localflow", appauthor=False)`.
///
/// Panics only where Python raises `RuntimeError` (no resolvable home
/// directory); use [`try_config_dir`] to handle that case.
pub fn config_dir() -> PathBuf {
    try_config_dir().unwrap_or_else(|err| panic!("{err:#}"))
}

/// Fallible [`config_dir`].
pub fn try_config_dir() -> Result<PathBuf> {
    Ok(platform_config_base()?.join("localflow"))
}

/// `LOCALFLOW_CONFIG` when set to a non-empty value, else
/// `config_dir()/config.toml`. Panics like [`config_dir`].
pub fn config_path() -> PathBuf {
    try_config_path().unwrap_or_else(|err| panic!("{err:#}"))
}

/// Fallible [`config_path`].
pub fn try_config_path() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("LOCALFLOW_CONFIG") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    Ok(try_config_dir()?.join("config.toml"))
}

/// `~/.localflow.toml` (Python `Path.home() / ".localflow.toml"`). Panics
/// like [`config_dir`].
pub fn legacy_config_path() -> PathBuf {
    try_legacy_config_path().unwrap_or_else(|err| panic!("{err:#}"))
}

/// Fallible [`legacy_config_path`].
pub fn try_legacy_config_path() -> Result<PathBuf> {
    Ok(python_home()?.join(".localflow.toml"))
}

#[cfg(windows)]
fn platform_config_base() -> Result<PathBuf> {
    // platformdirs Windows, roaming=False: WIN_PD_OVERRIDE_LOCAL_APPDATA when it
    // is a fully qualified path, else SHGetKnownFolderPath(FOLDERID_LocalAppData);
    // both go through os.path.normpath.
    let override_value = std::env::var("WIN_PD_OVERRIDE_LOCAL_APPDATA").unwrap_or_default();
    let override_value = py_strip(&override_value);
    if win_is_fully_qualified(override_value) {
        return Ok(PathBuf::from(win_normpath(override_value)));
    }
    let folder = dirs::data_local_dir()
        .ok_or_else(|| anyhow!("SHGetKnownFolderPath returned NULL for CSIDL_LOCAL_APPDATA"))?;
    Ok(PathBuf::from(win_normpath(&folder.to_string_lossy())))
}

#[cfg(not(windows))]
fn platform_config_base() -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    // platformdirs XDGMixin: an absolute $XDG_CONFIG_HOME wins on Unix and macOS.
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME") {
        if value.as_bytes().starts_with(b"/") {
            return Ok(PathBuf::from(value));
        }
    }
    let default = if is_darwin() {
        "~/Library/Application Support"
    } else {
        "~/.config"
    };
    platformdirs_expand_user(default)
}

/// platformdirs `_expand_user` for a `~/...` path: a relative or empty `HOME`
/// is treated as unset (the passwd entry is used) and the result must be absolute.
#[cfg(not(windows))]
fn platformdirs_expand_user(path: &str) -> Result<PathBuf> {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    let tail = &path[1..];
    let home = match std::env::var_os("HOME") {
        Some(home) if home.as_bytes().starts_with(b"/") => Some(home.into_vec()),
        _ => passwd_home(),
    };
    let expanded = match home {
        Some(mut home) => {
            while home.last() == Some(&b'/') {
                home.pop();
            }
            home.extend_from_slice(tail.as_bytes());
            if home.is_empty() {
                home.push(b'/');
            }
            home
        }
        None => path.as_bytes().to_vec(),
    };
    if !expanded.starts_with(b"/") {
        bail!(
            "could not determine the home directory for {path:?}, set an absolute HOME or XDG variable"
        );
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(expanded)))
}

/// platformdirs `_is_fully_qualified` (Windows drive or UNC share root).
#[cfg_attr(not(windows), allow(dead_code))]
fn win_is_fully_qualified(path: &str) -> bool {
    let path = path.replace('/', "\\");
    if let Some(rest) = path.strip_prefix("\\\\") {
        let unc_prefix = "?\\UNC\\";
        // Compare bytes: slicing the str could split a multi-byte char.
        let rest = if rest
            .as_bytes()
            .get(..unc_prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(unc_prefix.as_bytes()))
        {
            &rest[unc_prefix.len()..]
        } else {
            rest
        };
        let mut parts = rest.splitn(3, '\\');
        let server = parts.next().unwrap_or("");
        let share = parts.next().unwrap_or("");
        return !server.is_empty() && !share.is_empty();
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
}

/// `ntpath.normpath` for the absolute paths platformdirs produces.
#[cfg_attr(not(windows), allow(dead_code))]
fn win_normpath(path: &str) -> String {
    let path = path.replace('/', "\\");
    let (prefix, rest) = if let Some(rest) = path.strip_prefix("\\\\") {
        // \\server\share is the drive; keep it verbatim.
        let mut parts = rest.splitn(3, '\\');
        let server = parts.next().unwrap_or("");
        let share = parts.next().unwrap_or("");
        let tail = parts.next().unwrap_or("");
        (format!("\\\\{server}\\{share}"), tail.to_string())
    } else if path.len() >= 2 && path.as_bytes()[1] == b':' {
        (path[..2].to_string(), path[2..].to_string())
    } else {
        (String::new(), path.clone())
    };
    let rooted = rest.starts_with('\\') || prefix.starts_with("\\\\");
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('\\') {
        match part {
            "" | "." => {}
            ".." => {
                if matches!(parts.last(), Some(last) if *last != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut result = prefix;
    if rooted {
        result.push('\\');
    }
    result.push_str(&parts.join("\\"));
    if result.is_empty() {
        result.push('.');
    }
    result
}

// ---------------------------------------------------------------------------
// Parsing and loading
// ---------------------------------------------------------------------------

/// Python `Path.exists()`: missing paths are `false`, other OS errors (for
/// example permission denied on a parent) propagate.
fn py_exists(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if exists_ignored_error(&err) => Ok(false),
        Err(err) => Err(anyhow::Error::new(err).context(format!("stat {}", path.display()))),
    }
}

fn exists_ignored_error(err: &io::Error) -> bool {
    // pathlib ignores ENOENT, ENOTDIR, EBADF and ELOOP.
    if matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = err.raw_os_error() {
        return code == libc::EBADF || code == libc::ELOOP || code == libc::ENOTDIR;
    }
    #[cfg(windows)]
    if let Some(code) = err.raw_os_error() {
        // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_NOT_READY,
        // ERROR_INVALID_NAME, ERROR_CANT_RESOLVE_FILENAME (pathlib _IGNORED_WINERRORS)
        return matches!(code, 2 | 3 | 21 | 123 | 1921);
    }
    false
}

/// Parse a TOML config file into a JSON object (Python `_parse`).
///
/// TOML syntax errors are [`ConfigError`]s formatted `"{path}: {message}"`.
pub fn parse_toml_file(path: &Path) -> Result<Map<String, Value>> {
    let bytes = fs::read(path).with_context(|| format!("{}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|err| {
        anyhow!(
            "{}: 'utf-8' codec can't decode the file: {err}",
            path.display()
        )
    })?;
    parse_toml_str(&text).map_err(|message| config_error(format!("{}: {message}", path.display())))
}

/// Parse TOML text into a JSON object; the error is a tomllib-style message.
pub fn parse_toml_str(text: &str) -> std::result::Result<Map<String, Value>, String> {
    let table: toml::Table = text.parse().map_err(|err: toml::de::Error| {
        let message = err.message().trim().to_string();
        match err.span() {
            Some(span) => {
                let (line, column) = line_column(text, span.start);
                format!("{message} (at line {line}, column {column})")
            }
            None => message,
        }
    })?;
    Ok(toml_table_to_json(&table))
}

fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |tail| tail.chars().count())
        + 1;
    (line, column)
}

fn toml_table_to_json(table: &toml::Table) -> Map<String, Value> {
    table
        .iter()
        .map(|(key, value)| (key.clone(), toml_to_json(value)))
        .collect()
}

/// Convert a TOML value to JSON, keeping date/times and non-finite floats as
/// marker objects (see the module docs).
pub fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(text) => Value::String(text.clone()),
        toml::Value::Integer(number) => Value::from(*number),
        toml::Value::Float(number) => float_value(*number),
        toml::Value::Boolean(flag) => Value::Bool(*flag),
        toml::Value::Datetime(datetime) => {
            let mut marker = Map::new();
            marker.insert(
                DATETIME_MARKER.to_string(),
                Value::String(datetime.to_string()),
            );
            Value::Object(marker)
        }
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(toml_table_to_json(table)),
    }
}

fn float_value(number: f64) -> Value {
    match serde_json::Number::from_f64(number) {
        Some(number) => Value::Number(number),
        None => {
            let text = if number.is_nan() {
                "nan"
            } else if number > 0.0 {
                "inf"
            } else {
                "-inf"
            };
            let mut marker = Map::new();
            marker.insert(FLOAT_MARKER.to_string(), Value::String(text.to_string()));
            Value::Object(marker)
        }
    }
}

enum Special<'a> {
    Datetime(&'a str),
    Float(&'a str),
}

fn special(value: &Value) -> Option<Special<'_>> {
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let (key, inner) = object.iter().next()?;
    let text = inner.as_str()?;
    match key.as_str() {
        DATETIME_MARKER => Some(Special::Datetime(text)),
        FLOAT_MARKER if matches!(text, "nan" | "inf" | "-inf") => Some(Special::Float(text)),
        _ => None,
    }
}

fn is_table(value: &Value) -> bool {
    value.is_object() && special(value).is_none()
}

/// Python `type(value) is int`.
fn is_py_int(value: &Value) -> bool {
    matches!(value, Value::Number(number) if !number.is_f64())
}

/// Python `type(value) is float`.
fn is_py_float(value: &Value) -> bool {
    matches!(value, Value::Number(number) if number.is_f64())
        || matches!(special(value), Some(Special::Float(_)))
}

fn field_type(field: &str) -> Option<FieldType> {
    FIELD_TYPES
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, kind)| *kind)
}

fn enum_values(field: &str) -> Option<&'static [&'static str]> {
    ENUM_VALUES
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, values)| *values)
}

fn section_mapping(section: &str) -> Option<&'static [(&'static str, &'static str)]> {
    SECTIONS
        .iter()
        .find(|(name, _)| *name == section)
        .map(|(_, mapping)| *mapping)
}

fn mapping_field(mapping: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    mapping
        .iter()
        .find(|(file_key, _)| *file_key == key)
        .map(|(_, field)| *field)
}

fn enum_rejects(field: &str, value: &Value) -> bool {
    match enum_values(field) {
        Some(values) => !matches!(value, Value::String(text) if values.contains(&text.as_str())),
        None => false,
    }
}

/// Python `_valid_field_value` for fields listed in `_FIELD_TYPES`.
fn valid_field_value(field: &str, value: &Value) -> bool {
    let Some(kind) = field_type(field) else {
        return true;
    };
    let type_ok = match kind {
        FieldType::Str => value.is_string(),
        FieldType::Bool => value.is_boolean(),
        FieldType::Int => is_py_int(value),
    };
    if !type_ok {
        return false;
    }
    if field == "max_recording_seconds" || field == "keep_audio_clips" {
        return match value {
            Value::Number(number) => {
                number.as_u64().is_some() || number.as_i64().is_some_and(|n| n >= 0)
            }
            _ => false,
        };
    }
    if field == "llm_key_ref" {
        return matches!(value, Value::String(text) if text.is_empty() || is_llm_activation_ref(text));
    }
    true
}

/// `llm_activation.is_ref`: `llm_activation_<[a-z][a-z0-9]{0,31}>_<32 lowercase hex>`.
pub fn is_llm_activation_ref(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("llm_activation_") else {
        return false;
    };
    let Some((provider, digest)) = rest.rsplit_once('_') else {
        return false;
    };
    let mut provider_chars = provider.chars();
    let first_ok = provider_chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase());
    first_ok
        && provider.len() <= 32
        && provider_chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && digest.len() == 32
        && digest
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// `llm_activation.endpoint`: stripped, trailing slashes removed; non-strings are "".
pub fn llm_endpoint(url: &Value) -> String {
    match url {
        Value::String(text) => py_strip(text).trim_end_matches('/').to_string(),
        _ => String::new(),
    }
}

fn compare_version(version: &Value) -> Result<bool> {
    let limit = CONFIG_VERSION as f64;
    match version {
        Value::Bool(flag) => Ok(i64::from(*flag) > CONFIG_VERSION),
        Value::Number(number) => {
            if let Some(n) = number.as_i64() {
                Ok(n > CONFIG_VERSION)
            } else if number.as_u64().is_some() {
                Ok(true)
            } else {
                Ok(number.as_f64().is_some_and(|n| n > limit))
            }
        }
        other => match special(other) {
            Some(Special::Float(text)) => Ok(text == "inf"),
            _ => Err(anyhow!(
                "'>' not supported between instances of '{}' and 'int'",
                py_type_name(other)
            )),
        },
    }
}

fn warn(warnings: &mut Vec<String>, message: String) {
    log::warn!(target: LOG_TARGET, "{message}");
    warnings.push(message);
}

/// Python `_config_from_v2` on parsed config data, as on `sys.platform ==
/// "darwin"` when `darwin` is true. Warnings are logged and returned.
pub fn config_from_v2(data: &Map<String, Value>, darwin: bool) -> Result<(Config, Vec<String>)> {
    let mut warnings = Vec::new();
    let config = config_from_v2_inner(data, darwin, &mut warnings)?;
    Ok((config, warnings))
}

fn config_from_v2_inner(
    data: &Map<String, Value>,
    darwin: bool,
    warnings: &mut Vec<String>,
) -> Result<Config> {
    match data.get("config_version") {
        None => warn(
            warnings,
            format!("missing config_version; assuming {CONFIG_VERSION}"),
        ),
        Some(version) => {
            if compare_version(version)? {
                return Err(config_error(format!(
                    "config_version {} is newer than this localflow supports ({CONFIG_VERSION})",
                    py_str(version)
                )));
            }
        }
    }
    let mut config = Config::defaults_for(darwin);
    for (section, values) in data {
        if section == "config_version" {
            continue;
        }
        let Some(mapping) = section_mapping(section) else {
            warn(
                warnings,
                format!("unknown config section {}", py_str_repr(section)),
            );
            continue;
        };
        let Some(values) = values.as_object().filter(|_| is_table(values)) else {
            warn(
                warnings,
                format!("config section {} is not a table", py_str_repr(section)),
            );
            continue;
        };
        for (key, value) in values {
            let Some(field) = mapping_field(mapping, key) else {
                warn(warnings, format!("unknown config key {section}.{key}"));
                continue;
            };
            let mut value = value.clone();
            if field == "language" && value == Value::String(String::new()) {
                value = Value::Null;
            }
            if enum_rejects(field, &value) {
                warn(
                    warnings,
                    format!(
                        "invalid config value for {section}.{key}: {}; using default",
                        py_repr(&value)
                    ),
                );
                continue;
            }
            if field_type(field).is_some() && !valid_field_value(field, &value) {
                if field == "llm_key_ref" {
                    config.set_field("llm_key_ref", json!("llm_activation_invalid"));
                    warn(
                        warnings,
                        "invalid LLM credential reference; activate the provider again".to_string(),
                    );
                    continue;
                }
                warn(
                    warnings,
                    format!(
                        "invalid config value for {section}.{key}: {}; using default",
                        py_repr(&value)
                    ),
                );
                continue;
            }
            config.set_field(field, value);
        }
    }
    if !darwin {
        config.set_field("meetings_enabled", Value::Bool(false));
    }
    Ok(config)
}

/// Load and validate the v2 config file at `path` (Python
/// `_config_from_v2(_parse(path))`). A missing file is an error.
pub fn load_from(path: &Path) -> Result<Config> {
    let data = parse_toml_file(path)?;
    let mut warnings = Vec::new();
    config_from_v2_inner(&data, is_darwin(), &mut warnings)
}

/// Python `load_config()`: the v2 file, else a migrated `~/.localflow.toml`,
/// else defaults (meetings forced off outside macOS).
pub fn load_config() -> Result<Config> {
    let target = try_config_path()?;
    let legacy = try_legacy_config_path()?;
    let mut warnings = Vec::new();
    load_config_at(&target, &legacy, is_darwin(), &mut warnings)
}

fn load_config_at(
    target: &Path,
    legacy: &Path,
    darwin: bool,
    warnings: &mut Vec<String>,
) -> Result<Config> {
    if py_exists(target)? {
        return config_from_v2_inner(&parse_toml_file(target)?, darwin, warnings);
    }
    if py_exists(legacy)? {
        if let Some(migrated) = migrate_legacy_at(legacy, target, darwin, warnings)? {
            return Ok(migrated);
        }
    }
    let mut config = Config::defaults_for(darwin);
    if !darwin {
        config.set_field("meetings_enabled", Value::Bool(false));
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// Legacy migration (config_migrate.py)
// ---------------------------------------------------------------------------

/// Python `config_migrate.migrate_v1`: flat v1 data to nested v2 data plus the
/// legacy `fireworks_api_key` value (whatever type it had), if present.
pub fn migrate_v1(data: &Map<String, Value>) -> (Map<String, Value>, Option<Value>) {
    let mut warnings = Vec::new();
    migrate_v1_inner(data, &mut warnings)
}

fn migrate_v1_inner(
    data: &Map<String, Value>,
    warnings: &mut Vec<String>,
) -> (Map<String, Value>, Option<Value>) {
    let mut migrated = Map::new();
    migrated.insert("config_version".to_string(), json!(2));
    for (key, value) in data {
        if key == "fireworks_api_key" {
            continue;
        }
        let location = match key.as_str() {
            "ollama_url" => Some(("llm", "base_url")),
            "ollama_model" => Some(("llm", "model")),
            _ => field_location(key),
        };
        let Some((section, file_key)) = location else {
            warn(
                warnings,
                format!("ignoring unknown v1 config key {}", py_str_repr(key)),
            );
            continue;
        };
        let entry = migrated
            .entry(section.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(table) = entry {
            table.insert(file_key.to_string(), value.clone());
        }
    }
    if let Some(Value::Object(stt)) = migrated.get_mut("stt") {
        if stt.get("provider") == Some(&json!("mlx")) {
            stt.insert("provider".to_string(), json!("mlx-whisper"));
        }
    }
    (migrated, data.get("fireworks_api_key").cloned())
}

/// `(section, key)` for a flat dataclass field name.
pub fn field_location(field: &str) -> Option<(&'static str, &'static str)> {
    SECTIONS.iter().find_map(|(section, mapping)| {
        mapping
            .iter()
            .find(|(_, name)| *name == field)
            .map(|(file_key, _)| (*section, *file_key))
    })
}

/// Python truthiness of a JSON value.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => match special(value) {
            Some(Special::Float(_)) | Some(Special::Datetime(_)) => true,
            None => !map.is_empty(),
        },
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// Python `migrate_legacy(path)`: migrate `path` (default `~/.localflow.toml`)
/// into the v2 config at [`config_path`]. `Ok(None)` when the source is missing.
///
/// When the legacy `fireworks_api_key` cannot be stored in the keyring and no
/// fireworks key is in the environment, the key is exported as
/// `FIREWORKS_API_KEY` for this process (if unset), nothing is written and the
/// migrated config is returned; rerunning `lf config migrate` finishes the job.
///
/// Compatibility wrapper over [`migrate_legacy_outcome`], which tells a
/// written migration apart from a deferred one.
pub fn migrate_legacy(path: Option<&Path>) -> Result<Option<Config>> {
    Ok(migrate_legacy_outcome(path)?.into_config())
}

/// What [`migrate_legacy_outcome`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum MigrationOutcome {
    /// There is no legacy file at the source path; nothing was done.
    MissingSource,
    /// The migrated config was written to `target`.
    Migrated { config: Config, target: PathBuf },
    /// Nothing was written: the legacy fireworks API key could not be stored
    /// in the keyring and no fireworks key is in the environment. The legacy
    /// file and any existing v2 config are unchanged; the key is exported as
    /// `FIREWORKS_API_KEY` for this process (if unset). `reason` is the
    /// logged warning.
    Deferred { config: Config, reason: String },
}

impl MigrationOutcome {
    /// The migrated config (written or not), as [`migrate_legacy`] returns it.
    pub fn into_config(self) -> Option<Config> {
        match self {
            MigrationOutcome::MissingSource => None,
            MigrationOutcome::Migrated { config, .. }
            | MigrationOutcome::Deferred { config, .. } => Some(config),
        }
    }
}

/// [`migrate_legacy`] with a typed result: missing source, written, or
/// deferred (see [`MigrationOutcome`]).
pub fn migrate_legacy_outcome(path: Option<&Path>) -> Result<MigrationOutcome> {
    let source = match path {
        Some(path) => path.to_path_buf(),
        None => try_legacy_config_path()?,
    };
    if !py_exists(&source)? {
        return Ok(MigrationOutcome::MissingSource);
    }
    let target = try_config_path()?;
    let mut warnings = Vec::new();
    migrate_legacy_outcome_at(&source, &target, is_darwin(), &mut warnings)
}

fn migrate_legacy_at(
    source: &Path,
    target: &Path,
    darwin: bool,
    warnings: &mut Vec<String>,
) -> Result<Option<Config>> {
    Ok(migrate_legacy_outcome_at(source, target, darwin, warnings)?.into_config())
}

fn migrate_legacy_outcome_at(
    source: &Path,
    target: &Path,
    darwin: bool,
    warnings: &mut Vec<String>,
) -> Result<MigrationOutcome> {
    if !py_exists(source)? {
        return Ok(MigrationOutcome::MissingSource);
    }
    let (data, fireworks_key) = migrate_v1_inner(&parse_toml_file(source)?, warnings);
    let config = config_from_v2_inner(&data, darwin, warnings)?;
    if let Some(key) = fireworks_key.filter(py_truthy) {
        let stored = match &key {
            Value::String(text) => secrets::set_secret("fireworks_api_key", text),
            // Python hands the raw value to keyring, whose native backends
            // reject non-strings (the Windows backend would store str(value)).
            other => Err(anyhow::Error::new(secrets::SecretsUnavailable(format!(
                "keyring secrets must be strings, not {}",
                py_type_name(other)
            )))),
        };
        if stored.is_err() {
            if !(env_truthy("LOCALFLOW_FIREWORKS_API_KEY") || env_truthy("FIREWORKS_API_KEY")) {
                if std::env::var_os("FIREWORKS_API_KEY").is_none() {
                    match &key {
                        Value::String(text) => std::env::set_var("FIREWORKS_API_KEY", text),
                        other => bail!("str expected, not {}", py_type_name(other)),
                    }
                }
                let reason = "not migrating ~/.localflow.toml: fireworks_api_key could not be \
                              stored in the keyring; set FIREWORKS_API_KEY in the environment \
                              (or fix the keyring) and rerun `lf config migrate`"
                    .to_string();
                warn(warnings, reason.clone());
                return Ok(MigrationOutcome::Deferred { config, reason });
            }
            warn(
                warnings,
                "keyring unavailable; using the existing fireworks API key environment variable"
                    .to_string(),
            );
        }
    }
    save_config(&config, Some(target))?;
    log::info!(
        target: LOG_TARGET,
        "migrated ~/.localflow.toml to {}; the old file was left in place",
        target.display()
    );
    Ok(MigrationOutcome::Migrated {
        config,
        target: target.to_path_buf(),
    })
}

// ---------------------------------------------------------------------------
// Serialization and saving
// ---------------------------------------------------------------------------

/// Python `_toml_value`.
fn toml_value(value: &Value) -> Result<String> {
    match value {
        Value::Bool(flag) => Ok(if *flag { "true" } else { "false" }.to_string()),
        Value::String(_) | Value::Number(_) | Value::Array(_) => py_json_dumps(value),
        other => match special(other) {
            Some(Special::Float(_)) => py_json_dumps(other),
            _ => bail!("unsupported config value: {}", py_repr(other)),
        },
    }
}

/// Python `config_toml`: every section in order, `None` values omitted, values
/// written as JSON literals.
pub fn config_toml(config: &Config) -> Result<String> {
    let mut lines = vec![format!("config_version = {CONFIG_VERSION}"), String::new()];
    for (section, mapping) in SECTIONS {
        lines.push(format!("[{section}]"));
        for (file_key, field) in mapping.iter() {
            let value = config
                .field(field)
                .ok_or_else(|| anyhow!("'Config' object has no attribute '{field}'"))?;
            if value.is_null() {
                continue;
            }
            lines.push(format!("{file_key} = {}", toml_value(value)?));
        }
        lines.push(String::new());
    }
    Ok(lines.join("\n"))
}

/// Python `save_config`: atomically replace `path` (default [`config_path`])
/// with [`config_toml`] output. The temporary file is created next to the
/// target with owner-only permissions, fsynced and renamed over the target
/// (`MoveFileEx` with replace on Windows); it is removed on failure.
pub fn save_config(config: &Config, path: Option<&Path>) -> Result<PathBuf> {
    let target = match path {
        Some(path) => path.to_path_buf(),
        None => try_config_path()?,
    };
    let text = config_toml(config)?;
    // Python writes in text mode, which translates newlines on Windows.
    let text = if cfg!(windows) {
        text.replace('\n', "\r\n")
    } else {
        text
    };
    write_atomic(&target, text.as_bytes())?;
    Ok(target)
}

fn parent_dir(target: &Path) -> PathBuf {
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Atomic, durable file replacement used for the config file.
pub(crate) fn write_atomic(target: &Path, contents: &[u8]) -> Result<()> {
    let parent = parent_dir(target);
    fs::create_dir_all(&parent).with_context(|| format!("create {}", parent.display()))?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".tmp")
        .tempfile_in(&parent)
        .with_context(|| format!("create a temporary file in {}", parent.display()))?;
    temporary.write_all(contents)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(target).map_err(|err| {
        anyhow::Error::new(err.error).context(format!("replace {}", target.display()))
    })?;
    #[cfg(unix)]
    {
        // Make the rename itself durable. Some filesystems refuse to fsync a
        // directory; the replacement already happened, so that is not an error.
        if let Ok(directory) = fs::File::open(&parent) {
            let _ = directory.sync_all();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Keys, dict view and updates
// ---------------------------------------------------------------------------

/// Python `config_field`: `(section, field_name)` for a dotted `section.key`,
/// or `None` where Python raises `KeyError`.
pub fn config_field(section_key: &str) -> Option<(&'static str, &'static str)> {
    let (section, key) = section_key.split_once('.')?;
    let (section_name, mapping) = SECTIONS.iter().find(|(name, _)| *name == section)?;
    mapping_field(mapping, key).map(|field| (*section_name, field))
}

/// Python `valid_config_keys`: every `section.key`, in table order.
pub fn valid_config_keys() -> Vec<String> {
    SECTIONS
        .iter()
        .flat_map(|(section, mapping)| {
            mapping
                .iter()
                .map(move |(key, _)| format!("{section}.{key}"))
        })
        .collect()
}

/// Every flat dataclass field name, in table order.
pub fn field_names() -> Vec<&'static str> {
    SECTIONS
        .iter()
        .flat_map(|(_, mapping)| mapping.iter().map(|(_, field)| *field))
        .collect()
}

/// Python `config_to_dict`: `{"config_version": 2, section: {key: value}}`.
pub fn config_to_dict(config: &Config) -> Value {
    let mut result = Map::new();
    result.insert("config_version".to_string(), json!(CONFIG_VERSION));
    for (section, mapping) in SECTIONS {
        let values: Map<String, Value> = mapping
            .iter()
            .map(|(file_key, field)| {
                (
                    file_key.to_string(),
                    config.field(field).cloned().unwrap_or(Value::Null),
                )
            })
            .collect();
        result.insert(section.to_string(), Value::Object(values));
    }
    Value::Object(result)
}

/// Python `apply_config_update`: validate a sectioned partial update against
/// the current value types and return the updated copy. An update that moves
/// the LLM endpoint (provider or normalized base URL) clears `llm_key_ref`.
pub fn apply_config_update(config: &Config, update: &Value) -> Result<Config> {
    let update = update
        .as_object()
        .ok_or_else(|| anyhow!("'{}' object has no attribute 'items'", py_type_name(update)))?;
    let mut changes: Map<String, Value> = Map::new();
    for (section, values) in update {
        let mapping = section_mapping(section);
        let (Some(mapping), Some(values)) =
            (mapping, values.as_object().filter(|_| is_table(values)))
        else {
            return Err(config_error(format!("unknown config key '{section}'")));
        };
        for (file_key, value) in values {
            let Some(field) = mapping_field(mapping, file_key) else {
                return Err(config_error(format!(
                    "unknown config key '{section}.{file_key}'"
                )));
            };
            let current = config.field(field).cloned().unwrap_or(Value::Null);
            let valid_type = if field == "language" {
                value.is_null() || value.is_string()
            } else if current.is_boolean() {
                value.is_boolean()
            } else if is_py_int(&current) {
                is_py_int(value)
            } else if is_py_float(&current) {
                is_py_float(value)
            } else if current.is_array() {
                matches!(value, Value::Array(items) if items.iter().all(Value::is_string))
            } else {
                value.is_string()
            };
            if !valid_type {
                return Err(config_error(format!(
                    "invalid type for config key '{section}.{file_key}'"
                )));
            }
            if field_type(field).is_some() && !valid_field_value(field, value) {
                return Err(config_error(format!(
                    "invalid value for config key '{section}.{file_key}'"
                )));
            }
            if section == "paste"
                && file_key == "method"
                && !matches!(value, Value::String(text) if PASTE_METHODS.contains(&text.as_str()))
            {
                return Err(config_error(format!(
                    "invalid paste method: {}",
                    py_repr(value)
                )));
            }
            if enum_rejects(field, value) {
                let mut choices: Vec<&str> = enum_values(field).unwrap_or_default().to_vec();
                choices.sort_unstable();
                return Err(config_error(format!(
                    "invalid value for config key '{section}.{file_key}'; choose from: {}",
                    choices.join(", ")
                )));
            }
            if field == "llm_key_ref" && !(value == &json!("") || value == &current) {
                return Err(config_error(
                    "llm.api_key_ref is set by activating a provider in Models & keys; it can \
                     only be cleared here",
                ));
            }
            changes.insert(field.to_string(), value.clone());
        }
    }
    let old_provider = config.field("llm_provider").cloned().unwrap_or(Value::Null);
    let old_url = config.field("llm_base_url").cloned().unwrap_or(Value::Null);
    let new_provider = changes.get("llm_provider").unwrap_or(&old_provider);
    let new_url = changes.get("llm_base_url").unwrap_or(&old_url);
    let endpoint_changed =
        !py_equal(new_provider, &old_provider) || llm_endpoint(new_url) != llm_endpoint(&old_url);
    if endpoint_changed {
        changes.insert("llm_key_ref".to_string(), json!(""));
    }
    let mut updated = config.clone();
    for (field, value) in changes {
        updated.set_field(&field, value);
    }
    Ok(updated)
}

/// Python `==` for the JSON shapes config values take (numbers compare by value).
fn py_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => match (a.as_i64(), b.as_i64()) {
            (Some(a), Some(b)) => a == b,
            _ => match (a.as_u64(), b.as_u64()) {
                (Some(a), Some(b)) => a == b,
                _ => a.as_f64() == b.as_f64(),
            },
        },
        (Value::Bool(a), Value::Number(_)) => py_equal(&json!(i64::from(*a)), right),
        (Value::Number(_), Value::Bool(b)) => py_equal(left, &json!(i64::from(*b))),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| py_equal(x, y))
        }
        _ => match (special(left), special(right)) {
            (Some(Special::Float("nan")), _) | (_, Some(Special::Float("nan"))) => false,
            _ => left == right,
        },
    }
}

// ---------------------------------------------------------------------------
// Python formatting helpers
// ---------------------------------------------------------------------------

/// Python `str.strip()` (Unicode whitespace as `str.isspace` defines it).
pub(crate) fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => match special(value) {
            Some(Special::Datetime(_)) => "datetime",
            Some(Special::Float(_)) => "float",
            None => "dict",
        },
    }
}

/// Python `repr(float)`: shortest round-trip digits, exponent form outside
/// `1e-4 <= |x| < 1e16`.
pub(crate) fn py_float_repr(number: f64) -> String {
    if number.is_nan() {
        return "nan".to_string();
    }
    if number.is_infinite() {
        return if number > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if number == 0.0 {
        return if number.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_string();
    }
    let formatted = format!("{number:e}");
    let (mantissa, exponent) = formatted.split_once('e').expect("LowerExp has an exponent");
    let exponent: i32 = exponent.parse().expect("LowerExp exponent is an integer");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let decpt = exponent + 1;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if decpt <= -4 || decpt > 16 {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let sign = if exponent < 0 { '-' } else { '+' };
        out.push_str(&format!("e{sign}{:02}", exponent.abs()));
    } else if decpt <= 0 {
        out.push_str("0.");
        out.push_str(&"0".repeat((-decpt) as usize));
        out.push_str(&digits);
    } else if decpt as usize >= digits.len() {
        out.push_str(&digits);
        out.push_str(&"0".repeat(decpt as usize - digits.len()));
        out.push_str(".0");
    } else {
        out.push_str(&digits[..decpt as usize]);
        out.push('.');
        out.push_str(&digits[decpt as usize..]);
    }
    out
}

fn py_number_str(number: &serde_json::Number) -> String {
    if number.is_f64() {
        py_float_repr(number.as_f64().unwrap_or(f64::NAN))
    } else {
        number.to_string()
    }
}

/// Python `json.dumps(value, ensure_ascii=False)`.
pub(crate) fn py_json_dumps(value: &Value) -> Result<String> {
    Ok(match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => if *flag { "true" } else { "false" }.to_string(),
        Value::Number(number) => py_number_str(number),
        Value::String(text) => json_string(text),
        Value::Array(items) => {
            let parts: Result<Vec<String>> = items.iter().map(py_json_dumps).collect();
            format!("[{}]", parts?.join(", "))
        }
        Value::Object(map) => match special(value) {
            Some(Special::Float(text)) => match text {
                "nan" => "NaN",
                "inf" => "Infinity",
                _ => "-Infinity",
            }
            .to_string(),
            Some(Special::Datetime(_)) => {
                bail!("Object of type datetime is not JSON serializable")
            }
            None => {
                let parts: Result<Vec<String>> = map
                    .iter()
                    .map(|(key, item)| {
                        Ok(format!("{}: {}", json_string(key), py_json_dumps(item)?))
                    })
                    .collect();
                format!("{{{}}}", parts?.join(", "))
            }
        },
    })
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn py_str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python `repr()` of a config value (used in warnings and error messages).
pub(crate) fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
        Value::Number(number) => py_number_str(number),
        Value::String(text) => py_str_repr(text),
        Value::Array(items) => {
            format!(
                "[{}]",
                items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
            )
        }
        Value::Object(map) => match special(value) {
            Some(Special::Float(text)) => text.to_string(),
            Some(Special::Datetime(text)) => text.to_string(),
            None => format!(
                "{{{}}}",
                map.iter()
                    .map(|(key, item)| format!("{}: {}", py_str_repr(key), py_repr(item)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

/// Python `str()` of a config value.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => py_repr(other),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::secrets::{self, FailingKeyring, KeyringBackend, MemoryKeyring, SERVICE};
    use crate::test_support::{EnvGuard, GLOBAL_LOCK};

    #[test]
    fn win_is_fully_qualified_unicode_unc_does_not_panic() {
        // 'é' straddles byte 6 of the post-`\\` remainder.
        assert!(win_is_fully_qualified("\\\\abcéé\\share\\config"));
        assert!(win_is_fully_qualified("//abcéé/share/config"));
        assert!(win_is_fully_qualified("\\\\ééé\\ßhare"));
        assert!(!win_is_fully_qualified("\\\\abcéé"));
        assert!(!win_is_fully_qualified("\\\\abcéé\\"));
    }

    #[test]
    fn win_is_fully_qualified_drive_and_unc_forms() {
        assert!(win_is_fully_qualified("C:\\Users\\x"));
        assert!(win_is_fully_qualified("c:/Users/x"));
        assert!(!win_is_fully_qualified("C:"));
        assert!(!win_is_fully_qualified("C:relative"));
        assert!(!win_is_fully_qualified("\\Users\\x"));
        assert!(!win_is_fully_qualified("relative\\x"));
        assert!(!win_is_fully_qualified(""));
        assert!(win_is_fully_qualified("\\\\server\\share"));
        assert!(win_is_fully_qualified("\\\\server\\share\\dir"));
        assert!(!win_is_fully_qualified("\\\\server"));
        assert!(!win_is_fully_qualified("\\\\server\\"));
        assert!(!win_is_fully_qualified("\\\\\\share"));
        assert!(!win_is_fully_qualified("\\\\"));
        // Extended UNC prefix, matched case-insensitively.
        assert!(win_is_fully_qualified("\\\\?\\UNC\\server\\share\\x"));
        assert!(win_is_fully_qualified("\\\\?\\unc\\server\\share"));
        assert!(win_is_fully_qualified("//?/Unc/server/share"));
        assert!(win_is_fully_qualified("\\\\?\\UNC\\sérvér\\shäre"));
        assert!(!win_is_fully_qualified("\\\\?\\UNC\\server"));
        assert!(!win_is_fully_qualified("\\\\?\\UNC\\"));
    }

    const DEFAULT_LINUX_TOML: &str = "config_version = 2\n\n[stt]\nprovider = \"auto\"\nmodel = \"base\"\ndevice = \"auto\"\n\n[llm]\nprovider = \"ollama\"\nbase_url = \"http://localhost:11434\"\nmodel = \"llama3.2:3b\"\napi_key_ref = \"\"\ncleanup_enabled = true\ncleanup_timeout = 120\nnum_ctx = 8192\n\n[cleanup]\nlevel = \"light\"\nstyles_enabled = true\nstyle_work = \"formal\"\nstyle_email = \"formal\"\nstyle_personal = \"casual\"\nstyle_other = \"formal\"\napp_overrides = []\n\n[hotkey]\nkey = \"alt_l\"\nsounds = true\npaste_last = \"<ctrl>+<shift>+v\"\ncommand = \"alt_r\"\npolish = \"<ctrl>+<alt>+1\"\nprompt = \"<ctrl>+<alt>+2\"\ndictation_toggle = \"\"\n\n[paste]\nmethod = \"auto\"\n\n[dictation]\nenabled = true\nspoken_symbols = true\nspoken_punctuation = true\nsilence_threshold_dbfs = -50.0\ncommand_names = []\nskills_dirs = [\"~/.claude/skills\", \"~/.agents/skills\"]\nsample_rate = 16000\ninput_device = \"\"\nmax_recording_seconds = 1200\npause_media = false\n\n[server]\nhost = \"127.0.0.1\"\nport = 8756\n\n[features]\nmeetings = false\nhot_phrases = true\n\n[history]\nenabled = true\nkeep_audio_clips = 5\n\n[meetings]\nvault_path = \"~/projs\"\nnotes_folder = \"notes/meetings\"\nlogs_folder = \"notes/transcripts\"\nwatch = true\nchunk_seconds = 30\nmin_busy_seconds = 12\nauto_start = true\nwork_prompts = true\nfireworks_model = \"accounts/fireworks/models/kimi-k3\"\nprompts_dir = \"~/projs/prompts/meetings\"\norca_repo = \"\"\norca_agent = \"claude\"\ncapture_system_audio = false\nsystem_audio_device = \"\"\ndefault_template = \"general\"\ntemplates_dir = \"~/.localflow/meeting_templates\"\n\n[setup]\nversion = 0\n";

    const VALID_REF: &str = "llm_activation_openai_0123456789abcdef0123456789abcdef";

    struct Isolated {
        _dir: tempfile::TempDir,
        target: PathBuf,
        legacy: PathBuf,
    }

    impl Isolated {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("config.toml");
            let legacy = dir.path().join(".localflow.toml");
            Isolated {
                _dir: dir,
                target,
                legacy,
            }
        }

        fn load(&self, darwin: bool) -> Result<(Config, Vec<String>)> {
            let mut warnings = Vec::new();
            let config = load_config_at(&self.target, &self.legacy, darwin, &mut warnings)?;
            Ok((config, warnings))
        }

        fn write(&self, text: &str) {
            fs::write(&self.target, text).unwrap();
        }
    }

    struct BackendGuard(Option<Arc<dyn KeyringBackend>>);

    impl BackendGuard {
        fn install(backend: Arc<dyn KeyringBackend>) -> Self {
            BackendGuard(secrets::set_keyring_backend(Some(backend)))
        }
    }

    impl Drop for BackendGuard {
        fn drop(&mut self) {
            secrets::set_keyring_backend(self.0.take());
        }
    }

    fn with(config: &Config, values: &[(&str, Value)]) -> Config {
        let mut config = config.clone();
        for (field, value) in values {
            assert!(config.field(field).is_some(), "unknown field {field}");
            config.set_field(field, value.clone());
        }
        config
    }

    fn config_err(err: &anyhow::Error) -> String {
        err.downcast_ref::<ConfigError>()
            .unwrap_or_else(|| panic!("not a ConfigError: {err:#}"))
            .0
            .clone()
    }

    #[test]
    fn defaults_when_no_file() {
        let iso = Isolated::new();
        let (loaded, warnings) = iso.load(false).unwrap();
        assert_eq!(loaded, Config::defaults_for(false));
        assert!(warnings.is_empty());
        assert!(!iso.target.exists());
        // Off macOS Config() already has meetings off; on macOS the default is on.
        assert_eq!(
            Config::defaults_for(true).field_bool("meetings_enabled"),
            Some(true)
        );
        assert_eq!(iso.load(true).unwrap().0, Config::defaults_for(true));
    }

    #[test]
    fn host_default_matches_platform() {
        let config = Config::default();
        assert_eq!(config, Config::defaults_for(cfg!(target_os = "macos")));
        let paste_last = if cfg!(target_os = "macos") {
            "<ctrl>+<cmd>+v"
        } else {
            "<ctrl>+<shift>+v"
        };
        assert_eq!(config.field_str("paste_last_hotkey"), Some(paste_last));
    }

    #[test]
    fn config_table_covers_every_dataclass_field() {
        let defaults = Config::defaults_for(false);
        let names = field_names();
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len());
        assert_eq!(names.len(), 59);
        let default_names: std::collections::BTreeSet<&str> =
            defaults.fields.keys().map(String::as_str).collect();
        assert_eq!(unique, default_names);
        for (field, _) in FIELD_TYPES {
            assert!(unique.contains(field));
        }
        for (field, _) in ENUM_VALUES {
            assert!(unique.contains(field));
        }
    }

    #[test]
    fn default_toml_is_byte_identical_to_python() {
        assert_eq!(
            config_toml(&Config::defaults_for(false)).unwrap(),
            DEFAULT_LINUX_TOML
        );
        let darwin = config_toml(&Config::defaults_for(true)).unwrap();
        assert!(darwin.contains("paste_last = \"<ctrl>+<cmd>+v\"\n"));
        assert!(darwin.contains("[features]\nmeetings = true\n"));
    }

    #[test]
    fn toml_output_for_unusual_values_matches_python() {
        let config = with(
            &Config::defaults_for(false),
            &[
                ("language", json!("en")),
                ("silence_threshold_dbfs", json!(1e16)),
                ("command_names", json!(["a\"b", "é\n"])),
                ("model_size", json!(5)),
            ],
        );
        let text = config_toml(&config).unwrap();
        assert!(text.contains(
            "[stt]\nprovider = \"auto\"\nmodel = 5\nlanguage = \"en\"\ndevice = \"auto\"\n"
        ));
        assert!(text.contains("silence_threshold_dbfs = 1e+16\n"));
        assert!(text.contains("command_names = [\"a\\\"b\", \"é\\n\"]\n"));
    }

    #[test]
    fn python_float_repr() {
        let cases = [
            (-50.0, "-50.0"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (123.456, "123.456"),
            (1.5e-7, "1.5e-07"),
            (2.5e20, "2.5e+20"),
            (-0.0, "-0.0"),
            (-45.5, "-45.5"),
        ];
        for (value, expected) in cases {
            assert_eq!(py_float_repr(value), expected, "{value}");
        }
    }

    #[test]
    fn v2_round_trip_for_non_default_config() {
        let iso = Isolated::new();
        let expected = with(
            &Config::defaults_for(true),
            &[
                ("model_size", json!("small")),
                ("stt_backend", json!("mlx-whisper")),
                ("language", json!("en")),
                ("stt_device", json!("cpu")),
                ("llm_provider", json!("openai-compatible")),
                ("llm_base_url", json!("https://example.test/v1")),
                ("llm_model", json!("model")),
                ("llm_key_ref", json!(VALID_REF)),
                ("cleanup_enabled", json!(false)),
                ("cleanup_level", json!("medium")),
                ("styles_enabled", json!(false)),
                ("style_work", json!("excited")),
                ("style_email", json!("very_casual")),
                ("style_personal", json!("formal")),
                ("style_other", json!("casual")),
                ("app_category_overrides", json!(["com.example.editor=work"])),
                ("cleanup_timeout", json!(17)),
                ("cleanup_num_ctx", json!(4096)),
                ("hotkey", json!("<cmd>+<shift>+<space>")),
                ("sounds_enabled", json!(false)),
                ("paste_method", json!("clipboard")),
                ("spoken_symbols", json!(false)),
                ("spoken_punctuation", json!(false)),
                ("silence_threshold_dbfs", json!(-45.5)),
                ("command_names", json!(["one", "two"])),
                ("skills_dirs", json!(["/skills"])),
                ("sample_rate", json!(8000)),
                ("server_host", json!("0.0.0.0")),
                ("server_port", json!(9000)),
                ("meetings_enabled", json!(true)),
                ("vault_path", json!("/vault")),
                ("notes_folder", json!("meetings")),
                ("logs_folder", json!("transcripts")),
                ("meeting_watch", json!(false)),
                ("meeting_chunk_seconds", json!(15)),
                ("meeting_min_busy_seconds", json!(4)),
                ("meeting_auto_start", json!(false)),
                ("work_prompts", json!(false)),
                ("fireworks_model", json!("fireworks")),
                ("prompts_dir", json!("/prompts")),
                ("orca_repo", json!("path:/repo")),
                ("orca_agent", json!("agent")),
                ("input_device", json!("Studio Mic")),
                ("setup_version", json!(1)),
                ("command_hotkey", json!("alt_r")),
                ("pause_media_while_dictating", json!(true)),
                ("max_recording_seconds", json!(600)),
                ("capture_system_audio", json!(true)),
                ("system_audio_device", json!("BlackHole 2ch")),
                ("meeting_default_template", json!("standup")),
                ("meeting_templates_dir", json!("/templates")),
                ("history_enabled", json!(false)),
                ("keep_audio_clips", json!(0)),
                ("hot_phrases", json!(false)),
            ],
        );
        assert_eq!(
            save_config(&expected, Some(&iso.target)).unwrap(),
            iso.target
        );
        let (loaded, warnings) = iso.load(true).unwrap();
        assert_eq!(loaded, expected);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn save_is_atomic_private_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("config.toml");
        save_config(&Config::defaults_for(false), Some(&target)).unwrap();
        fs::write(&target, "stale").unwrap();
        save_config(&Config::defaults_for(false), Some(&target)).unwrap();
        let expected = if cfg!(windows) {
            DEFAULT_LINUX_TOML.replace('\n', "\r\n")
        } else {
            DEFAULT_LINUX_TOML.to_string()
        };
        assert_eq!(fs::read_to_string(&target).unwrap(), expected);
        let entries: Vec<_> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("config.toml")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn save_failure_leaves_target_and_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        let bad = with(
            &Config::defaults_for(false),
            &[("model_size", json!({"a": 1}))],
        );
        let err = save_config(&bad, Some(&target)).unwrap_err();
        assert!(format!("{err}").contains("unsupported config value"));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        // Replacing a directory fails; the temporary file is cleaned up.
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), "x").unwrap();
        assert!(save_config(&Config::defaults_for(false), Some(&target)).is_err());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.toml")]);
    }

    #[test]
    fn unknown_section_and_key_are_warned() {
        let iso = Isolated::new();
        iso.write("[stt]\nmodel = \"tiny\"\nunknown = true\n[unknown]\nvalue = 1\n");
        let (loaded, warnings) = iso.load(false).unwrap();
        assert_eq!(loaded.field_str("model_size"), Some("tiny"));
        assert!(warnings
            .iter()
            .any(|w| w == "unknown config key stt.unknown"));
        assert!(warnings
            .iter()
            .any(|w| w == "unknown config section 'unknown'"));
    }

    #[test]
    fn non_table_section_is_warned() {
        let iso = Isolated::new();
        iso.write(
            "config_version = 2\nstt = 5\nserver = 1979-05-27\n[[paste]]\nmethod = \"clipboard\"\n",
        );
        let (loaded, warnings) = iso.load(false).unwrap();
        assert_eq!(loaded, Config::defaults_for(false));
        assert!(warnings.contains(&"config section 'stt' is not a table".to_string()));
        assert!(warnings.contains(&"config section 'server' is not a table".to_string()));
        assert!(warnings.contains(&"config section 'paste' is not a table".to_string()));
    }

    #[test]
    fn invalid_cleanup_enums_are_logged_and_defaulted() {
        let iso = Isolated::new();
        iso.write(
            "[cleanup]\nlevel = \"aggressive\"\nstyle_work = \"wild\"\nstyle_personal = 12\n",
        );
        let (loaded, warnings) = iso.load(false).unwrap();
        assert_eq!(loaded.field_str("cleanup_level"), Some("light"));
        assert_eq!(loaded.field_str("style_work"), Some("formal"));
        assert_eq!(loaded.field_str("style_personal"), Some("casual"));
        let count = warnings
            .iter()
            .filter(|w| w.starts_with("invalid config value for cleanup."))
            .count();
        assert_eq!(count, 3);
        assert!(warnings.contains(
            &"invalid config value for cleanup.level: 'aggressive'; using default".to_string()
        ));
        assert!(warnings.contains(
            &"invalid config value for cleanup.style_personal: 12; using default".to_string()
        ));
    }

    #[test]
    fn malformed_toml_raises_config_error() {
        let iso = Isolated::new();
        iso.write("[stt\n");
        let err = iso.load(false).unwrap_err();
        let message = config_err(&err);
        assert!(
            message.starts_with(&format!("{}: ", iso.target.display())),
            "{message}"
        );
        assert!(message.contains("(at line 1, column"), "{message}");
    }

    #[test]
    fn duplicate_keys_are_config_errors() {
        let iso = Isolated::new();
        iso.write("[stt]\nmodel = \"a\"\nmodel = \"b\"\n");
        config_err(&iso.load(false).unwrap_err());
    }

    #[test]
    fn invalid_utf8_is_not_a_config_error() {
        let iso = Isolated::new();
        fs::write(&iso.target, [0x5b, 0xff, 0x5d]).unwrap();
        let err = iso.load(false).unwrap_err();
        assert!(err.downcast_ref::<ConfigError>().is_none());
    }

    #[test]
    fn newer_config_version_raises_config_error() {
        let iso = Isolated::new();
        iso.write("config_version = 3\n");
        assert_eq!(
            config_err(&iso.load(false).unwrap_err()),
            "config_version 3 is newer than this localflow supports (2)"
        );
        iso.write("config_version = 2.5\n");
        assert_eq!(
            config_err(&iso.load(false).unwrap_err()),
            "config_version 2.5 is newer than this localflow supports (2)"
        );
        iso.write("config_version = inf\n");
        assert!(
            config_err(&iso.load(false).unwrap_err()).starts_with("config_version inf is newer")
        );
    }

    #[test]
    fn older_or_odd_config_versions() {
        let iso = Isolated::new();
        for text in [
            "config_version = 1\n",
            "config_version = true\n",
            "config_version = nan\n",
            "config_version = 2.0\n",
        ] {
            iso.write(text);
            let (loaded, warnings) = iso.load(false).unwrap();
            assert_eq!(loaded, Config::defaults_for(false), "{text}");
            assert!(warnings.is_empty(), "{text}");
        }
        // Python compares with `>`, which a str cannot do: a TypeError, not ConfigError.
        iso.write("config_version = \"2\"\n");
        let err = iso.load(false).unwrap_err();
        assert!(err.downcast_ref::<ConfigError>().is_none());
        assert!(format!("{err}").contains("'>' not supported between instances of 'str' and 'int'"));
    }

    #[test]
    fn missing_config_version_warns_and_loads() {
        let iso = Isolated::new();
        iso.write("[server]\nport = 9001\n");
        let (loaded, warnings) = iso.load(false).unwrap();
        assert_eq!(loaded.field_i64("server_port"), Some(9001));
        assert_eq!(
            warnings,
            vec!["missing config_version; assuming 2".to_string()]
        );
    }

    #[test]
    fn empty_file_loads_defaults() {
        let iso = Isolated::new();
        iso.write("");
        let (loaded, _) = iso.load(false).unwrap();
        assert_eq!(loaded, Config::defaults_for(false));
    }

    #[test]
    fn empty_language_maps_to_none() {
        let iso = Isolated::new();
        iso.write("[stt]\nlanguage = \"\"\n");
        assert_eq!(
            iso.load(false).unwrap().0.field("language"),
            Some(&Value::Null)
        );
        // None is omitted from the TOML output.
        let text = config_toml(&Config::defaults_for(false)).unwrap();
        assert!(!text.contains("language"));
    }

    #[test]
    fn unusual_types_accepted_by_python_loader_survive() {
        let iso = Isolated::new();
        iso.write(
            "config_version = 2\n[stt]\nmodel = 5\nlanguage = [\"en\"]\n[server]\nport = \"9000\"\n\
             [dictation]\nsilence_threshold_dbfs = -45\nskills_dirs = \"one\"\n[setup]\nversion = true\n",
        );
        let (loaded, warnings) = iso.load(false).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(loaded.field("model_size"), Some(&json!(5)));
        assert_eq!(loaded.field("language"), Some(&json!(["en"])));
        assert_eq!(loaded.field("server_port"), Some(&json!("9000")));
        assert_eq!(loaded.field("silence_threshold_dbfs"), Some(&json!(-45)));
        assert_eq!(loaded.field("skills_dirs"), Some(&json!("one")));
        assert_eq!(loaded.field("setup_version"), Some(&json!(true)));
        save_config(&loaded, Some(&iso.target)).unwrap();
        assert_eq!(iso.load(false).unwrap().0, loaded);

        // apply_config_update follows the current (unusual) type, like Python.
        let updated = apply_config_update(&loaded, &json!({"stt": {"model": 7}})).unwrap();
        assert_eq!(updated.field("model_size"), Some(&json!(7)));
        let err = apply_config_update(&loaded, &json!({"stt": {"model": "tiny"}})).unwrap_err();
        assert_eq!(config_err(&err), "invalid type for config key 'stt.model'");
        let err = apply_config_update(
            &loaded,
            &json!({"dictation": {"silence_threshold_dbfs": -40.0}}),
        )
        .unwrap_err();
        assert_eq!(
            config_err(&err),
            "invalid type for config key 'dictation.silence_threshold_dbfs'"
        );
    }

    #[test]
    fn toml_datetimes_and_non_finite_floats() {
        let iso = Isolated::new();
        iso.write(
            "[stt]\nmodel = 1979-05-27T07:32:00Z\n[dictation]\nsilence_threshold_dbfs = nan\n",
        );
        let (loaded, _) = iso.load(false).unwrap();
        // Python json.dumps writes NaN for a float('nan').
        let without_datetime = with(&loaded, &[("model_size", json!("base"))]);
        assert!(config_toml(&without_datetime)
            .unwrap()
            .contains("silence_threshold_dbfs = NaN\n"));
        // A datetime value cannot be serialized (Python raises TypeError).
        let err = config_toml(&loaded).unwrap_err();
        assert!(format!("{err}").starts_with("unsupported config value: 1979-05-27T07:32:00Z"));
        // A nan float field accepts float updates, not ints.
        assert!(apply_config_update(
            &loaded,
            &json!({"dictation": {"silence_threshold_dbfs": -40.0}})
        )
        .is_ok());
        assert!(apply_config_update(
            &loaded,
            &json!({"dictation": {"silence_threshold_dbfs": -40}})
        )
        .is_err());
        // A datetime current value only accepts strings.
        assert!(apply_config_update(&loaded, &json!({"stt": {"model": "tiny"}})).is_ok());
        // Lists holding a datetime fail inside json.dumps.
        let listed = with(
            &without_datetime,
            &[("command_names", json!([{DATETIME_MARKER: "07:32:00"}]))],
        );
        assert!(format!("{}", config_toml(&listed).unwrap_err()).contains("not JSON serializable"));
    }

    #[test]
    fn llm_key_ref_fails_closed_on_load() {
        let iso = Isolated::new();
        iso.write(&format!("[llm]\napi_key_ref = \"{VALID_REF}\"\n"));
        assert_eq!(
            iso.load(false).unwrap().0.field_str("llm_key_ref"),
            Some(VALID_REF)
        );
        for bad in [
            "\"llm_api_key\"",
            "7",
            "\"llm_activation_Openai_0123456789abcdef0123456789abcdef\"",
            "\"llm_activation_openai_0123456789ABCDEF0123456789abcdef\"",
            "\"llm_activation_openai_0123456789abcdef0123456789abcde\"",
            "\"llm_activation__0123456789abcdef0123456789abcdef\"",
        ] {
            iso.write(&format!("[llm]\napi_key_ref = {bad}\n"));
            let (loaded, warnings) = iso.load(false).unwrap();
            assert_eq!(
                loaded.field_str("llm_key_ref"),
                Some("llm_activation_invalid"),
                "{bad}"
            );
            assert!(warnings.contains(
                &"invalid LLM credential reference; activate the provider again".to_string()
            ));
        }
        iso.write("[llm]\napi_key_ref = \"\"\n");
        assert_eq!(
            iso.load(false).unwrap().0.field_str("llm_key_ref"),
            Some("")
        );
    }

    #[test]
    fn activation_ref_shape() {
        assert!(is_llm_activation_ref(VALID_REF));
        assert!(is_llm_activation_ref(&format!(
            "llm_activation_{}_{}",
            "a".repeat(32),
            "0".repeat(32)
        )));
        assert!(!is_llm_activation_ref(&format!(
            "llm_activation_{}_{}",
            "a".repeat(33),
            "0".repeat(32)
        )));
        assert!(!is_llm_activation_ref(&format!(
            "llm_activation_1a_{}",
            "0".repeat(32)
        )));
        assert!(!is_llm_activation_ref(&format!(
            "llm_activation_a_b_{}",
            "0".repeat(32)
        )));
        assert!(!is_llm_activation_ref(""));
    }

    #[test]
    fn meetings_enabled_platform_override_on_load_only() {
        let iso = Isolated::new();
        save_config(
            &with(
                &Config::defaults_for(true),
                &[("meetings_enabled", json!(true))],
            ),
            Some(&iso.target),
        )
        .unwrap();
        assert_eq!(
            iso.load(true).unwrap().0.field_bool("meetings_enabled"),
            Some(true)
        );
        assert_eq!(
            iso.load(false).unwrap().0.field_bool("meetings_enabled"),
            Some(false)
        );
        // apply_config_update does not force it off.
        let updated = apply_config_update(
            &Config::defaults_for(false),
            &json!({"features": {"meetings": true}}),
        )
        .unwrap();
        assert_eq!(updated.field_bool("meetings_enabled"), Some(true));
    }

    #[test]
    fn invalid_new_config_fields_default_when_loaded() {
        let iso = Isolated::new();
        for contents in [
            "[dictation]\nenabled = 1\n",
            "[dictation]\nenabled = \"false\"\n",
            "[hotkey]\ndictation_toggle = 7\n",
            "[hotkey]\ncommand = 7\n",
            "[dictation]\npause_media = \"yes\"\n",
            "[dictation]\nmax_recording_seconds = -1\n",
            "[dictation]\nmax_recording_seconds = true\n",
            "[dictation]\nmax_recording_seconds = 1.5\n",
            "[history]\nenabled = \"yes\"\n",
            "[history]\nkeep_audio_clips = -1\n",
            "[meetings]\ncapture_system_audio = 1\n",
        ] {
            iso.write(contents);
            let (loaded, warnings) = iso.load(false).unwrap();
            assert!(
                warnings
                    .iter()
                    .any(|w| w.starts_with("invalid config value")),
                "{contents}"
            );
            assert_eq!(loaded, Config::defaults_for(false), "{contents}");
        }
    }

    #[test]
    fn hotkey_and_dictation_fields_validate_and_round_trip() {
        let iso = Isolated::new();
        let expected = with(
            &Config::defaults_for(false),
            &[
                ("paste_last_hotkey", json!("<ctrl>+<cmd>+v")),
                ("dictation_enabled", json!(false)),
                ("dictation_toggle_hotkey", json!("<ctrl>+<alt>+d")),
                ("command_hotkey", json!("")),
                ("pause_media_while_dictating", json!(true)),
                ("max_recording_seconds", json!(0)),
            ],
        );
        save_config(&expected, Some(&iso.target)).unwrap();
        assert_eq!(iso.load(false).unwrap().0, expected);
        let updated = apply_config_update(
            &Config::defaults_for(false),
            &json!({"hotkey": {"command": ""}, "dictation": {"pause_media": true, "max_recording_seconds": 0}}),
        )
        .unwrap();
        assert_eq!(
            updated.field_bool("pause_media_while_dictating"),
            Some(true)
        );
        assert_eq!(updated.field_str("command_hotkey"), Some(""));
    }

    #[test]
    fn history_config_defaults_round_trip_and_validation() {
        let iso = Isolated::new();
        let defaults = Config::defaults_for(false);
        assert_eq!(defaults.field_bool("history_enabled"), Some(true));
        assert_eq!(defaults.field_i64("keep_audio_clips"), Some(5));
        let expected = with(
            &defaults,
            &[
                ("history_enabled", json!(false)),
                ("keep_audio_clips", json!(0)),
            ],
        );
        save_config(&expected, Some(&iso.target)).unwrap();
        assert_eq!(iso.load(false).unwrap().0, expected);
        let updated = apply_config_update(
            &defaults,
            &json!({"history": {"enabled": false, "keep_audio_clips": 3}}),
        )
        .unwrap();
        assert_eq!(updated.field_bool("history_enabled"), Some(false));
        assert_eq!(updated.field_i64("keep_audio_clips"), Some(3));
    }

    #[test]
    fn config_to_dict_and_partial_update() {
        let original = Config::defaults_for(false);
        let serialized = config_to_dict(&original);
        assert_eq!(serialized["config_version"], json!(2));
        assert_eq!(serialized["stt"]["model"], json!("base"));
        assert_eq!(serialized["stt"]["language"], Value::Null);
        assert_eq!(serialized["llm"]["provider"], json!("ollama"));
        let sections: Vec<&String> = serialized.as_object().unwrap().keys().collect();
        assert_eq!(sections[0], "config_version");
        assert_eq!(sections[1], "stt");
        assert_eq!(sections.last().unwrap().as_str(), "setup");

        let updated = apply_config_update(
            &original,
            &json!({"stt": {"model": "small", "language": null}, "server": {"port": 9000}}),
        )
        .unwrap();
        assert_eq!(updated.field_str("model_size"), Some("small"));
        assert_eq!(updated.field("language"), Some(&Value::Null));
        assert_eq!(updated.field_i64("server_port"), Some(9000));
        assert_eq!(original.field_i64("server_port"), Some(8756));

        let english = apply_config_update(&original, &json!({"stt": {"language": "en"}})).unwrap();
        assert_eq!(english.field_str("language"), Some("en"));
        let cleared = apply_config_update(&english, &json!({"stt": {"language": ""}})).unwrap();
        assert_eq!(cleared.field_str("language"), Some(""));
        assert_eq!(
            apply_config_update(&original, &json!({})).unwrap(),
            original
        );
    }

    #[test]
    fn apply_config_update_validates() {
        let cases = [
            (json!({"stt": {"missing": "x"}}), "unknown config key 'stt.missing'"),
            (json!({"missing": {"key": "x"}}), "unknown config key 'missing'"),
            (json!({"stt": "small"}), "unknown config key 'stt'"),
            (json!({"server": {"port": "9000"}}), "invalid type for config key 'server.port'"),
            (json!({"server": {"port": true}}), "invalid type for config key 'server.port'"),
            (json!({"server": {"port": 9000.0}}), "invalid type for config key 'server.port'"),
            (json!({"cleanup": {"styles_enabled": 1}}), "invalid type for config key 'cleanup.styles_enabled'"),
            (json!({"dictation": {"silence_threshold_dbfs": -40}}), "invalid type for config key 'dictation.silence_threshold_dbfs'"),
            (json!({"dictation": {"skills_dirs": ["a", 1]}}), "invalid type for config key 'dictation.skills_dirs'"),
            (json!({"stt": {"model": null}}), "invalid type for config key 'stt.model'"),
            (json!({"stt": {"language": 5}}), "invalid type for config key 'stt.language'"),
            (json!({"paste": {"method": "invalid"}}), "invalid paste method: 'invalid'"),
            (
                json!({"cleanup": {"level": "aggressive"}}),
                "invalid value for config key 'cleanup.level'; choose from: light, medium, none",
            ),
            (json!({"cleanup": {"level": []}}), "invalid type for config key 'cleanup.level'"),
            (
                json!({"cleanup": {"style_work": "wild"}}),
                "invalid value for config key 'cleanup.style_work'; choose from: casual, excited, formal, very_casual",
            ),
            (json!({"dictation": {"max_recording_seconds": -1}}), "invalid value for config key 'dictation.max_recording_seconds'"),
            (json!({"history": {"keep_audio_clips": -1}}), "invalid value for config key 'history.keep_audio_clips'"),
            (json!({"llm": {"api_key_ref": "llm_api_key"}}), "invalid value for config key 'llm.api_key_ref'"),
            (
                json!({"llm": {"api_key_ref": VALID_REF}}),
                "llm.api_key_ref is set by activating a provider in Models & keys; it can only be cleared here",
            ),
        ];
        for (update, message) in cases {
            let err = apply_config_update(&Config::defaults_for(false), &update).unwrap_err();
            assert_eq!(config_err(&err), message, "{update}");
        }
        // A non-object payload is a type error in Python, not a ConfigError.
        let err = apply_config_update(&Config::defaults_for(false), &json!([1])).unwrap_err();
        assert!(err.downcast_ref::<ConfigError>().is_none());
    }

    #[test]
    fn apply_update_clears_binding_only_on_endpoint_change() {
        let bound = with(
            &Config::defaults_for(false),
            &[
                ("llm_provider", json!("openai-compatible")),
                ("llm_base_url", json!("https://api.example/v1")),
                ("llm_key_ref", json!(VALID_REF)),
            ],
        );
        // Same ref and cosmetic URL changes keep the binding.
        for update in [
            json!({"llm": {"api_key_ref": VALID_REF}}),
            json!({"llm": {"base_url": " https://api.example/v1/ "}}),
            json!({"llm": {"provider": "openai-compatible", "model": "other"}}),
            json!({"stt": {"model": "small"}}),
        ] {
            let updated = apply_config_update(&bound, &update).unwrap();
            assert_eq!(
                updated.field_str("llm_key_ref"),
                Some(VALID_REF),
                "{update}"
            );
        }
        for update in [
            json!({"llm": {"base_url": "https://other.example/v1"}}),
            json!({"llm": {"provider": "ollama"}}),
            json!({"llm": {"api_key_ref": ""}}),
        ] {
            let updated = apply_config_update(&bound, &update).unwrap();
            assert_eq!(updated.field_str("llm_key_ref"), Some(""), "{update}");
        }
        // An explicit same-ref write together with an endpoint move still clears.
        let updated = apply_config_update(
            &bound,
            &json!({"llm": {"api_key_ref": VALID_REF, "base_url": "https://other.example"}}),
        )
        .unwrap();
        assert_eq!(updated.field_str("llm_key_ref"), Some(""));
    }

    #[test]
    fn config_field_and_valid_keys() {
        assert_eq!(config_field("server.port"), Some(("server", "server_port")));
        assert_eq!(
            config_field("llm.api_key_ref"),
            Some(("llm", "llm_key_ref"))
        );
        assert_eq!(config_field("bad.key"), None);
        assert_eq!(config_field("server"), None);
        assert_eq!(config_field("server.port.extra"), None);
        assert_eq!(config_field(""), None);
        let keys = valid_config_keys();
        assert_eq!(keys.len(), 59);
        assert_eq!(keys[0], "stt.provider");
        assert_eq!(keys.last().unwrap(), "setup.version");
        assert_eq!(
            field_location("pause_media_while_dictating"),
            Some(("dictation", "pause_media"))
        );
    }

    #[test]
    fn config_path_honours_override() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut env = EnvGuard::new();
        env.set("LOCALFLOW_CONFIG", "/tmp/somewhere/custom.toml");
        assert_eq!(config_path(), PathBuf::from("/tmp/somewhere/custom.toml"));
        env.set("LOCALFLOW_CONFIG", "");
        assert_eq!(config_path(), config_dir().join("config.toml"));
        env.remove("LOCALFLOW_CONFIG");
        assert_eq!(config_path(), config_dir().join("config.toml"));
        assert_eq!(config_dir().file_name().unwrap(), "localflow");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_config_dir_follows_platformdirs() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut env = EnvGuard::new();
        env.set("XDG_CONFIG_HOME", "/xdg/config");
        assert_eq!(config_dir(), PathBuf::from("/xdg/config/localflow"));
        env.set("XDG_CONFIG_HOME", "relative/config");
        env.set("HOME", "/home/someone/");
        assert_eq!(
            config_dir(),
            PathBuf::from("/home/someone/.config/localflow")
        );
        env.remove("XDG_CONFIG_HOME");
        assert_eq!(
            config_dir(),
            PathBuf::from("/home/someone/.config/localflow")
        );
        assert_eq!(
            legacy_config_path(),
            PathBuf::from("/home/someone/.localflow.toml")
        );
        env.set("HOME", "/");
        assert_eq!(config_dir(), PathBuf::from("/.config/localflow"));
        assert_eq!(legacy_config_path(), PathBuf::from("/.localflow.toml"));
        // A relative HOME is ignored by platformdirs (passwd entry is used).
        env.set("HOME", "relative-home");
        let dir = try_config_dir().unwrap();
        assert!(
            dir.is_absolute() && dir.ends_with(".config/localflow"),
            "{}",
            dir.display()
        );
    }

    #[test]
    fn windows_path_helpers() {
        assert!(win_is_fully_qualified("C:\\Users\\me\\AppData\\Local"));
        assert!(win_is_fully_qualified("c:/x"));
        assert!(win_is_fully_qualified("\\\\server\\share\\x"));
        assert!(win_is_fully_qualified("\\\\?\\UNC\\server\\share"));
        assert!(!win_is_fully_qualified("C:relative"));
        assert!(!win_is_fully_qualified("\\rooted"));
        assert!(!win_is_fully_qualified("\\\\server"));
        assert!(!win_is_fully_qualified(""));
        assert_eq!(
            win_normpath("C:/Users/me/./AppData/../Local/"),
            "C:\\Users\\me\\Local"
        );
        assert_eq!(
            win_normpath("\\\\server\\share\\a\\..\\b"),
            "\\\\server\\share\\b"
        );
    }

    #[test]
    fn migrate_v1_maps_fields() {
        let data = parse_toml_str(
            "stt_backend = \"mlx\"\nollama_url = \"http://o\"\nmystery = 1\nfireworks_api_key = \"k\"\nserver_port = 1\n",
        )
        .unwrap();
        let (migrated, key) = migrate_v1(&data);
        assert_eq!(
            Value::Object(migrated),
            json!({
                "config_version": 2,
                "stt": {"provider": "mlx-whisper"},
                "llm": {"base_url": "http://o"},
                "server": {"port": 1}
            })
        );
        assert_eq!(key, Some(json!("k")));
        let (_, key) = migrate_v1(&Map::new());
        assert_eq!(key, None);
        // Later keys for the same location win, like Python dict assignment.
        let data = parse_toml_str("llm_base_url = \"a\"\nollama_url = \"b\"\n").unwrap();
        assert_eq!(migrate_v1(&data).0["llm"]["base_url"], json!("b"));
    }

    #[test]
    fn migration_maps_every_v1_field_and_stores_secret() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let memory = Arc::new(MemoryKeyring::new());
        let _keyring = BackendGuard::install(memory.clone());
        let iso = Isolated::new();
        fs::write(
            &iso.legacy,
            r#"
model_size = "small"
stt_backend = "mlx"
language = "en"
ollama_url = "http://ollama"
ollama_model = "model"
cleanup_enabled = false
cleanup_timeout = 30
cleanup_num_ctx = 2048
hotkey = "space"
sounds_enabled = false
paste_method = "clipboard"
spoken_symbols = false
command_names = ["go"]
skills_dirs = ["/skills"]
sample_rate = 8000
server_host = "0.0.0.0"
server_port = 9999
meetings_enabled = true
vault_path = "/vault"
notes_folder = "notes"
logs_folder = "logs"
meeting_watch = false
meeting_chunk_seconds = 10
meeting_min_busy_seconds = 5
meeting_auto_start = false
work_prompts = false
fireworks_api_key = "secret"
fireworks_model = "fw"
prompts_dir = "/prompts"
orca_repo = "repo"
orca_agent = "agent"
"#,
        )
        .unwrap();

        let (loaded, _) = iso.load(false).unwrap();

        assert_eq!(loaded.field_str("stt_backend"), Some("mlx-whisper"));
        assert_eq!(loaded.field_str("llm_base_url"), Some("http://ollama"));
        assert_eq!(loaded.field_str("llm_model"), Some("model"));
        assert_eq!(loaded.field_str("fireworks_model"), Some("fw"));
        assert_eq!(loaded.field_i64("server_port"), Some(9999));
        assert_eq!(loaded.field_bool("meetings_enabled"), Some(false));
        assert_eq!(
            memory.value(SERVICE, "fireworks_api_key").as_deref(),
            Some("secret")
        );
        let written = fs::read_to_string(&iso.target).unwrap();
        assert!(!written.contains("fireworks_api_key"));
        assert!(!written.contains("secret"));
        assert!(iso.legacy.exists());
        assert_eq!(iso.load(false).unwrap().0, loaded);
    }

    #[test]
    fn second_load_does_not_remigrate() {
        let iso = Isolated::new();
        fs::write(&iso.legacy, "model_size = \"tiny\"\n").unwrap();
        iso.load(false).unwrap();
        let before = fs::read(&iso.legacy).unwrap();
        // Change the legacy file: a re-migration would pick this up.
        fs::write(&iso.legacy, "model_size = \"large\"\n").unwrap();
        assert_eq!(
            iso.load(false).unwrap().0.field_str("model_size"),
            Some("tiny")
        );
        assert_ne!(before, fs::read(&iso.legacy).unwrap());
    }

    #[test]
    fn migration_keeps_secret_in_process_when_keyring_unavailable() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _keyring = BackendGuard::install(Arc::new(FailingKeyring("no backend".into())));
        let mut env = EnvGuard::new();
        env.remove("FIREWORKS_API_KEY");
        env.remove("LOCALFLOW_FIREWORKS_API_KEY");
        let iso = Isolated::new();
        fs::write(
            &iso.legacy,
            "fireworks_api_key = \"secret\"\nmodel_size = \"tiny\"\n",
        )
        .unwrap();

        let (loaded, warnings) = iso.load(false).unwrap();

        assert_eq!(loaded.field_str("model_size"), Some("tiny"));
        assert!(!iso.target.exists());
        assert!(iso.legacy.exists());
        assert_eq!(secrets::get_secret("fireworks_api_key"), "secret");
        assert!(warnings
            .iter()
            .any(|w| w.starts_with("not migrating ~/.localflow.toml")));
        secrets::reset_keyring_warning();
    }

    #[test]
    fn migration_does_not_override_present_but_empty_variable() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _keyring = BackendGuard::install(Arc::new(FailingKeyring("no backend".into())));
        let mut env = EnvGuard::new();
        env.set("FIREWORKS_API_KEY", "");
        env.remove("LOCALFLOW_FIREWORKS_API_KEY");
        let iso = Isolated::new();
        fs::write(&iso.legacy, "fireworks_api_key = \"secret\"\n").unwrap();

        iso.load(false).unwrap();

        // os.environ.setdefault leaves an existing (empty) value alone.
        assert_eq!(std::env::var("FIREWORKS_API_KEY").unwrap(), "");
        assert!(!iso.target.exists());
        secrets::reset_keyring_warning();
    }

    #[test]
    fn migration_with_existing_secret_environment_writes_v2() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _keyring = BackendGuard::install(Arc::new(FailingKeyring("no backend".into())));
        let mut env = EnvGuard::new();
        env.set("FIREWORKS_API_KEY", "existing");
        env.remove("LOCALFLOW_FIREWORKS_API_KEY");
        let iso = Isolated::new();
        fs::write(&iso.legacy, "fireworks_api_key = \"legacy\"\n").unwrap();

        let (_, warnings) = iso.load(false).unwrap();

        assert!(iso.target.exists());
        assert_eq!(std::env::var("FIREWORKS_API_KEY").unwrap(), "existing");
        assert!(warnings.contains(
            &"keyring unavailable; using the existing fireworks API key environment variable"
                .to_string()
        ));
        secrets::reset_keyring_warning();
    }

    #[test]
    fn migration_with_empty_or_non_string_key() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _keyring = BackendGuard::install(Arc::new(FailingKeyring("no backend".into())));
        let mut env = EnvGuard::new();
        env.remove("FIREWORKS_API_KEY");
        env.remove("LOCALFLOW_FIREWORKS_API_KEY");
        let iso = Isolated::new();
        // A falsy key is skipped entirely: the v2 file is written.
        fs::write(&iso.legacy, "fireworks_api_key = \"\"\n").unwrap();
        iso.load(false).unwrap();
        assert!(iso.target.exists());

        // A truthy non-string key cannot be stored or exported (Python TypeError).
        let iso = Isolated::new();
        fs::write(&iso.legacy, "fireworks_api_key = 5\n").unwrap();
        let err = iso.load(false).unwrap_err();
        assert!(format!("{err}").contains("str expected, not int"));
        assert!(!iso.target.exists());
        secrets::reset_keyring_warning();
    }

    #[test]
    fn migrate_legacy_missing_source_is_none() {
        let iso = Isolated::new();
        let mut warnings = Vec::new();
        assert!(
            migrate_legacy_at(&iso.legacy, &iso.target, false, &mut warnings)
                .unwrap()
                .is_none()
        );
        assert!(!iso.target.exists());
    }

    #[test]
    fn migrate_legacy_outcome_is_deferred_when_keyring_fails() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _keyring = BackendGuard::install(Arc::new(FailingKeyring("no backend".into())));
        let mut env = EnvGuard::new();
        env.remove("FIREWORKS_API_KEY");
        env.remove("LOCALFLOW_FIREWORKS_API_KEY");
        let iso = Isolated::new();
        env.set("LOCALFLOW_CONFIG", &iso.target);
        let legacy_text = "fireworks_api_key = \"secret\"\nmodel_size = \"tiny\"\n";
        fs::write(&iso.legacy, legacy_text).unwrap();

        // No v2 target.
        let outcome = migrate_legacy_outcome(Some(&iso.legacy)).unwrap();
        let MigrationOutcome::Deferred { config, reason } = &outcome else {
            panic!("not deferred: {outcome:?}");
        };
        assert_eq!(config.field_str("model_size"), Some("tiny"));
        assert!(reason.starts_with("not migrating ~/.localflow.toml"));
        assert!(!iso.target.exists());
        assert_eq!(fs::read_to_string(&iso.legacy).unwrap(), legacy_text);
        // The env fallback is unchanged: the key is exported for this process.
        assert_eq!(std::env::var("FIREWORKS_API_KEY").unwrap(), "secret");

        // An older v2 target exists: still deferred, target untouched.
        env.remove("FIREWORKS_API_KEY");
        let existing = "config_version = 2\n\n[stt]\nmodel = \"small\"\n";
        iso.write(existing);
        let outcome = migrate_legacy_outcome(Some(&iso.legacy)).unwrap();
        assert!(
            matches!(outcome, MigrationOutcome::Deferred { .. }),
            "{outcome:?}"
        );
        assert_eq!(fs::read_to_string(&iso.target).unwrap(), existing);
        assert_eq!(fs::read_to_string(&iso.legacy).unwrap(), legacy_text);

        // The compatibility wrapper still returns the (unwritten) config.
        env.remove("FIREWORKS_API_KEY");
        let compat = migrate_legacy(Some(&iso.legacy)).unwrap().unwrap();
        assert_eq!(compat.field_str("model_size"), Some("tiny"));
        assert_eq!(fs::read_to_string(&iso.target).unwrap(), existing);
        secrets::reset_keyring_warning();
    }

    #[test]
    fn migrate_legacy_outcome_missing_and_migrated() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let iso = Isolated::new();
        let mut env = EnvGuard::new();
        env.set("LOCALFLOW_CONFIG", &iso.target);
        assert_eq!(
            migrate_legacy_outcome(Some(&iso.legacy)).unwrap(),
            MigrationOutcome::MissingSource
        );
        fs::write(&iso.legacy, "server_port = 1234\n").unwrap();
        match migrate_legacy_outcome(Some(&iso.legacy)).unwrap() {
            MigrationOutcome::Migrated { config, target } => {
                assert_eq!(target, iso.target);
                assert_eq!(config.field_i64("server_port"), Some(1234));
                assert_eq!(load_from(&iso.target).unwrap(), config);
            }
            other => panic!("not migrated: {other:?}"),
        }
    }

    #[test]
    fn malformed_legacy_file_is_a_config_error() {
        let iso = Isolated::new();
        fs::write(&iso.legacy, "model_size = \n").unwrap();
        config_err(&iso.load(false).unwrap_err());
    }

    #[test]
    fn public_migrate_legacy_writes_to_config_path() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let iso = Isolated::new();
        let mut env = EnvGuard::new();
        env.set("LOCALFLOW_CONFIG", &iso.target);
        fs::write(&iso.legacy, "server_port = 1234\n").unwrap();
        let migrated = migrate_legacy(Some(&iso.legacy)).unwrap().unwrap();
        assert_eq!(migrated.field_i64("server_port"), Some(1234));
        assert_eq!(load_from(&iso.target).unwrap(), migrated);
        assert!(load_from(&iso.legacy.with_extension("missing")).is_err());
        assert!(migrate_legacy(Some(&iso.legacy.with_extension("missing")))
            .unwrap()
            .is_none());
    }

    #[test]
    fn python_json_dumps_and_repr() {
        assert_eq!(
            py_json_dumps(&json!(["a", 1, 1.5, true, null, {"k": [2]}])).unwrap(),
            "[\"a\", 1, 1.5, true, null, {\"k\": [2]}]"
        );
        assert_eq!(
            py_json_dumps(&json!("\u{1}\u{8}\u{c}é\u{7f}/")).unwrap(),
            "\"\\u0001\\b\\fé\u{7f}/\""
        );
        assert_eq!(
            py_repr(&json!(["it's", true, null])),
            "[\"it's\", True, None]"
        );
        assert_eq!(py_repr(&json!("a'b\"c")), "'a\\'b\"c'");
    }
}
