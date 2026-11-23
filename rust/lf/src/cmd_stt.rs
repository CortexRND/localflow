use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use lf_core::config;
use lf_providers::pyfmt::py_repr;
use lf_server::api::{self, STT_IDS};
use lf_stt::{model_storage, setup_downloads};
use serde_json::{json, Value};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::args::{SttCommand, SttDevice};
use crate::{fail, outln, pyfmt, Io};

pub fn run(command: SttCommand, io: &mut Io<'_>) -> Result<()> {
    match command {
        SttCommand::List { json } => list(json, io),
        SttCommand::Use {
            provider,
            model,
            device,
        } => select(&provider, model, device, io),
        SttCommand::Download { model_id } => download(&model_id, io),
    }
}

fn unknown_provider(provider: &str, ids: &[&str]) -> String {
    py_repr(&format!(
        "unknown STT provider {}; valid ids: {}",
        py_repr(provider),
        ids.join(", ")
    ))
}

fn provider_ids(probe: &Value) -> Vec<&str> {
    let mut ids = STT_IDS.to_vec();
    if let Some(plugins) = probe["plugins"].as_object() {
        ids.extend(plugins.keys().map(String::as_str));
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn runtime_module(provider: &str) -> &str {
    match provider {
        "faster-whisper" => "faster_whisper",
        "mlx-whisper" => "mlx_whisper",
        "parakeet" => "parakeet_mlx",
        _ => "",
    }
}

fn list(json_output: bool, io: &mut Io<'_>) -> Result<()> {
    let config = config::load_config()?;
    let provider = api::resolve_stt_provider(config.field_str("stt_backend").unwrap_or("auto"));
    let probe = probe_python("list", provider);
    let ids = provider_ids(&probe);
    let providers = ids
        .iter()
        .map(|&id| {
            let (offline, available) = if STT_IDS.contains(&id) {
                let available = (id == "faster-whisper" || cfg!(target_os = "macos"))
                    && probe["modules"][runtime_module(id)]
                        .as_bool()
                        .unwrap_or(false);
                (Value::Bool(true), available)
            } else {
                (probe["plugins"][id].clone(), true)
            };
            json!({"id": id, "offline": offline, "available": available})
        })
        .collect::<Vec<_>>();
    let mut selected = if STT_IDS.contains(&provider) {
        api::models(&config, "stt", provider, None)
    } else if ids.contains(&provider) {
        probe["selected"].clone()
    } else {
        json!({"models": [], "error": unknown_provider(provider, &ids)})
    };
    if json_output {
        let result = json!({
            "providers": providers,
            "models": selected["models"].take(),
            "error": selected["error"].take(),
        });
        return outln!(io.out, "{}", pyfmt::json_dumps(&result));
    }
    for provider in providers {
        let id = provider["id"].as_str().unwrap_or_default();
        let state = if provider["available"] == true {
            "available"
        } else {
            "unavailable"
        };
        outln!(io.out, "{id}: {state}")?;
    }
    if let Some(models) = selected["models"].as_array() {
        for model in models {
            let id = model["id"].as_str().unwrap_or_default();
            let label = model["label"].as_str().unwrap_or_default();
            outln!(io.out, "{id}: {label}")?;
        }
    }
    if let Some(error) = selected["error"].as_str().filter(|s| !s.is_empty()) {
        outln!(
            io.out,
            "{}",
            wrap_text(&format!("error: {error}"), console_width())
        )?;
    }
    Ok(())
}

fn console_width() -> usize {
    env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&width| width > 0)
        .unwrap_or(80)
}

fn wrap_text(text: &str, width: usize) -> String {
    let mut lines = Vec::new();
    for mut rest in text.split('\n') {
        let mut line = String::new();
        let mut cells = 0;
        while !rest.is_empty() {
            let word_start = rest
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(rest.len());
            let word_end = word_start
                + rest[word_start..]
                    .find(char::is_whitespace)
                    .unwrap_or(rest.len() - word_start);
            let end = word_end
                + rest[word_end..]
                    .find(|c: char| !c.is_whitespace())
                    .unwrap_or(rest.len() - word_end);
            let (word, trailing) = rest[..end].split_at(word_end);
            if cells + word.width() > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                cells = 0;
            }
            for ch in word.chars() {
                let ch_width = ch.width().unwrap_or(0);
                if cells + ch_width > width && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    cells = 0;
                }
                line.push(ch);
                cells += ch_width;
            }
            for ch in trailing.chars().take(width.saturating_sub(cells)) {
                line.push(ch);
                cells += ch.width().unwrap_or(0);
            }
            rest = &rest[end..];
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn select(provider: &str, model: Option<String>, device: SttDevice, io: &mut Io<'_>) -> Result<()> {
    let selected = api::resolve_stt_provider(provider);
    if !STT_IDS.contains(&selected) {
        let probe = probe_python("use", selected);
        let ids = provider_ids(&probe);
        if !ids.contains(&selected) {
            return fail(unknown_provider(selected, &ids));
        }
        if let Some(error) = probe["error"].as_str() {
            return fail(error);
        }
    }
    let config = config::load_config()?;
    let updated = config::apply_config_update(
        &config,
        &json!({"stt": {
            "provider": if provider == "mlx" { selected } else { provider },
            "model": model.as_deref().filter(|s| !s.is_empty()).or_else(|| config.field_str("model_size")),
            "device": match device {
                SttDevice::Auto => "auto",
                SttDevice::Cpu => "cpu",
                SttDevice::Cuda => "cuda",
            },
        }}),
    )?;
    outln!(
        io.out,
        "saved {}",
        config::save_config(&updated, None)?.display()
    )
}

fn download(model_id: &str, io: &mut Io<'_>) -> Result<()> {
    let Some(repo) = model_storage::python_catalog_repo(model_id) else {
        return fail(format!("unknown speech model: {model_id}"));
    };
    let apple_runtime = if model_id.starts_with("mlx-") {
        Some(("mlx", "mlx-whisper"))
    } else if model_id == "parakeet-v2" {
        Some(("parakeet", "parakeet"))
    } else {
        None
    };
    if let Some((extra, provider)) = apple_runtime {
        let probe = probe_python("runtime", provider);
        if probe["modules"][runtime_module(provider)] != true {
            return fail(format!("install the runtime: pip install -e '.[{extra}]'"));
        }
        let apple_silicon = probe["apple_silicon"]
            .as_bool()
            .unwrap_or(cfg!(all(target_os = "macos", target_arch = "aarch64")));
        if !apple_silicon {
            return fail(format!("{model_id} is only available on Apple Silicon."));
        }
    }
    let endpoint = env::var("HF_ENDPOINT").unwrap_or_else(|_| "https://huggingface.co".into());
    let cache = model_storage::hf_hub_cache(None);
    let mut progress = None;
    let terminal = std::io::stdout().is_terminal();
    let width = console_width();
    let result = setup_downloads::download_snapshot(&repo, &cache, &endpoint, |done, total| {
        progress = Some((done, total));
        if terminal {
            let _ = write!(
                io.out,
                "\r\x1b[2K{}",
                progress_line(model_id, progress, width)
            );
            let _ = io.out.flush();
        }
    });
    if terminal && progress.is_some() {
        write!(io.out, "\r\x1b[2K")?;
    }
    outln!(io.out, "{}", progress_line(model_id, progress, width))?;
    if let Err(error) = result {
        let lower = error.to_lowercase();
        let message: String = if [
            "connect",
            "timed out",
            "network",
            "reconstruction",
            "operation timed out",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            "Network problem while downloading — check your connection, then Retry.".into()
        } else {
            error
                .lines()
                .next()
                .unwrap_or("DownloadError")
                .trim()
                .chars()
                .take(140)
                .collect()
        };
        return fail(message);
    }
    outln!(io.out, "downloaded {model_id}")
}

fn progress_line(model_id: &str, progress: Option<(u64, u64)>, width: usize) -> String {
    let title = format!("Downloading {model_id}");
    let (fraction, detail, remaining) = match progress {
        Some((done, total)) => (
            (done.min(total) as f64) / (total as f64),
            format!(
                "{:.0}/{:.0} MB",
                (done as f64) / 1_000_000.,
                (total as f64) / 1_000_000.
            ),
            if done >= total { "0:00:00" } else { "-:--:--" },
        ),
        None => (0., String::new(), "-:--:--"),
    };
    let width = width
        .saturating_sub(title.len() + detail.len() + 4 + remaining.len() + 4)
        .min(40);
    let half_cells = (fraction * width as f64 * 2.).round_ties_even() as usize;
    let full = half_cells / 2;
    let half = half_cells % 2;
    let bar = format!(
        "{}{}{}",
        "━".repeat(full),
        "╸".repeat(half),
        " ".repeat(width - full - half)
    );
    let percent = (fraction * 100.).round_ties_even() as u8;
    format!("{title} {bar} {percent:>3}% {detail} {remaining}")
}

const PROBE: &str = r#"
import contextlib
import dataclasses
import importlib.metadata
import importlib.util
import json
import platform
import sys

def available(module):
    try:
        return importlib.util.find_spec(module) is not None
    except (ImportError, ValueError):
        return False

mode, selected = sys.argv[1:]
result = {}
if mode in ("runtime", "list"):
    result["modules"] = {m: available(m) for m in ("faster_whisper", "mlx_whisper", "parakeet_mlx")}
    result["apple_silicon"] = sys.platform == "darwin" and platform.machine() in ("arm64", "aarch64")
if mode in ("list", "use"):
    builtins = ("faster-whisper", "mlx-whisper", "parakeet")
    entries = {}
    for entry in importlib.metadata.entry_points(group="localflow.stt_providers"):
        if entry.name not in builtins:
            entries.setdefault(entry.name, entry)
    result["plugins"] = dict.fromkeys(entries)
    with contextlib.redirect_stdout(sys.stderr):
        if mode == "list":
            for name, entry in entries.items():
                result["plugins"][name] = entry.load().offline
            if selected in entries:
                try:
                    models = [dataclasses.asdict(m) for m in entries[selected].load()().list_models()]
                    result["selected"] = {"models": models, "error": None}
                except Exception as exc:
                    result["selected"] = {"models": [], "error": str(exc)}
        elif selected in entries:
            try:
                entries[selected].load()
            except Exception as exc:
                result["error"] = str(exc)
print(json.dumps(result))
"#;

// Availability and entry points describe the installed Python runtime, not Rust
// engine features. Only probe that boundary; config, catalogs and downloads are native.
fn probe_python(mode: &str, provider: &str) -> Value {
    let python = if cfg!(windows) {
        "python.exe"
    } else {
        "python3"
    };
    let launcher = if cfg!(windows) { "lf.exe" } else { "lf" };
    let venv = env::var_os("VIRTUAL_ENV").map(|root| {
        Path::new(&root)
            .join(if cfg!(windows) { "Scripts" } else { "bin" })
            .join(python)
    });
    let path = env::var_os("PATH").unwrap_or_default();
    let installed = env::split_paths(&path).find_map(|dir| {
        let binary = dir.join(launcher).canonicalize().ok()?;
        let python = binary.with_file_name(python);
        python.is_file().then_some(python)
    });
    venv.into_iter()
        .chain(installed)
        .chain([PathBuf::from(python), PathBuf::from("python")])
        .find_map(|interpreter| {
            let output = Command::new(interpreter)
                .args(["-c", PROBE, mode, provider])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            serde_json::from_slice(&output.stdout).ok()
        })
        .unwrap_or(Value::Null)
}
