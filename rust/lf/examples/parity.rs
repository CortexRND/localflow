//! Parity test driver for `tests/test_rust_parity.py` (not a user command).
//!
//! Reads a JSON array of requests from stdin and writes a JSON array of
//! results to stdout, one per request: `{"ok": value}` or `{"error": text}`.
//! Each request is `{"op": name, ...arguments}` and calls the `lf-core`
//! function of the same purpose (`cli` runs `lf::run` in-process against an
//! in-memory keyring); the Python side computes its own result in-process
//! and compares. Nothing here evaluates Python.

use std::io::{self, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{anyhow, bail, Context, Result};
use lf_core::config::{self, Config};
use lf_core::dictionary::{self, DictionaryStore};
use lf_core::history::{EntryUpdate, HistoryStore, NewEntry};
use lf_core::hotphrases::{self, HotPhraseStore, RenderSteps};
use lf_core::secrets::{self, FailingKeyring, KeyringBackend, MemoryKeyring};
use lf_core::{apitoken, formatting, symbols};
use serde_json::{json, Value};

/// Collects WARN and ERROR log messages (what `lf-rs` prints to stderr
/// through its own logger) so `cli` results can include them.
struct CaptureLogger(Mutex<Vec<String>>);

impl log::Log for CaptureLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let mut records = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            records.push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

static LOGGER: CaptureLogger = CaptureLogger(Mutex::new(Vec::new()));

fn take_warnings() -> Vec<String> {
    std::mem::take(&mut *LOGGER.0.lock().unwrap_or_else(PoisonError::into_inner))
}

fn main() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
    let mut input = String::new();
    if let Err(error) = io::stdin().read_to_string(&mut input) {
        eprintln!("parity: cannot read stdin: {error}");
        std::process::exit(2);
    }
    let requests: Vec<Value> = match serde_json::from_str(&input) {
        Ok(Value::Array(items)) => items,
        Ok(_) => {
            eprintln!("parity: expected a JSON array of requests");
            std::process::exit(2);
        }
        Err(error) => {
            eprintln!("parity: invalid JSON: {error}");
            std::process::exit(2);
        }
    };
    // Panics become error results; keep the default hook's stderr report.
    let responses: Vec<Value> = requests
        .iter()
        .map(
            |request| match panic::catch_unwind(AssertUnwindSafe(|| handle(request))) {
                Ok(Ok(value)) => json!({ "ok": value }),
                Ok(Err(error)) => json!({ "error": format!("{error}") }),
                Err(_) => json!({ "error": "panic" }),
            },
        )
        .collect();
    let mut stdout = io::stdout().lock();
    let text = serde_json::to_string(&responses).expect("JSON values serialize");
    if stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .is_err()
    {
        std::process::exit(2);
    }
}

fn arg<'a>(request: &'a Value, name: &str) -> Result<&'a Value> {
    request
        .get(name)
        .ok_or_else(|| anyhow!("request is missing {name:?}"))
}

fn text<'a>(request: &'a Value, name: &str) -> Result<&'a str> {
    arg(request, name)?
        .as_str()
        .ok_or_else(|| anyhow!("{name:?} must be a string"))
}

fn opt_text<'a>(request: &'a Value, name: &str) -> Option<&'a str> {
    request.get(name).and_then(Value::as_str)
}

fn path(request: &Value, name: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(text(request, name)?))
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `[[trigger, expansion], ...]` (or `[from, to]` pairs).
fn pairs(value: Option<&Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in value.and_then(Value::as_array).into_iter().flatten() {
        match item {
            Value::Array(pair) if pair.len() == 2 => {
                if let (Some(a), Some(b)) = (pair[0].as_str(), pair[1].as_str()) {
                    out.push((a.to_string(), b.to_string()));
                }
            }
            // Python apply_replacements: pair.get("from", ""), pair.get("to", "");
            // a non-string target drops the pair.
            Value::Object(map) => {
                let source = map.get("from").and_then(Value::as_str).unwrap_or("");
                if let Some(target) = map.get("to").and_then(Value::as_str) {
                    out.push((source.to_string(), target.to_string()));
                }
            }
            _ => {}
        }
    }
    out
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}

fn error_value(error: anyhow::Error) -> Value {
    json!({ "error": format!("{error}") })
}

fn base_config(request: &Value) -> Result<Config> {
    match opt_text(request, "path") {
        Some(file) => config::load_from(Path::new(file)),
        None => Ok(Config::default()),
    }
}

fn updated_config(request: &Value) -> Result<Config> {
    let base = base_config(request)?;
    config::apply_config_update(&base, request.get("update").unwrap_or(&json!({})))
}

/// `lf::richfmt` output for one Rich `console.print` call: `kind` is
/// `markup` (a str), `text` (`Text(str)`) or `table` (`Table(box=None)`).
fn rich(request: &Value) -> Result<Value> {
    let width = arg(request, "width")?.as_u64().context("width")? as usize;
    let console = lf::richfmt::Console { width };
    Ok(Value::String(match text(request, "kind")? {
        "markup" => console.markup(text(request, "text")?),
        "text" => console.text(text(request, "text")?),
        "table" => {
            let headers = strings(request.get("headers"));
            let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
            let rows: Vec<Vec<String>> = arg(request, "rows")?
                .as_array()
                .context("rows")?
                .iter()
                .map(|row| strings(Some(row)))
                .collect();
            console.table(&headers, &rows)
        }
        other => bail!("unknown rich kind {other:?}"),
    }))
}

fn handle(request: &Value) -> Result<Value> {
    let op = text(request, "op")?;
    match op {
        "rich" => rich(request),
        "config_defaults" => Ok(config::config_to_dict(&Config::default())),
        "config_update" => Ok(config::config_to_dict(&updated_config(request)?)),
        "shortcut_normalize" => Ok(json!(lf_core::shortcuts::normalize_optional_combo(
            text(request, "value")?,
            text(request, "platform")?
        )?)),
        "shortcut_conflicts" => {
            let cfg = updated_config(request)?;
            let changed = strings(request.get("changed"));
            let changed: Vec<&str> = changed.iter().map(String::as_str).collect();
            Ok(json!(lf_core::shortcuts::shortcut_conflicts(
                &cfg,
                &changed,
                text(request, "platform")?
            )
            .iter()
            .map(|(key, _)| *key)
            .collect::<Vec<_>>()))
        }
        "config_toml" => Ok(Value::String(config::config_toml(&updated_config(
            request,
        )?)?)),
        "config_save" => {
            let target = path(request, "path")?;
            let base = Config::default();
            let cfg =
                config::apply_config_update(&base, request.get("update").unwrap_or(&json!({})))?;
            let saved = config::save_config(&cfg, Some(&target))?;
            Ok(Value::String(saved.display().to_string()))
        }
        "config_load" => Ok(config::config_to_dict(&config::load_from(&path(
            request, "path",
        )?)?)),
        "migrate_legacy" => {
            let source = opt_text(request, "path").map(PathBuf::from);
            Ok(match config::migrate_legacy(source.as_deref())? {
                Some(cfg) => config::config_to_dict(&cfg),
                None => Value::Null,
            })
        }
        "paths" => Ok(json!({
            "config_dir": config::try_config_dir()?.display().to_string(),
            "config_path": config::try_config_path()?.display().to_string(),
            "legacy_config_path": config::try_legacy_config_path()?.display().to_string(),
            "token_path": apitoken::try_token_path()?.display().to_string(),
            "history_path": lf_core::history::history_path().display().to_string(),
            "dictionary_path": dictionary::dictionary_path().display().to_string(),
            "hot_phrases_path": hotphrases::hot_phrases_path().display().to_string(),
        })),
        "token" => Ok(json!({ "read": apitoken::read()?, "load": apitoken::load()? })),
        "token_create" => Ok(Value::String(apitoken::load_or_create()?)),
        "dict_apply" => Ok(Value::String(dictionary::apply_replacements(
            text(request, "text")?,
            &pairs(request.get("pairs")),
        ))),
        "stt_prompt" => Ok(to_json(dictionary::stt_prompt(&strings(
            request.get("words"),
        )))?),
        "dict_store" => dict_store(request),
        "history" => history(request),
        "symbols" => Ok(Value::String(symbols::apply_spoken_symbols(
            text(request, "text")?,
            &strings(request.get("commands")),
        ))),
        "formatting" => Ok(Value::String(formatting::apply_spoken_formatting(text(
            request, "text",
        )?))),
        "style" => Ok(Value::String(formatting::apply_style(
            text(request, "text")?,
            text(request, "style")?,
            &strings(request.get("vocabulary")),
        ))),
        "normalize_trigger" => Ok(Value::String(hotphrases::normalize_trigger(text(
            request, "text",
        )?))),
        "hot_expand" => Ok(Value::String(hotphrases::expand_hot_phrases(
            text(request, "text")?,
            &pairs(request.get("phrases")),
        ))),
        "hot_matched" => Ok(to_json(hotphrases::matched_triggers(
            text(request, "text")?,
            &pairs(request.get("phrases")),
        ))?),
        "render" => render(request),
        "hot_store" => hot_store(request),
        "cli" => cli(request),
        other => bail!("unknown op {other:?}"),
    }
}

/// Deleting a missing entry fails, as with a real keyring (Python keyring's
/// `PasswordDeleteError`); `MemoryKeyring` itself treats it as a no-op.
struct StrictMemory(Arc<MemoryKeyring>);

impl KeyringBackend for StrictMemory {
    fn get_password(&self, service: &str, name: &str) -> Result<Option<String>> {
        self.0.get_password(service, name)
    }

    fn set_password(&self, service: &str, name: &str, value: &str) -> Result<()> {
        self.0.set_password(service, name, value)
    }

    fn delete_password(&self, service: &str, name: &str) -> Result<()> {
        if self.0.value(service, name).is_none() {
            bail!("Password not found");
        }
        self.0.delete_password(service, name)
    }
}

/// `lf::run(args)` in-process with `stdin` as input and an in-memory keyring
/// seeded from `keyring` (`{name: value}`), or a keyring whose every call
/// fails with `failing_keyring`. Returns `{code, out, err, keyring, warnings}`,
/// `warnings` being the messages `lf-rs` would log to stderr.
fn cli(request: &Value) -> Result<Value> {
    let mut argv = vec!["lf-rs".to_string()];
    argv.extend(strings(request.get("args")));
    let memory = Arc::new(MemoryKeyring::new());
    for (name, value) in request
        .get("keyring")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let value = value.as_str().context("keyring values must be strings")?;
        memory.set_password(secrets::SERVICE, name, value)?;
    }
    let backend: Arc<dyn KeyringBackend> = match opt_text(request, "failing_keyring") {
        Some(message) => Arc::new(FailingKeyring(message.to_string())),
        None => Arc::new(StrictMemory(Arc::clone(&memory))),
    };
    let previous = secrets::set_keyring_backend(Some(backend));
    secrets::reset_keyring_warning();
    take_warnings();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut input = io::Cursor::new(opt_text(request, "stdin").unwrap_or("").as_bytes().to_vec());
    let mut clipboard = NoClipboard;
    let code = lf::run(
        argv,
        &mut lf::Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
            clipboard: &mut clipboard,
            server_url: None,
            terminal: false,
        },
    );
    secrets::set_keyring_backend(previous);
    Ok(json!({
        "code": code,
        "out": String::from_utf8(out)?,
        "err": String::from_utf8(err)?,
        "keyring": memory.values(secrets::SERVICE),
        "warnings": take_warnings(),
    }))
}

struct NoClipboard;

impl lf::Clipboard for NoClipboard {
    fn set_text(&mut self, _text: &str) -> Result<()> {
        bail!("no clipboard in the parity driver")
    }
}

fn render(request: &Value) -> Result<Value> {
    let phrases = pairs(request.get("phrases"));
    let replacements = pairs(request.get("replacements"));
    let style = opt_text(request, "style").map(str::to_string);
    let replace = |t: &str| dictionary::apply_replacements(t, &replacements);
    let fmt = |t: &str| formatting::apply_spoken_formatting(t);
    let styled = |t: &str| formatting::apply_style::<&str>(t, style.as_deref().unwrap_or(""), &[]);
    let mut steps = RenderSteps::default();
    if !replacements.is_empty() {
        steps.replace = Some(&replace);
    }
    if request
        .get("formatting")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        steps.fmt = Some(&fmt);
    }
    if style.is_some() {
        steps.style = Some(&styled);
    }
    Ok(Value::String(hotphrases::render_dictation(
        text(request, "text")?,
        &phrases,
        &steps,
    )))
}

fn dict_store(request: &Value) -> Result<Value> {
    let store = DictionaryStore::new(Some(path(request, "path")?));
    let mut results = Vec::new();
    for op in arg(request, "ops")?
        .as_array()
        .context("ops must be a list")?
    {
        let result = (|| -> Result<Value> {
            match text(op, "op")? {
                "list" => to_json(store.list()),
                "add_word" => to_json(store.add_word(text(op, "word")?)?),
                "add_replacement" => {
                    to_json(store.add_replacement(text(op, "from")?, text(op, "to")?)?)
                }
                "remove" => to_json(store.remove(text(op, "value")?, opt_text(op, "kind"))?),
                "import" => to_json(store.import_file(&path(op, "file")?)?),
                other => bail!("unknown dictionary op {other:?}"),
            }
        })();
        results.push(result.unwrap_or_else(error_value));
    }
    Ok(Value::Array(results))
}

fn opt_string(op: &Value, name: &str) -> Option<String> {
    opt_text(op, name).map(str::to_string)
}

fn opt_f64(op: &Value, name: &str) -> Option<f64> {
    op.get(name).and_then(Value::as_f64)
}

fn history_op(store: &HistoryStore, op: &Value) -> Result<Value> {
    match text(op, "op")? {
        "recent" => to_json(store.recent(
            op.get("limit").and_then(Value::as_i64).unwrap_or(50),
            opt_text(op, "query"),
            op.get("offset").and_then(Value::as_i64).unwrap_or(0),
        )?),
        "get" => to_json(store.get(arg(op, "id")?.as_i64().context("id")?)?),
        "last_ok" => to_json(store.last_ok()?),
        "stats" => to_json(store.stats(opt_f64(op, "now"))?),
        "update" => {
            let changes = EntryUpdate {
                status: opt_string(op, "status"),
                raw_text: opt_string(op, "raw_text"),
                final_text: opt_string(op, "final_text"),
                duration_s: opt_f64(op, "duration_s"),
                audio_path: opt_string(op, "audio_path"),
            };
            to_json(store.update(arg(op, "id")?.as_i64().context("id")?, &changes)?)
        }
        "delete" => to_json(store.delete(arg(op, "id")?.as_i64().context("id")?)?),
        "clear" => to_json(store.clear()?),
        "add" => {
            let mut entry = NewEntry::new(text(op, "kind")?);
            entry.app_name = opt_string(op, "app_name");
            entry.bundle_id = opt_string(op, "bundle_id");
            entry.category = opt_string(op, "category");
            if let Some(duration) = opt_f64(op, "duration_s") {
                entry.duration_s = duration;
            }
            if let Some(raw) = opt_string(op, "raw_text") {
                entry.raw_text = raw;
            }
            if let Some(final_text) = opt_string(op, "final_text") {
                entry.final_text = final_text;
            }
            if let Some(status) = opt_string(op, "status") {
                entry.status = status;
            }
            entry.audio_path = opt_string(op, "audio_path");
            entry.ts = opt_f64(op, "ts");
            to_json(store.add(&entry)?)
        }
        other => bail!("unknown history op {other:?}"),
    }
}

fn history(request: &Value) -> Result<Value> {
    let enabled = request
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let store = HistoryStore::open(
        Some(path(request, "path")?),
        opt_text(request, "clips").map(PathBuf::from),
        enabled,
    )?;
    let mut results = Vec::new();
    for op in arg(request, "ops")?
        .as_array()
        .context("ops must be a list")?
    {
        results.push(history_op(&store, op).unwrap_or_else(error_value));
    }
    Ok(Value::Array(results))
}

fn hot_store(request: &Value) -> Result<Value> {
    let store = HotPhraseStore::new(path(request, "path")?);
    let mut results = Vec::new();
    for op in arg(request, "ops")?
        .as_array()
        .context("ops must be a list")?
    {
        let result = (|| -> Result<Value> {
            match text(op, "op")? {
                "list" => to_json(store.list()),
                "enabled" => to_json(store.enabled()?),
                "add" => to_json(store.add(
                    text(op, "trigger")?,
                    text(op, "text")?,
                    op.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                )?),
                "delete" => to_json(store.delete(text(op, "id")?)?),
                other => bail!("unknown hot phrase op {other:?}"),
            }
        })();
        results.push(result.unwrap_or_else(error_value));
    }
    Ok(Value::Array(results))
}
