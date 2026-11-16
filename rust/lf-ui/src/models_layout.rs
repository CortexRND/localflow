//! Pure state rules and geometry for the Models & keys page (no AppKit); port
//! of `localflow/models_layout.py`.
//!
//! Status, "which provider is active", import-tick defaults and every frame
//! live here so they can be tested without macOS; the platform layer only
//! renders them. Things that depend on the machine are injected:
//!
//! * secrets/config through [`lf_providers::ProviderKeys`] (in-memory fakes in
//!   tests, the keyring and config file in the app),
//! * which speech engines can run here through an `available` closure,
//! * saving the config and the explicit speech-retry request through closures,
//! * the Ollama listing through [`OllamaClient`].
//!
//! Nothing here caches a result, so an unavailable engine is never reported as
//! available.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use lf_core::config::Config;
use lf_providers::keys::{ImportCandidate, Preset, PRESETS};
use lf_providers::ollama::{OllamaLlm, DEFAULT_NUM_CTX};
use lf_providers::{LlmProvider, ModelInfo, ProviderError, ProviderKeys};
use serde_json::{json, Value};

use crate::pystr::PyStr;
use crate::text_wrap::wrap_lines;
use crate::tokens::{
    CARD_PADDING, CONTROL_GAP, MODELS_FIELD_HEIGHT, MODELS_LABEL_HEIGHT, MODELS_ROW_GAP,
    MODELS_ROW_HEIGHT,
};

pub const ACTION_H: f64 = 28.0;
pub const SAVE_W: f64 = 64.0;
pub const REMOVE_W: f64 = 80.0;
pub const USE_W: f64 = 190.0;
pub const TEST_W: f64 = 140.0;
pub const ROW_BUTTON_W: f64 = 72.0;
pub const EDITOR_TOP: f64 = 8.0;
pub const EDITOR_BOTTOM: f64 = 12.0;
pub const MESSAGE_LINE: f64 = 16.0;
pub const IMPORT_ROW_H: f64 = 28.0;
/// Card title + detail line.
pub const IMPORT_HEADER_H: f64 = 48.0;

/// `(x, y, w, h)`.
pub type Frame = (f64, f64, f64, f64);

fn pad() -> f64 {
    f64::from(CARD_PADDING)
}
fn gap() -> f64 {
    f64::from(CONTROL_GAP)
}
fn row_gap() -> f64 {
    f64::from(MODELS_ROW_GAP)
}
fn field_h() -> f64 {
    f64::from(MODELS_FIELD_HEIGHT)
}

// ----------------------------------------------------------- config access ---

fn cfg_str<'a>(config: &'a Config, field: &str) -> &'a str {
    config.field_str(field).unwrap_or("")
}

fn with_fields(config: &Config, changes: &[(&str, Value)]) -> Config {
    let mut updated = config.clone();
    for (field, value) in changes {
        updated.set_field(field, value.clone());
    }
    updated
}

// ------------------------------------------------------------ active rules ---

fn same_url(left: &str, right: &str) -> bool {
    left.py_strip().trim_end_matches('/') == right.py_strip().trim_end_matches('/')
}

/// True only when the configured provider AND endpoint are this preset's.
///
/// `url` is the endpoint to compare against (the typed one for the custom
/// preset); `None` means the preset's own URL. The custom preset is active
/// when the configured endpoint is not one of the named presets'.
pub fn is_active(preset: &Preset, config: &Config, url: Option<&str>) -> bool {
    if cfg_str(config, "llm_provider") != preset.llm_provider {
        return false;
    }
    let configured = cfg_str(config, "llm_base_url");
    if preset.id == "custom" {
        if let Some(url) = url {
            return !url.py_strip().is_empty() && same_url(configured, url);
        }
        let named = PRESETS.iter().filter(|p| p.id != "custom").any(|p| {
            p.base_url.trim_end_matches('/') == configured.py_strip().trim_end_matches('/')
        });
        return !configured.py_strip().is_empty() && !named;
    }
    same_url(configured, url.unwrap_or(preset.base_url))
}

/// The first preset that is active for `config`.
pub fn active_preset(config: &Config) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| is_active(p, config, None))
}

/// `(tint token, text token)` of a status pill.
pub type PillTokens = (&'static str, &'static str);

/// `(label, (tint token, text token))` for a row's status pill.
pub fn provider_status(
    preset: &Preset,
    config: &Config,
    has_key: bool,
) -> (&'static str, PillTokens) {
    if is_active(preset, config, None) {
        ("Active", ("cobalt", "cobalt"))
    } else if !preset.needs_key {
        ("Local", ("line", "ink"))
    } else if has_key {
        ("Key saved", ("success", "success"))
    } else {
        ("No key", ("muted", "muted"))
    }
}

/// The single primary action: hidden once the editor matches what is active.
///
/// `key_changed`: a saved key is newer than the one in use (see
/// `ProviderKeys::key_needs_activation`); it keeps Use available.
pub fn show_use_button(
    preset: &Preset,
    config: &Config,
    url: &str,
    model: &str,
    key_changed: bool,
) -> bool {
    key_changed
        || !(is_active(preset, config, Some(url))
            && model.py_strip() == cfg_str(config, "llm_model").py_strip())
}

/// [`show_use_button`] plus the key comparison, made only when the provider
/// is otherwise active and unchanged (no secret lookup for other rows).
///
/// A blank model is never offered: activation would refuse it (or quietly
/// pick a default the user did not choose), so Use stays hidden until a model
/// is typed or picked. Errors from the key comparison (keyring/config
/// failures) are returned, as Python raises them.
pub fn use_button_visible(
    preset: &Preset,
    config: &Config,
    url: &str,
    model: &str,
    keys: &ProviderKeys<'_, Config>,
) -> anyhow::Result<bool> {
    if model.py_strip().is_empty() {
        return Ok(false);
    }
    if show_use_button(preset, config, url, model, false) {
        return Ok(true);
    }
    let changed = keys.key_needs_activation(preset.id, Some(config))?;
    Ok(show_use_button(preset, config, url, model, changed))
}

/// Friendly names for the configured speech provider (`[stt] provider`).
/// Display only: nothing is resolved or loaded.
pub const STT_LABELS: [(&str, &str); 5] = [
    ("auto", "Automatic"),
    ("faster-whisper", "Faster Whisper"),
    ("mlx-whisper", "MLX Whisper"),
    ("mlx", "MLX Whisper"),
    ("parakeet", "Parakeet"),
];
/// Parakeet runs one fixed model and ignores `[stt] model`.
pub const STT_FIXED_MODEL: [(&str, &str); 1] = [("parakeet", "TDT 0.6B v2")];

fn stt_label(key: &str) -> Option<&'static str> {
    STT_LABELS.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn stt_fixed_model(key: &str) -> Option<&'static str> {
    STT_FIXED_MODEL
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
}

/// `'Parakeet / TDT 0.6B v2'`, `'MLX Whisper / small'`, `'Automatic / base'`.
///
/// Reads only the configured provider and model size. `auto` stays
/// "Automatic": which engine auto picks is only known once the model loads.
/// `stt_device` plays no part. Unknown (entry-point) ids are shown as is.
pub fn speech_summary(config: &Config) -> String {
    let provider = current_provider_text(config);
    let lower = provider.to_lowercase();
    let label = stt_label(&lower).unwrap_or(&provider);
    let model = stt_fixed_model(&lower)
        .map(str::to_string)
        .or_else(|| Some(cfg_str(config, "model_size").to_string()).filter(|m| !m.is_empty()));
    match model {
        Some(model) => format!("{label} / {model}"),
        None => label.to_string(),
    }
}

/// `(config.stt_backend or "auto").strip()`.
fn current_provider_text(config: &Config) -> String {
    let raw = cfg_str(config, "stt_backend");
    let value = if raw.is_empty() { "auto" } else { raw };
    value.py_strip().to_string()
}

/// `(speech text, cleanup & Ask text)`: plain-text names of the models in use.
pub fn summary_values(config: &Config) -> (String, String) {
    let speech = speech_summary(config);
    let provider = match active_preset(config) {
        Some(preset) => preset.label.to_string(),
        None => cfg_str(config, "llm_provider").to_string(),
    };
    let model = cfg_str(config, "llm_model");
    let model = if model.is_empty() {
        "not selected"
    } else {
        model
    };
    (speech, format!("{provider} / {model}"))
}

/// Tick state key: `(provider id, source)`.
pub type ImportKey = (String, String);

/// Tick state per candidate: ON when its provider has no saved key, OFF when
/// it already has one; a tick the user changed (`overrides`) always wins.
pub fn import_states(
    candidates: &[ImportCandidate],
    has_key: &dyn Fn(&str) -> bool,
    overrides: &BTreeMap<ImportKey, bool>,
) -> BTreeMap<ImportKey, bool> {
    let mut states = BTreeMap::new();
    for candidate in candidates {
        let key = (candidate.provider_id.clone(), candidate.source.clone());
        let state = match overrides.get(&key) {
            Some(value) => *value,
            None => !has_key(&candidate.provider_id),
        };
        states.insert(key, state);
    }
    states
}

/// Columns of one list row.
#[derive(Clone, Debug, PartialEq)]
pub struct RowColumns {
    /// `(x, w)` of the name + URL column.
    pub text: (f64, f64),
    /// x of the status pill.
    pub pill: f64,
    /// `(x, w)` of the Set up / Edit button.
    pub button: (f64, f64),
}

/// Name+URL column, status pill and Set up/Edit button of one list row.
pub fn row_columns(width: f64, pill_w: f64) -> RowColumns {
    let button_x = width - pad() - ROW_BUTTON_W;
    let pill_x = button_x - gap() - pill_w;
    RowColumns {
        text: (pad(), (pill_x - gap() - pad()).max(0.0)),
        pill: pill_x,
        button: (button_x, ROW_BUTTON_W),
    }
}

/// Frames of the provider editor, only for the controls this provider needs.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorRows {
    pub url_label: Option<Frame>,
    pub url_field: Option<Frame>,
    pub key_label: Option<Frame>,
    pub key_field: Option<Frame>,
    pub key_save: Option<Frame>,
    pub key_remove: Option<Frame>,
    pub model_label: Frame,
    pub model_field: Frame,
    pub use_button: Option<Frame>,
    pub test: Frame,
    pub message: Option<Frame>,
}

impl EditorRows {
    /// Present frames by their Python dict key, in insertion order.
    pub fn frames(&self) -> Vec<(&'static str, Frame)> {
        let optional = [
            ("url_label", self.url_label),
            ("url_field", self.url_field),
            ("key_label", self.key_label),
            ("key_field", self.key_field),
            ("key_save", self.key_save),
            ("key_remove", self.key_remove),
        ];
        let mut out: Vec<_> = optional
            .into_iter()
            .filter_map(|(name, frame)| frame.map(|f| (name, f)))
            .collect();
        out.push(("model_label", self.model_label));
        out.push(("model_field", self.model_field));
        if let Some(frame) = self.use_button {
            out.push(("use", frame));
        }
        out.push(("test", self.test));
        if let Some(frame) = self.message {
            out.push(("message", frame));
        }
        out
    }
}

/// Frames `(x, y, w, h)` relative to the editor's top-left, only for the
/// controls this provider needs, and the editor's total height; nothing
/// reserves room for a hidden one.
pub fn editor_layout(
    inner: f64,
    custom: bool,
    needs_key: bool,
    show_use: bool,
    message_lines: usize,
) -> (EditorRows, f64) {
    let x = pad();
    let mut y = EDITOR_TOP;
    let (mut url_label, mut url_field) = (None, None);
    let (mut key_label, mut key_field, mut key_save, mut key_remove) = (None, None, None, None);
    if custom {
        url_label = Some((x, y, inner, f64::from(MODELS_LABEL_HEIGHT)));
        url_field = Some((x, y + 18.0, inner, field_h()));
        y += 18.0 + field_h() + row_gap();
    }
    if needs_key {
        key_label = Some((x, y, inner, f64::from(MODELS_LABEL_HEIGHT)));
        let field_w = inner - SAVE_W - REMOVE_W - 2.0 * gap();
        key_field = Some((x, y + 18.0, field_w, field_h()));
        key_save = Some((x + field_w + gap(), y + 18.0, SAVE_W, field_h()));
        key_remove = Some((
            x + field_w + 2.0 * gap() + SAVE_W,
            y + 18.0,
            REMOVE_W,
            field_h(),
        ));
        y += 18.0 + field_h() + row_gap();
    }
    let model_label = (x, y, inner, f64::from(MODELS_LABEL_HEIGHT));
    let model_field = (x, y + 18.0, inner, field_h());
    y += 18.0 + field_h() + row_gap();
    let mut action_x = x;
    let mut use_button = None;
    if show_use {
        use_button = Some((action_x, y, USE_W, ACTION_H));
        action_x += USE_W + gap();
    }
    let test = (action_x, y, TEST_W, ACTION_H);
    y += ACTION_H;
    let mut message = None;
    if message_lines > 0 {
        y += row_gap();
        let h = message_lines as f64 * MESSAGE_LINE;
        message = Some((x, y, inner, h));
        y += h;
    }
    let rows = EditorRows {
        url_label,
        url_field,
        key_label,
        key_field,
        key_save,
        key_remove,
        model_label,
        model_field,
        use_button,
        test,
        message,
    };
    (rows, y + EDITOR_BOTTOM)
}

/// `(use frame, test frame)` of the shared action row.
///
/// `rows` comes from `editor_layout(.., show_use = true)`: the Use slot always
/// exists there (`None` otherwise, where Python raises `KeyError`). When Use is
/// hidden, Test moves left into its place; the row height never changes, so
/// nothing is reserved or reflowed.
pub fn action_frames(rows: &EditorRows, show_use: bool) -> Option<(Frame, Frame)> {
    let use_frame = rows.use_button?;
    let test = rows.test;
    Some(if show_use {
        (use_frame, test)
    } else {
        (use_frame, (use_frame.0, test.1, test.2, test.3))
    })
}

/// Row tops inside one list card.
#[derive(Clone, Debug, PartialEq)]
pub struct ListLayout {
    pub rows: Vec<f64>,
    pub editor_y: Option<f64>,
    pub height: f64,
}

/// Row tops inside one list card, with the editor inserted under the
/// expanded row.
pub fn list_layout(
    _width: f64,
    count: usize,
    expanded_index: Option<usize>,
    editor_height: f64,
) -> ListLayout {
    let mut rows = Vec::new();
    let (mut y, mut editor_y) = (0.0, None);
    for index in 0..count {
        rows.push(y);
        y += f64::from(MODELS_ROW_HEIGHT);
        if Some(index) == expanded_index {
            editor_y = Some(y);
            y += editor_height;
        }
    }
    ListLayout {
        rows,
        editor_y,
        height: y,
    }
}

pub fn import_height(count: usize) -> f64 {
    pad() + IMPORT_HEADER_H + count as f64 * IMPORT_ROW_H + row_gap() + ACTION_H + pad()
}

// ---------------------------------------------------- change models directly ---
//
// The "Change models" card picks the cleanup & Ask model of the *active*
// provider and the speech provider/model in place. Saving only writes the
// config: the running app and server reload their clients from it.

pub const MAX_MODEL_ID_CHARS: usize = 200;
pub const OLLAMA_LIST_TIMEOUT: i64 = 5;

/// Speech providers LocalFlow ships, in menu order.
pub const SPEECH_PROVIDER_IDS: [&str; 4] = ["auto", "mlx-whisper", "faster-whisper", "parakeet"];

/// `model_storage._FW_CATALOG` / `_MLX_CATALOG` (menu order).
pub const FW_CATALOG: [&str; 7] = [
    "tiny",
    "base",
    "small",
    "medium",
    "large-v3-turbo",
    "large-v3",
    "distil-large-v3",
];
pub const MLX_CATALOG: [&str; 6] = [
    "tiny",
    "base",
    "small",
    "medium",
    "large-v3",
    "large-v3-turbo",
];
/// `model_storage._SIZE_LABELS`.
pub const SIZE_LABELS: [(&str, &str); 7] = [
    ("tiny", "Tiny"),
    ("base", "Base"),
    ("small", "Small"),
    ("medium", "Medium"),
    ("large-v3", "Large v3"),
    ("large-v3-turbo", "Large v3 Turbo"),
    ("distil-large-v3", "Distil Large v3"),
];

/// `live_config.STT_FIELDS`.
pub const STT_FIELDS: [&str; 4] = ["stt_backend", "model_size", "language", "stt_device"];

/// A refused model choice; the message is user-facing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoiceError(pub String);

impl fmt::Display for ModelChoiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModelChoiceError {}

fn refuse(message: &str) -> anyhow::Error {
    anyhow::Error::new(ModelChoiceError(message.to_string()))
}

fn is_model_id(text: &str) -> bool {
    let mut chars = text.chars();
    let ok_first = matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric());
    ok_first
        && chars.all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '@' | '+' | '-')
        })
}

/// A cloud or Ollama model id as typed/picked: one plain token. Errors are
/// [`ModelChoiceError`]s.
pub fn validate_model_id(text: &str) -> Result<String, ModelChoiceError> {
    let model = text.py_strip();
    if model.is_empty() {
        return Err(ModelChoiceError("Enter a model ID.".into()));
    }
    if model.chars().count() > MAX_MODEL_ID_CHARS || !is_model_id(model) {
        return Err(ModelChoiceError(
            "Use the provider's model ID: letters, numbers and . _ - : / @ + only.".into(),
        ));
    }
    Ok(model.to_string())
}

fn norm(url: &str) -> &str {
    url.py_strip().trim_end_matches('/')
}

/// Change only the active provider's model and save; returns the config.
///
/// Provider, endpoint and the endpoint-bound key reference are left exactly as
/// they are, so a stored key never has to be entered again and no key is
/// re-paired with another endpoint. `expected` is the `(provider, base_url)`
/// the control was drawn for: if the saved config moved to another provider
/// since, nothing is written. Refusals are [`ModelChoiceError`]s inside the
/// returned `anyhow::Error` (use `downcast_ref`); `save` errors pass through.
pub fn set_active_model(
    model: &str,
    config: &Config,
    expected: Option<(&str, &str)>,
    save: &dyn Fn(&Config) -> anyhow::Result<()>,
) -> anyhow::Result<Config> {
    let chosen = validate_model_id(model)?;
    if let Some((provider, base_url)) = expected {
        if (
            cfg_str(config, "llm_provider"),
            norm(cfg_str(config, "llm_base_url")),
        ) != (provider, norm(base_url))
        {
            return Err(refuse(
                "Your cleanup provider changed meanwhile; pick the model again.",
            ));
        }
    }
    if chosen == cfg_str(config, "llm_model") {
        return Ok(config.clone());
    }
    let updated = with_fields(config, &[("llm_model", json!(chosen))]);
    save(&updated)?;
    Ok(updated)
}

/// The configured speech provider id (`mlx` is the same as `mlx-whisper`).
pub fn current_speech_provider(config: &Config) -> String {
    let current = current_provider_text(config);
    if current == "mlx" {
        "mlx-whisper".to_string()
    } else {
        current
    }
}

/// `[(id, label)]` the speech popup offers. The configured provider is listed
/// even when unavailable here, so the popup never shows a lie. `available`
/// says whether an engine can run on this machine (the real check is
/// platform-specific and supplied by the caller; `auto` always can).
pub fn speech_provider_choices(
    config: &Config,
    available: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
    let current = current_speech_provider(config);
    let mut choices: Vec<(String, String)> = SPEECH_PROVIDER_IDS
        .iter()
        .filter(|pid| **pid == "auto" || **pid == current || available(pid))
        .map(|pid| (pid.to_string(), stt_label(pid).unwrap_or(pid).to_string()))
        .collect();
    if !choices.iter().any(|(pid, _)| *pid == current) {
        let label = stt_label(&current.to_lowercase())
            .unwrap_or(&current)
            .to_string();
        choices.push((current.clone(), format!("{label} (not offered)")));
    }
    choices
}

/// `[(size, label)]` for a speech provider; empty for Parakeet (fixed model).
pub fn speech_model_choices(provider: &str) -> Vec<(&'static str, &'static str)> {
    if stt_fixed_model(provider).is_some() {
        return Vec::new();
    }
    let sizes: &[&str] = if matches!(provider, "mlx-whisper" | "mlx") {
        &MLX_CATALOG
    } else {
        &FW_CATALOG
    };
    sizes
        .iter()
        .map(|size| {
            let label = SIZE_LABELS
                .iter()
                .find(|(s, _)| s == size)
                .map_or(*size, |(_, l)| *l);
            (*size, label)
        })
        .collect()
}

/// Save the speech provider and model together; returns the config.
///
/// Only providers this machine can run (plus the configured one) and only
/// their catalog sizes are accepted (refusals are [`ModelChoiceError`]s).
/// Parakeet runs one fixed model, so its `model` is ignored and the saved
/// size is left alone. Providers that only run on the default device get
/// `stt_device = "auto"` so the pair can load. Nothing is loaded here.
/// Picking the speech selection that is already saved (`STT_FIELDS`
/// unchanged) also calls `retry(&updated)` once the save succeeded
/// (see [`request_retry`]); a changed choice makes no request.
pub fn set_speech(
    provider: &str,
    model: Option<&str>,
    config: &Config,
    available: &dyn Fn(&str) -> bool,
    save: &dyn Fn(&Config) -> anyhow::Result<()>,
    retry: &dyn Fn(&Config),
) -> anyhow::Result<Config> {
    let offered = speech_provider_choices(config, available);
    if !offered.iter().any(|(pid, _)| pid == provider) || !SPEECH_PROVIDER_IDS.contains(&provider) {
        return Err(refuse(
            "That speech engine isn't available on this computer.",
        ));
    }
    let saved_size = cfg_str(config, "model_size");
    let size = if stt_fixed_model(provider).is_some() {
        saved_size.to_string()
    } else {
        let sizes: Vec<&str> = speech_model_choices(provider)
            .iter()
            .map(|(s, _)| *s)
            .collect();
        match model {
            Some(model) if sizes.contains(&model) => model.to_string(),
            _ if sizes.contains(&saved_size) => saved_size.to_string(),
            _ => "base".to_string(),
        }
    };
    let device = if matches!(provider, "mlx-whisper" | "parakeet") {
        "auto"
    } else {
        cfg_str(config, "stt_device")
    };
    let updated = with_fields(
        config,
        &[
            ("stt_backend", json!(provider)),
            ("model_size", json!(size)),
            ("stt_device", json!(device)),
        ],
    );
    save(&updated)?;
    if STT_FIELDS
        .iter()
        .all(|name| updated.field(name) == config.field(name))
    {
        retry(&updated);
    }
    Ok(updated)
}

/// The retry request file for a config file (`speech_retry.request_path`):
/// `<config name>.speech-retry.json` next to it.
pub fn retry_request_path(config_file: Option<&Path>) -> PathBuf {
    let target = match config_file {
        Some(path) => path.to_path_buf(),
        None => lf_core::config::config_path(),
    };
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!("{name}.speech-retry.json"))
}

/// Ask the running app to load `config`'s speech model again (if it failed)
/// (`speech_retry.request_retry`). Metadata only: a fresh random id and the
/// speech selection; never credentials. Written atomically (temporary file +
/// rename). This writes next to the config file, so tests pass a temporary
/// `config_file`.
pub fn request_retry(config: &Config, config_file: Option<&Path>) -> std::io::Result<PathBuf> {
    use std::io::Write;

    let target = retry_request_path(config_file);
    let mut selection = serde_json::Map::new();
    for name in STT_FIELDS {
        selection.insert(
            name.to_string(),
            config.field(name).cloned().unwrap_or(Value::Null),
        );
    }
    let data = json!({
        "version": 1,
        "id": uuid::Uuid::new_v4().simple().to_string(),
        "selection": Value::Object(selection),
    });
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = target.with_file_name(format!(
        ".{file_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let written = (|| {
        let mut handle = std::fs::File::create(&temporary)?;
        handle.write_all(data.to_string().as_bytes())?;
        handle.sync_all()?;
        std::fs::rename(&temporary, &target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.map(|()| target)
}

/// What the cleanup picker asks of an Ollama client: the installed models.
/// Implemented for every [`LlmProvider`]; tests inject a fake.
pub trait OllamaClient {
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
}

impl<T: LlmProvider> OllamaClient for T {
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        LlmProvider::list_models(self)
    }
}

/// State of the Ollama listing shown by the cleanup picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListState {
    /// A request is in flight (set by the caller; never returned here).
    Loading,
    /// Some models are installed.
    Ready,
    /// Ollama answered, nothing installed.
    Empty,
    /// No answer.
    Unavailable,
}

impl ListState {
    pub fn as_str(self) -> &'static str {
        match self {
            ListState::Loading => "loading",
            ListState::Ready => "ready",
            ListState::Empty => "empty",
            ListState::Unavailable => "unavailable",
        }
    }
}

/// `(installed model names, state)` for the cleanup picker; never fails.
///
/// With `client = None` a real [`OllamaLlm`] for `base_url` is used, which is
/// a blocking network call: run it off the main thread. Tests (and anything
/// offline) inject a client.
pub fn ollama_models(
    base_url: &str,
    client: Option<&dyn OllamaClient>,
) -> (Vec<String>, ListState) {
    let listing = match client {
        Some(client) => client.list_models(),
        None => {
            let real = OllamaLlm::new(base_url, "", OLLAMA_LIST_TIMEOUT, DEFAULT_NUM_CTX);
            LlmProvider::list_models(&real)
        }
    };
    let Ok(infos) = listing else {
        return (Vec::new(), ListState::Unavailable);
    };
    let names: Vec<String> = infos
        .into_iter()
        .map(|info| info.id)
        .filter(|id| is_model_id(id) && id.chars().count() <= MAX_MODEL_ID_CHARS)
        .collect();
    let state = if names.is_empty() {
        ListState::Empty
    } else {
        ListState::Ready
    };
    (names, state)
}

/// What the cleanup model control shows: `(items, selected, note)`.
///
/// Ollama lists only installed models; a configured one that is not installed
/// is still shown (marked) with what to do, because Ollama answers 404 for a
/// model it does not have.
pub fn cleanup_picker(
    config: &Config,
    state: ListState,
    models: &[String],
) -> (Vec<String>, String, String) {
    let configured = cfg_str(config, "llm_model").py_strip().to_string();
    let only_configured = || {
        if configured.is_empty() {
            Vec::new()
        } else {
            vec![configured.clone()]
        }
    };
    match state {
        ListState::Loading => (
            only_configured(),
            configured.clone(),
            "Loading installed models…".into(),
        ),
        ListState::Unavailable => (
            only_configured(),
            configured.clone(),
            "Ollama isn't answering. Start Ollama, then Refresh.".into(),
        ),
        ListState::Empty => (
            only_configured(),
            configured.clone(),
            "No models installed. Run `ollama pull gemma4`, then Refresh.".into(),
        ),
        ListState::Ready => {
            let mut items = models.to_vec();
            let mut note = String::new();
            if !configured.is_empty() && !items.contains(&configured) {
                items.insert(0, configured.clone());
                note = format!(
                    "{configured} isn't installed in Ollama (it would fail). Pick an \
                     installed model, or run `ollama pull {configured}` and Refresh."
                );
            }
            (items, configured, note)
        }
    }
}

pub const QUICK_LABEL_W: f64 = 150.0;
pub const QUICK_CONTROL_H: f64 = 26.0;
pub const QUICK_ROW_H: f64 = 34.0;
pub const REFRESH_W: f64 = 96.0;
pub const IMPORT_W: f64 = 120.0;
pub const STATUS_H: f64 = 16.0;
/// The row label; the provider has its own status line.
pub const CLEANUP_LABEL: &str = "Cleanup & Ask";

/// The active cleanup provider, shown under the model control.
pub fn provider_status_text(config: &Config) -> String {
    let name = match active_preset(config) {
        Some(preset) => preset.label,
        None => cfg_str(config, "llm_provider"),
    };
    format!("Active provider: {name}")
}

/// Most lines any of `texts` wraps to at `width` (0 when all are empty).
pub fn note_lines<S: AsRef<str>>(texts: &[S], width: f64, measure: &dyn Fn(&str) -> f64) -> usize {
    texts
        .iter()
        .map(AsRef::as_ref)
        .filter(|text| !text.is_empty())
        .map(|text| wrap_lines(text, width.max(40.0), measure).len())
        .max()
        .unwrap_or(0)
}

/// Frames of the one Models card; nothing overlaps.
#[derive(Clone, Debug, PartialEq)]
pub struct QuickLayout {
    pub title: Frame,
    pub import: Frame,
    pub detail: Frame,
    pub speech_label: Frame,
    pub speech_provider: Frame,
    /// Present when `fixed_speech`.
    pub speech_fixed: Option<Frame>,
    /// Present when not `fixed_speech`.
    pub speech_model: Option<Frame>,
    pub cleanup_label: Frame,
    pub cleanup_model: Frame,
    /// Present for an Ollama cleanup provider.
    pub refresh: Option<Frame>,
    pub provider_status: Frame,
    pub message: Option<Frame>,
    pub height: f64,
}

impl QuickLayout {
    /// Every frame (without `height`) by its Python dict key.
    pub fn frames(&self) -> Vec<(&'static str, Frame)> {
        let mut out = vec![
            ("title", self.title),
            ("import", self.import),
            ("detail", self.detail),
            ("speech_label", self.speech_label),
            ("speech_provider", self.speech_provider),
        ];
        if let Some(f) = self.speech_fixed {
            out.push(("speech_fixed", f));
        }
        if let Some(f) = self.speech_model {
            out.push(("speech_model", f));
        }
        out.push(("cleanup_label", self.cleanup_label));
        out.push(("cleanup_model", self.cleanup_model));
        if let Some(f) = self.refresh {
            out.push(("refresh", f));
        }
        out.push(("provider_status", self.provider_status));
        if let Some(f) = self.message {
            out.push(("message", f));
        }
        out
    }
}

/// Frames `(x, y, w, h)` of the one Models card; nothing overlaps.
///
/// Title row: "Change models" and Import keys…. Row 1: speech engine popup and
/// model popup (or the fixed-model label). Row 2: "Cleanup & Ask": an Ollama
/// popup plus Refresh, or a model-ID field for a cloud provider, with the
/// active provider on a status line under it. Then a wrapped note sized by
/// the caller (`message_lines`); none is reserved when there is none.
pub fn quick_layout(
    width: f64,
    ollama: bool,
    fixed_speech: bool,
    message_lines: usize,
) -> QuickLayout {
    let inner = width - 2.0 * pad();
    let x = pad();
    let controls_x = x + QUICK_LABEL_W + gap();
    let controls_w = inner - QUICK_LABEL_W - gap();
    let half = ((controls_w - gap()) / 2.0).floor();
    let mut y = pad();
    let title = (
        x,
        y + ((ACTION_H - 20.0) / 2.0).floor(),
        inner - IMPORT_W - gap(),
        20.0,
    );
    let import = (x + inner - IMPORT_W, y, IMPORT_W, ACTION_H);
    y += ACTION_H + 2.0;
    let detail = (x, y, inner, 16.0);
    y += 16.0 + row_gap();
    let speech_label = (x, y + 5.0, QUICK_LABEL_W, 16.0);
    let speech_provider = (controls_x, y, half, QUICK_CONTROL_H);
    let second = (
        controls_x + half + gap(),
        y + if fixed_speech { 5.0 } else { 0.0 },
        controls_w - half - gap(),
        if fixed_speech { 16.0 } else { QUICK_CONTROL_H },
    );
    y += QUICK_ROW_H;
    let cleanup_label = (x, y + 5.0, QUICK_LABEL_W, 16.0);
    let (cleanup_model, refresh) = if ollama {
        (
            (
                controls_x,
                y,
                controls_w - REFRESH_W - gap(),
                QUICK_CONTROL_H,
            ),
            Some((x + inner - REFRESH_W, y, REFRESH_W, QUICK_CONTROL_H)),
        )
    } else {
        ((controls_x, y, controls_w, QUICK_CONTROL_H), None)
    };
    y += QUICK_CONTROL_H + 4.0;
    let provider_status = (controls_x, y, controls_w, STATUS_H);
    y += STATUS_H;
    let mut message = None;
    if message_lines > 0 {
        y += row_gap();
        let h = message_lines as f64 * MESSAGE_LINE;
        message = Some((x, y, inner, h));
        y += h;
    }
    QuickLayout {
        title,
        import,
        detail,
        speech_label,
        speech_provider,
        speech_fixed: fixed_speech.then_some(second),
        speech_model: (!fixed_speech).then_some(second),
        cleanup_label,
        cleanup_model,
        refresh,
        provider_status,
        message,
        height: y + pad(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::*;
    use lf_providers::keys::get_preset;
    use lf_providers::{ConfigStore, SecretStore};
    use std::cell::{Cell, RefCell};

    const BODY_MIN: f64 = (HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER) as f64;
    const VIEWPORT: f64 = (HUB_MIN_HEIGHT - 24) as f64;

    fn preset(id: &str) -> &'static Preset {
        get_preset(id).unwrap()
    }

    fn config_with(changes: &[(&str, Value)]) -> Config {
        with_fields(&Config::defaults_for(false), changes)
    }

    fn cloud(p: &Preset, model: &str, url: Option<&str>) -> Config {
        config_with(&[
            ("llm_provider", json!("openai-compatible")),
            ("llm_base_url", json!(url.unwrap_or(p.base_url))),
            ("llm_model", json!(model)),
        ])
    }

    fn active_ids(config: &Config) -> Vec<&'static str> {
        PRESETS
            .iter()
            .filter(|p| is_active(p, config, None))
            .map(|p| p.id)
            .collect()
    }

    // In-memory secrets/config: no keyring, no config file.
    #[derive(Default)]
    struct Vault {
        secrets: RefCell<BTreeMap<String, String>>,
        saved: RefCell<Vec<Config>>,
        forbid_lookup: Cell<bool>,
    }

    impl SecretStore for Vault {
        fn get(&self, name: &str) -> String {
            assert!(
                !self.forbid_lookup.get(),
                "secret lookup for a non-active provider"
            );
            self.secrets.borrow().get(name).cloned().unwrap_or_default()
        }
        fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
            self.secrets.borrow_mut().insert(name.into(), value.into());
            Ok(())
        }
        fn delete(&self, name: &str) -> anyhow::Result<()> {
            self.secrets.borrow_mut().remove(name);
            Ok(())
        }
    }

    impl ConfigStore<Config> for Vault {
        fn load(&self) -> anyhow::Result<Config> {
            Ok(Config::defaults_for(false))
        }
        fn save(&self, config: &Config) -> anyhow::Result<()> {
            self.saved.borrow_mut().push(config.clone());
            Ok(())
        }
    }

    // ---- active rules ----

    #[test]
    fn default_config_is_the_local_ollama_provider() {
        let config = Config::defaults_for(false);
        assert_eq!(active_ids(&config), ["ollama"]);
        assert_eq!(active_preset(&config).unwrap().id, "ollama");
    }

    #[test]
    fn active_needs_both_provider_and_endpoint_to_match() {
        let fireworks = preset("fireworks");
        assert_eq!(active_ids(&cloud(fireworks, "m", None)), ["fireworks"]);
        let other = cloud(fireworks, "m", Some("https://llm.internal.example/v1"));
        assert_eq!(active_ids(&other), ["custom"]);
        let remote = config_with(&[("llm_base_url", json!("http://other-host:11434"))]);
        assert!(!is_active(preset("ollama"), &remote, None));
        let slash = format!("{}/", fireworks.base_url);
        assert!(is_active(
            fireworks,
            &cloud(fireworks, "m", Some(&slash)),
            None
        ));
        // A blank endpoint is not the custom one.
        assert!(!is_active(
            preset("custom"),
            &cloud(preset("custom"), "m", Some("  ")),
            None
        ));
    }

    #[test]
    fn custom_is_judged_against_the_typed_endpoint_in_the_editor() {
        let custom = preset("custom");
        let url = "https://llm.internal.example/v1";
        let config = cloud(custom, "m", Some(url));
        assert!(is_active(custom, &config, Some(url)));
        assert!(!is_active(
            custom,
            &config,
            Some("https://other.example/v1")
        ));
        assert!(!is_active(custom, &config, Some("")));
    }

    #[test]
    fn status_pill() {
        let (fw, oa, ol) = (preset("fireworks"), preset("openai"), preset("ollama"));
        let cases = [
            (
                fw,
                cloud(fw, "m", None),
                true,
                "Active",
                ("cobalt", "cobalt"),
            ),
            (
                fw,
                cloud(oa, "m", None),
                true,
                "Key saved",
                ("success", "success"),
            ),
            (
                fw,
                cloud(oa, "m", None),
                false,
                "No key",
                ("muted", "muted"),
            ),
            (ol, cloud(oa, "m", None), false, "Local", ("line", "ink")),
            (
                ol,
                Config::defaults_for(false),
                false,
                "Active",
                ("cobalt", "cobalt"),
            ),
            (
                fw,
                cloud(fw, "m", None),
                false,
                "Active",
                ("cobalt", "cobalt"),
            ),
        ];
        for (p, config, has_key, label, kind) in cases {
            assert_eq!(
                provider_status(p, &config, has_key),
                (label, kind),
                "{}",
                p.id
            );
        }
    }

    #[test]
    fn use_button_is_hidden_only_when_already_active_and_unchanged() {
        let (fw, oa, cu) = (preset("fireworks"), preset("openai"), preset("custom"));
        let config = cloud(fw, "model-a", None);
        assert!(!show_use_button(fw, &config, fw.base_url, "model-a", false));
        assert!(show_use_button(fw, &config, fw.base_url, "model-b", false));
        assert!(show_use_button(oa, &config, oa.base_url, "model-a", false));
        let url = "https://llm.internal.example/v1";
        let custom = cloud(cu, "m", Some(url));
        assert!(!show_use_button(cu, &custom, url, "m", false));
        assert!(show_use_button(
            cu,
            &custom,
            "https://typed.example/v1",
            "m",
            false
        ));
        assert!(show_use_button(fw, &config, fw.base_url, "model-a", true));
        assert!(!show_use_button(
            fw,
            &config,
            fw.base_url,
            " model-a ",
            false
        ));
    }

    #[test]
    fn summary_values_name_the_speech_and_cleanup_models() {
        let config = with_fields(
            &cloud(preset("fireworks"), "", None),
            &[
                ("stt_backend", json!("mlx-whisper")),
                ("model_size", json!("small")),
            ],
        );
        let (speech, cleanup) = summary_values(&config);
        assert_eq!(speech, "MLX Whisper / small");
        assert_eq!(cleanup, "Fireworks / not selected");
    }

    #[test]
    fn speech_summary_uses_the_provider_field_not_the_device() {
        let parakeet = config_with(&[
            ("stt_backend", json!("parakeet")),
            ("model_size", json!("large-v3")),
            ("stt_device", json!("cpu")),
        ]);
        let text = speech_summary(&parakeet);
        assert_eq!(text, "Parakeet / TDT 0.6B v2");
        assert!(!text.contains("cpu") && !text.contains("large-v3"));
        for (backend, label) in [
            ("faster-whisper", "Faster Whisper / base"),
            ("mlx", "MLX Whisper / base"),
            ("mlx-whisper", "MLX Whisper / base"),
            ("my-plugin", "my-plugin / base"),
            ("Parakeet", "Parakeet / TDT 0.6B v2"),
        ] {
            let config = config_with(&[
                ("stt_backend", json!(backend)),
                ("model_size", json!("base")),
                ("stt_device", json!("mps")),
            ]);
            let text = speech_summary(&config);
            assert_eq!(text, label);
            assert!(!text.contains("mps"));
        }
        assert_eq!(
            speech_summary(&Config::defaults_for(false)),
            "Automatic / base"
        );
        assert_eq!(
            speech_summary(&config_with(&[("stt_device", json!("cuda"))])),
            "Automatic / base"
        );
        // Empty provider and size.
        let blank = config_with(&[("stt_backend", json!("")), ("model_size", json!(""))]);
        assert_eq!(speech_summary(&blank), "Automatic");
        assert_eq!(
            summary_values(&Config::defaults_for(false)).0,
            "Automatic / base"
        );
    }

    // ---- import ----

    fn candidates() -> Vec<ImportCandidate> {
        let mut environ = BTreeMap::new();
        environ.insert(
            "FIREWORKS_API_KEY".to_string(),
            "secret-value-1234".to_string(),
        );
        environ.insert(
            "OPENAI_API_KEY".to_string(),
            "secret-value-5678".to_string(),
        );
        lf_providers::keys::detect_imports(None, &environ).unwrap()
    }

    fn fw_key() -> ImportKey {
        (
            "fireworks".to_string(),
            "environment: FIREWORKS_API_KEY".to_string(),
        )
    }

    fn oa_key() -> ImportKey {
        (
            "openai".to_string(),
            "environment: OPENAI_API_KEY".to_string(),
        )
    }

    #[test]
    fn import_ticks_default_on_only_without_a_saved_key() {
        let found = candidates();
        assert_eq!(found.len(), 2);
        let states = import_states(&found, &|pid| pid == "openai", &BTreeMap::new());
        assert!(states[&fw_key()]);
        assert!(!states[&oa_key()]);
        assert!(import_states(&[], &|_| false, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn user_changed_ticks_survive_rebuilds() {
        let found = candidates();
        let mut overrides = BTreeMap::new();
        overrides.insert(fw_key(), false);
        overrides.insert(oa_key(), true);
        let states = import_states(&found, &|pid| pid == "openai", &overrides);
        assert!(!states[&fw_key()]);
        assert!(states[&oa_key()]);
    }

    // ---- geometry ----

    fn overlap(a: Frame, b: Frame) -> bool {
        a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
    }

    #[test]
    fn models_card_import_button_clears_the_title() {
        for width in [BODY_MIN, 800.0, 1016.0] {
            let geo = quick_layout(width, true, false, 0);
            let (title, button) = (geo.title, geo.import);
            assert!(!overlap(title, button) && title.0 + title.2 <= button.0);
            assert_eq!(button.0 + button.2, width - pad());
            assert!(button.1 + button.3 <= geo.detail.1);
        }
    }

    #[test]
    fn row_columns_never_overlap() {
        for pill_w in [70.0, 90.0, 120.0] {
            let cols = row_columns(BODY_MIN, pill_w);
            assert!(cols.text.0 + cols.text.1 < cols.pill);
            assert!(cols.pill + pill_w < cols.button.0);
            assert!(cols.button.0 + cols.button.1 <= BODY_MIN - 16.0);
            assert!(cols.text.1 > 200.0);
        }
        assert_eq!(row_columns(10.0, 500.0).text.1, 0.0);
    }

    #[test]
    fn collapsed_list_is_compact_and_the_page_fits_the_minimum_window() {
        let geo = list_layout(BODY_MIN, PRESETS.len(), None, 0.0);
        assert_eq!(
            geo.height,
            PRESETS.len() as f64 * f64::from(MODELS_ROW_HEIGHT)
        );
        assert_eq!(geo.editor_y, None);
        let card = quick_layout(BODY_MIN, true, false, 0).height;
        let top = f64::from(HUB_HEADER_OFFSET) + card + 12.0;
        assert!(top + 5.0 * f64::from(MODELS_ROW_HEIGHT) <= VIEWPORT - f64::from(HUB_PAGE_BOTTOM));
        let loading = quick_layout(BODY_MIN, true, false, 1).height;
        assert!(card < loading && loading < 108.0 + 12.0 + 178.0);
        assert!(list_layout(BODY_MIN, 0, None, 0.0).rows.is_empty());
    }

    #[test]
    fn only_the_expanded_row_adds_height() {
        let (rows, editor_height) = editor_layout(BODY_MIN - 32.0, false, true, true, 0);
        let collapsed = list_layout(BODY_MIN, 7, None, 0.0);
        let expanded = list_layout(BODY_MIN, 7, Some(2), editor_height);
        assert_eq!(expanded.height, collapsed.height + editor_height);
        assert_eq!(expanded.rows[..3], collapsed.rows[..3]);
        assert_eq!(
            expanded.editor_y,
            Some(expanded.rows[2] + f64::from(MODELS_ROW_HEIGHT))
        );
        assert_eq!(expanded.rows[3], expanded.editor_y.unwrap() + editor_height);
        assert!(!rows.frames().is_empty());
    }

    #[test]
    fn editor_has_only_needed_controls_and_never_overlaps() {
        let inner = BODY_MIN - 32.0;
        for (custom, needs_key, show_use, lines) in [
            (false, true, true, 0),
            (false, true, false, 0),
            (false, false, true, 0),
            (true, true, true, 1),
            (true, true, false, 2),
            (false, false, false, 0),
        ] {
            let (rows, height) = editor_layout(inner, custom, needs_key, show_use, lines);
            assert_eq!(rows.url_field.is_some(), custom);
            assert_eq!(rows.key_field.is_some(), needs_key);
            assert_eq!(rows.use_button.is_some(), show_use);
            assert_eq!(rows.message.is_some(), lines > 0);
            let frames = rows.frames();
            for (i, (_, first)) in frames.iter().enumerate() {
                assert!(first.0 >= 16.0 && first.0 + first.2 <= 16.0 + inner);
                assert!(first.1 + first.3 <= height);
                for (_, second) in &frames[i + 1..] {
                    assert!(!overlap(*first, *second));
                }
            }
            if !show_use {
                assert_eq!(rows.test.0, 16.0);
            }
        }
    }

    #[test]
    fn hiding_unused_rows_makes_the_editor_shorter() {
        let inner = BODY_MIN - 32.0;
        let full = editor_layout(inner, true, true, true, 2).1;
        let local = editor_layout(inner, false, false, false, 0).1;
        assert!(local < full && local < 150.0);
    }

    #[test]
    fn import_card_height_grows_with_candidates_only() {
        assert_eq!(import_height(2) - import_height(1), IMPORT_ROW_H);
    }

    #[test]
    fn single_primary_is_hidden_not_reserved_space() {
        let (rows, _) = editor_layout(BODY_MIN - 32.0, false, true, true, 0);
        let (use_f, test) = action_frames(&rows, true).unwrap();
        let (hidden_use, hidden_test) = action_frames(&rows, false).unwrap();
        assert_eq!(hidden_use, use_f);
        assert_eq!(test, rows.test);
        assert!(hidden_test.0 == use_f.0 && use_f.0 < test.0);
        assert_eq!((hidden_test.1, hidden_test.3), (test.1, test.3));
        assert_eq!(hidden_test.2, test.2);
        let (no_use, _) = editor_layout(BODY_MIN - 32.0, false, true, false, 0);
        assert_eq!(action_frames(&no_use, false), None);
    }

    // ---- key-aware Use button ----

    #[test]
    fn replacing_the_key_of_the_active_provider_offers_use_until_activated() {
        let vault = Vault::default();
        let keys = ProviderKeys::new(&vault, &vault);
        let fireworks = preset("fireworks");
        let mut config = cloud(fireworks, "m", None);
        let visible = |config: &Config| {
            use_button_visible(fireworks, config, fireworks.base_url, "m", &keys).unwrap()
        };
        keys.save_key("fireworks", "fake-key-A").unwrap();
        config = keys
            .activate("fireworks", None, Some("m"), Some(config))
            .unwrap();
        assert!(!visible(&config)); // active, same model, same key
        keys.save_key("fireworks", "fake-key-B").unwrap(); // a replacement
        assert!(visible(&config));
        assert!(keys
            .key_needs_activation("fireworks", Some(&config))
            .unwrap());
        assert!(vault.secrets.borrow().get("llm_api_key").is_none());
        config = keys
            .activate("fireworks", None, Some("m"), Some(config))
            .unwrap();
        assert!(!visible(&config));
        let found = {
            let mut environ = BTreeMap::new();
            environ.insert("FIREWORKS_API_KEY".to_string(), "fake-key-C".to_string());
            lf_providers::keys::detect_imports(None, &environ).unwrap()
        };
        keys.save_imports(&found).unwrap();
        assert!(visible(&config)); // an imported replacement is activateable too
        config = keys
            .activate("fireworks", None, Some("m"), Some(config))
            .unwrap();
        assert!(!visible(&config));
        assert_eq!(vault.saved.borrow().len(), 3);
    }

    #[test]
    fn missing_or_irrelevant_keys_never_offer_reactivation() {
        let vault = Vault::default();
        let keys = ProviderKeys::new(&vault, &vault);
        let fireworks = preset("fireworks");
        let config = cloud(fireworks, "m", None);
        assert!(!keys
            .key_needs_activation("fireworks", Some(&config))
            .unwrap()); // nothing saved
        keys.save_key("fireworks", "fake-key-A").unwrap();
        let config = keys
            .activate("fireworks", None, Some("m"), Some(config))
            .unwrap();
        keys.remove_key("fireworks", None).unwrap();
        assert!(!keys
            .key_needs_activation("fireworks", Some(&config))
            .unwrap());
        assert!(vault.secrets.borrow().is_empty()); // snapshot and index went too
        assert!(!use_button_visible(fireworks, &config, fireworks.base_url, "m", &keys).unwrap());
        assert!(!keys.key_needs_activation("ollama", None).unwrap()); // keyless provider
    }

    #[test]
    fn secrets_are_only_compared_for_an_otherwise_active_provider() {
        let vault = Vault::default();
        vault.forbid_lookup.set(true);
        let keys = ProviderKeys::new(&vault, &vault);
        let config = cloud(preset("fireworks"), "m", None);
        let (oa, fw) = (preset("openai"), preset("fireworks"));
        assert!(use_button_visible(oa, &config, oa.base_url, "m", &keys).unwrap());
        assert!(use_button_visible(fw, &config, fw.base_url, "other", &keys).unwrap());
        // A blank model is never offered, without any lookup either.
        assert!(!use_button_visible(oa, &config, oa.base_url, "  ", &keys).unwrap());
        assert!(show_use_button(fw, &config, fw.base_url, "m", true));
    }

    // ---- change models ----

    #[test]
    fn model_ids_accept_real_provider_ids() {
        for model in [
            "gemma4:e4b",
            "llama3.2:3b",
            "accounts/fireworks/models/kimi-k2p6",
            "gpt-4o-mini",
            "meta-llama/Llama-3.3-70B-Instruct-Turbo",
            "qwen/qwen3-32b@latest",
            "  padded  ",
        ] {
            assert_eq!(validate_model_id(model).unwrap(), model.py_strip());
        }
    }

    /// Python `str.strip()` also removes U+001C..U+001F (Rust `trim` does not).
    #[test]
    fn model_ids_strip_like_python_including_information_separators() {
        for sep in ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}'] {
            let typed = format!("{sep}gpt-4o{sep}");
            assert_eq!(validate_model_id(&typed).unwrap(), "gpt-4o", "{sep:?}");
        }
        assert_eq!(validate_model_id("\u{a0}gpt\u{2003}").unwrap(), "gpt");
        assert_eq!(
            validate_model_id("\u{1c}\u{1f}").unwrap_err().0,
            "Enter a model ID."
        );
        // Inside the id they are still junk.
        assert!(validate_model_id("gpt\u{1c}4").is_err());
    }

    #[test]
    fn model_ids_refuse_junk() {
        let long = "a".repeat(201);
        let bidi = format!("m{}", '\u{202E}');
        for model in [
            "", "   ", "gpt 4o", "x\ny", &long, "-flag", &bidi, "é", "日本",
        ] {
            assert!(validate_model_id(model).is_err(), "{model:?}");
        }
        assert_eq!(validate_model_id("").unwrap_err().0, "Enter a model ID.");
        assert!(validate_model_id(&"a".repeat(200)).is_ok());
    }

    fn fireworks_config() -> Config {
        config_with(&[
            ("llm_provider", json!("openai-compatible")),
            ("llm_base_url", json!(preset("fireworks").base_url)),
            ("llm_model", json!("accounts/fireworks/models/a")),
            (
                "llm_key_ref",
                json!(format!("llm_activation_fireworks_{}", "a".repeat(32))),
            ),
        ])
    }

    #[test]
    fn model_only_change_keeps_provider_endpoint_and_key_pairing() {
        let saved = RefCell::new(Vec::new());
        let save = |c: &Config| {
            saved.borrow_mut().push(c.clone());
            Ok(())
        };
        let config = fireworks_config();
        let base = format!("{}/", cfg_str(&config, "llm_base_url"));
        let updated = set_active_model(
            "accounts/fireworks/models/b",
            &config,
            Some((cfg_str(&config, "llm_provider"), &base)),
            &save,
        )
        .unwrap();
        assert_eq!(saved.borrow().len(), 1);
        assert_eq!(saved.borrow()[0], updated);
        assert_eq!(
            cfg_str(&updated, "llm_model"),
            "accounts/fireworks/models/b"
        );
        for name in ["llm_provider", "llm_base_url", "llm_key_ref"] {
            assert_eq!(updated.field(name), config.field(name));
        }
    }

    #[test]
    fn unchanged_invalid_or_moved_provider_writes_nothing() {
        let saved = RefCell::new(0usize);
        let save = |_: &Config| {
            *saved.borrow_mut() += 1;
            Ok(())
        };
        let config = fireworks_config();
        let same = set_active_model(cfg_str(&config, "llm_model"), &config, None, &save).unwrap();
        assert_eq!(same, config);
        let bad = set_active_model("bad id", &config, None, &save).unwrap_err();
        assert!(bad.downcast_ref::<ModelChoiceError>().is_some());
        let moved = set_active_model(
            "x",
            &config,
            Some(("ollama", "http://localhost:11434")),
            &save,
        )
        .unwrap_err();
        assert!(moved.to_string().contains("provider changed"));
        assert_eq!(*saved.borrow(), 0);
        // A failing save is reported, not hidden.
        let failing = |_: &Config| Err(anyhow::anyhow!("disk full"));
        let err = set_active_model("other-model", &config, None, &failing).unwrap_err();
        assert_eq!(err.to_string(), "disk full");
    }

    struct FakeOllama {
        names: Vec<&'static str>,
        error: Option<&'static str>,
    }

    impl OllamaClient for FakeOllama {
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            if let Some(error) = self.error {
                return Err(ProviderError(error.into()));
            }
            Ok(self
                .names
                .iter()
                .map(|n| ModelInfo {
                    id: n.to_string(),
                    label: n.to_string(),
                    installed: None,
                })
                .collect())
        }
    }

    #[test]
    fn ollama_listing_states() {
        let ok = FakeOllama {
            names: vec!["gemma4:e4b"],
            error: None,
        };
        assert_eq!(
            ollama_models("u", Some(&ok)),
            (vec!["gemma4:e4b".to_string()], ListState::Ready)
        );
        let empty = FakeOllama {
            names: vec![],
            error: None,
        };
        assert_eq!(ollama_models("u", Some(&empty)), (vec![], ListState::Empty));
        let down = FakeOllama {
            names: vec![],
            error: Some("refused"),
        };
        assert_eq!(
            ollama_models("u", Some(&down)),
            (vec![], ListState::Unavailable)
        );
        let junk = FakeOllama {
            names: vec!["ok:1", "bad\nname", "x y"],
            error: None,
        };
        assert_eq!(
            ollama_models("u", Some(&junk)),
            (vec!["ok:1".to_string()], ListState::Ready)
        );
        // Only junk -> nothing usable -> "empty".
        let only_junk = FakeOllama {
            names: vec!["x y"],
            error: None,
        };
        assert_eq!(ollama_models("u", Some(&only_junk)).1, ListState::Empty);
    }

    #[test]
    fn cleanup_picker_states_say_what_to_do() {
        let config = config_with(&[("llm_model", json!("llama3.2:3b"))]);
        let (items, selected, note) =
            cleanup_picker(&config, ListState::Ready, &["gemma4:e4b".into()]);
        assert_eq!(items, ["llama3.2:3b", "gemma4:e4b"]);
        assert_eq!(selected, "llama3.2:3b");
        assert!(note.contains("isn't installed") && note.contains("ollama pull llama3.2:3b"));
        assert_eq!(
            cleanup_picker(&config, ListState::Ready, &["llama3.2:3b".into()]).2,
            ""
        );
        assert!(cleanup_picker(&config, ListState::Unavailable, &[])
            .2
            .contains("Start Ollama"));
        assert!(cleanup_picker(&config, ListState::Empty, &[])
            .2
            .contains("ollama pull"));
        assert!(cleanup_picker(&config, ListState::Loading, &[])
            .2
            .contains("Loading"));
        let blank = config_with(&[("llm_model", json!(""))]);
        assert_eq!(
            cleanup_picker(&blank, ListState::Empty, &[]).0,
            Vec::<String>::new()
        );
        assert_eq!(ListState::Unavailable.as_str(), "unavailable");
    }

    fn avail<'a>(ids: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |p| ids.contains(&p)
    }

    #[test]
    fn speech_menu_offers_only_what_runs_here_plus_the_current_one() {
        let ids = |c: &[(String, String)]| c.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
        let choices =
            speech_provider_choices(&Config::defaults_for(false), &avail(&["faster-whisper"]));
        assert_eq!(ids(&choices), ["auto", "faster-whisper"]);
        let parakeet = config_with(&[("stt_backend", json!("parakeet"))]);
        assert!(
            ids(&speech_provider_choices(&parakeet, &avail(&[]))).contains(&"parakeet".to_string())
        );
        let mlx = config_with(&[("stt_backend", json!("mlx"))]);
        assert_eq!(current_speech_provider(&mlx), "mlx-whisper");
        // An unknown configured provider is listed, marked as not offered.
        let plugin = config_with(&[("stt_backend", json!("my-plugin"))]);
        let choices = speech_provider_choices(&plugin, &avail(&[]));
        assert_eq!(
            choices.last().unwrap(),
            &(
                "my-plugin".to_string(),
                "my-plugin (not offered)".to_string()
            )
        );
    }

    #[test]
    fn parakeet_is_a_fixed_model_and_keeps_the_saved_size() {
        assert!(speech_model_choices("parakeet").is_empty());
        let saved = RefCell::new(Vec::new());
        let save = |c: &Config| {
            saved.borrow_mut().push(c.clone());
            Ok(())
        };
        let config = config_with(&[("stt_device", json!("cpu")), ("model_size", json!("small"))]);
        let updated = set_speech(
            "parakeet",
            Some("large-v3"),
            &config,
            &avail(&["parakeet"]),
            &save,
            &|_| {},
        )
        .unwrap();
        assert_eq!(cfg_str(&updated, "stt_backend"), "parakeet");
        assert_eq!(cfg_str(&updated, "model_size"), "small");
        assert_eq!(cfg_str(&updated, "stt_device"), "auto");
        assert_eq!(saved.borrow().len(), 1);
        assert_eq!(saved.borrow()[0], updated);
    }

    #[test]
    fn whisper_sizes_and_refusals() {
        let saved = RefCell::new(0usize);
        let save = |_: &Config| {
            *saved.borrow_mut() += 1;
            Ok(())
        };
        let config = Config::defaults_for(false);
        let fw = avail(&["faster-whisper"]);
        let updated = set_speech(
            "faster-whisper",
            Some("small"),
            &config,
            &fw,
            &save,
            &|_| {},
        )
        .unwrap();
        assert_eq!(cfg_str(&updated, "stt_backend"), "faster-whisper");
        assert_eq!(cfg_str(&updated, "model_size"), "small");
        let sizes = |p: &str| {
            speech_model_choices(p)
                .iter()
                .map(|(s, _)| *s)
                .collect::<Vec<_>>()
        };
        assert!(sizes("faster-whisper").contains(&"distil-large-v3"));
        assert!(!sizes("mlx-whisper").contains(&"distil-large-v3"));
        assert_eq!(
            speech_model_choices("mlx")[5],
            ("large-v3-turbo", "Large v3 Turbo")
        );
        let odd = set_speech(
            "faster-whisper",
            Some("not-a-size"),
            &config,
            &fw,
            &save,
            &|_| {},
        )
        .unwrap();
        assert_eq!(cfg_str(&odd, "model_size"), "base"); // only catalog sizes are saved
        let none = set_speech("faster-whisper", None, &config, &fw, &save, &|_| {}).unwrap();
        assert_eq!(cfg_str(&none, "model_size"), "base");
        let refused =
            set_speech("parakeet", None, &config, &avail(&[]), &save, &|_| {}).unwrap_err();
        assert!(refused.downcast_ref::<ModelChoiceError>().is_some());
        let unknown = set_speech("whisper-cloud", None, &config, &|_| true, &save, &|_| {});
        assert!(unknown.is_err());
        assert_eq!(*saved.borrow(), 3);

        // An unchanged choice is still saved and asks explicitly for a retry.
        let retries = RefCell::new(Vec::new());
        let retry = |c: &Config| retries.borrow_mut().push(c.clone());
        let same = set_speech("auto", Some("base"), &config, &avail(&[]), &save, &retry).unwrap();
        assert_eq!(same, config);
        assert_eq!(*saved.borrow(), 4);
        assert_eq!(retries.borrow().len(), 1);
        assert_eq!(retries.borrow()[0], config);
        // A changed choice makes no retry request.
        set_speech("faster-whisper", Some("small"), &config, &fw, &save, &retry).unwrap();
        assert_eq!(retries.borrow().len(), 1);
    }

    #[test]
    fn a_failed_save_makes_no_retry_request() {
        let retries = Cell::new(0);
        let failing = |_: &Config| Err(anyhow::anyhow!("read-only"));
        let config = Config::defaults_for(false);
        let result = set_speech(
            "auto",
            Some("base"),
            &config,
            &avail(&[]),
            &failing,
            &|_| retries.set(retries.get() + 1),
        );
        assert!(result.is_err());
        assert_eq!(retries.get(), 0);
    }

    #[test]
    fn retry_request_is_written_next_to_the_given_config_only() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join("sub").join("config.toml");
        let config = config_with(&[("model_size", json!("small"))]);
        let path = request_retry(&config, Some(&config_file)).unwrap();
        assert_eq!(
            path,
            dir.path().join("sub").join("config.toml.speech-retry.json")
        );
        let data: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(data["version"], 1);
        assert_eq!(data["id"].as_str().unwrap().len(), 32);
        assert_eq!(data["selection"]["model_size"], "small");
        assert_eq!(data["selection"]["language"], Value::Null);
        assert_eq!(data["selection"].as_object().unwrap().len(), 4);
        // Each request gets a fresh id; no temporary files are left behind.
        let again = request_retry(&config, Some(&config_file)).unwrap();
        let second: Value = serde_json::from_str(&std::fs::read_to_string(again).unwrap()).unwrap();
        assert_ne!(data["id"], second["id"]);
        assert_eq!(
            std::fs::read_dir(dir.path().join("sub")).unwrap().count(),
            1
        );
    }

    // ---- layout ----

    #[test]
    fn change_models_card_never_overlaps() {
        for width in [596.0, 800.0, 1016.0] {
            for ollama in [true, false] {
                for fixed in [true, false] {
                    let layout = quick_layout(width, ollama, fixed, 2);
                    let frames = layout.frames();
                    for (name, f) in &frames {
                        assert!(
                            f.0 >= 0.0
                                && f.0 + f.2 <= width
                                && f.1 >= 0.0
                                && f.1 + f.3 <= layout.height,
                            "{name}"
                        );
                    }
                    for (i, (_, a)) in frames.iter().enumerate() {
                        for (_, b) in &frames[i + 1..] {
                            assert!(!overlap(*a, *b));
                        }
                    }
                    assert_eq!(layout.refresh.is_some(), ollama);
                    assert_eq!(layout.speech_fixed.is_some(), fixed);
                    assert_eq!(layout.speech_model.is_some(), !fixed);
                    assert!(layout.import.1 == pad() && layout.title.1 < layout.detail.1);
                    let (status, model) = (layout.provider_status, layout.cleanup_model);
                    assert!(status.0 == model.0 && status.1 >= model.1 + model.3);
                    assert!(status.1 + status.3 <= layout.message.unwrap().1);
                }
            }
        }
    }

    #[test]
    fn no_message_reserves_no_blank_lines() {
        for width in [596.0, 1016.0] {
            let bare = quick_layout(width, true, false, 0);
            assert!(bare.message.is_none());
            let status = bare.provider_status;
            assert_eq!(bare.height, status.1 + status.3 + pad());
            let one = quick_layout(width, true, false, 1);
            assert_eq!(one.height - bare.height, row_gap() + MESSAGE_LINE);
        }
    }

    fn char_measure(text: &str) -> f64 {
        text.chars().count() as f64 * 7.0
    }

    #[test]
    fn note_lines_are_the_wrapped_lines_and_zero_when_empty() {
        let config = config_with(&[("llm_model", json!("a-rather-long-model-name:latest"))]);
        let missing = cleanup_picker(&config, ListState::Ready, &["other".into()]).2;
        let wrapped = wrap_lines(&missing, 200.0, &char_measure).len();
        assert_eq!(
            note_lines(&[missing.as_str()], 200.0, &char_measure),
            wrapped
        );
        assert!(wrapped > 1);
        assert_eq!(note_lines(&[missing.as_str()], 2000.0, &char_measure), 1);
        assert_eq!(note_lines(&[""], 200.0, &char_measure), 0);
        assert_eq!(note_lines::<&str>(&[], 200.0, &char_measure), 0);
        // Width is floored at 40 like Python's max(width, 40).
        assert_eq!(
            note_lines(&["aaaa bbbb"], 1.0, &char_measure),
            wrap_lines("aaaa bbbb", 40.0, &char_measure).len()
        );
    }

    #[test]
    fn provider_status_names_the_active_provider_only() {
        assert_eq!(
            provider_status_text(&Config::defaults_for(false)),
            "Active provider: Local (Ollama)"
        );
        assert_eq!(
            provider_status_text(&fireworks_config()),
            "Active provider: Fireworks"
        );
        let odd = config_with(&[
            ("llm_provider", json!("plugin-llm")),
            ("llm_base_url", json!("http://x")),
        ]);
        assert_eq!(provider_status_text(&odd), "Active provider: plugin-llm");
        assert_eq!(CLEANUP_LABEL, "Cleanup & Ask");
    }
}
