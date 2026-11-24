use std::env;
use std::io::IsTerminal;

use anyhow::Result;
use lf_core::config;
use lf_providers::pyfmt::py_repr;
use lf_server::api::{self, STT_IDS};
use lf_stt::{model_storage, setup_downloads};
use serde_json::{json, Value};

use crate::args::{SttCommand, SttDevice};
#[cfg(test)]
use crate::richfmt::effective_width;
use crate::richfmt::{console_width, legacy_windows_console, Console};
use crate::{fail, outln, pyfmt, CliError, Io};

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
    crashed(&probe, io)?;
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
        // console.print(f"[red]error:[/red] {error}")
        let console = Console::detect();
        write!(
            io.out,
            "{}",
            console.markup(&format!("[red]error:[/red] {error}"))
        )?;
    }
    Ok(())
}

fn select(provider: &str, model: Option<String>, device: SttDevice, io: &mut Io<'_>) -> Result<()> {
    let selected = api::resolve_stt_provider(provider);
    if !STT_IDS.contains(&selected) {
        let probe = probe_python("use", selected);
        crashed(&probe, io)?;
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
        crashed(&probe, io)?;
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
    let ascii = legacy_windows_console();
    let width = console_width(ascii);
    let result = setup_downloads::download_snapshot(
        &repo,
        &cache,
        &endpoint,
        |done, total| {
            progress = Some((done, total));
            if terminal {
                let _ = write!(
                    io.out,
                    "\r\x1b[2K{}",
                    progress_line(model_id, progress, width, ascii)
                );
                let _ = io.out.flush();
            }
        },
        ctrl_c(),
    );
    if terminal && progress.is_some() {
        write!(io.out, "\r\x1b[2K")?;
    }
    outln!(
        io.out,
        "{}",
        progress_line(model_id, progress, width, ascii)
    )?;
    if result
        .as_ref()
        .is_err_and(|error| error == setup_downloads::INTERRUPTED)
    {
        // click: KeyboardInterrupt -> blank line + "Aborted!" on stderr, exit 1.
        outln!(io.err)?;
        outln!(io.err, "Aborted!")?;
        return Err(CliError::Exit(1).into());
    }
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

/// Resolves on Ctrl+C. The handler is registered on first poll by `stt
/// download`'s transfer, so other commands keep the default Ctrl+C behavior.
/// If registration fails the default handler is still in place and this
/// never resolves.
async fn ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn progress_line(
    model_id: &str,
    progress: Option<(u64, u64)>,
    width: usize,
    ascii: bool,
) -> String {
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
    let (bar, half_bar) = if ascii { ("-", " ") } else { ("━", "╸") };
    let bar = format!(
        "{}{}{}",
        bar.repeat(full),
        half_bar.repeat(half),
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
import traceback

def available(module):
    try:
        return importlib.util.find_spec(module) is not None
    except (ImportError, ValueError):
        return False

def crash(exc):
    report = traceback.TracebackException.from_exception(exc)
    del report.stack[0]
    text = "".join(report.format())
    return text if report.stack else "Traceback (most recent call last):\n" + text

sys.stdout.write("lf-stt-probe:%d:" % available("localflow"))
sys.stdout.flush()
mode, selected = sys.argv[1:]
result = {}
if mode in ("runtime", "list"):
    result["modules"] = {m: available(m) for m in ("faster_whisper", "mlx_whisper", "parakeet_mlx")}
    result["apple_silicon"] = sys.platform == "darwin" and platform.machine() in ("arm64", "aarch64")
entries = {}
if mode in ("list", "use"):
    builtins = ("faster-whisper", "mlx-whisper", "parakeet")
    try:
        group = importlib.metadata.entry_points()
        if hasattr(group, "select"):
            group = group.select(group="localflow.stt_providers")
        else:
            group = group.get("localflow.stt_providers", ())
        for entry in group:
            if entry.name not in builtins:
                entries.setdefault(entry.name, entry)
    except Exception as exc:
        result["traceback"] = crash(exc)
    result["plugins"] = dict.fromkeys(entries)
with contextlib.redirect_stdout(sys.stderr):
    if mode == "list":
        for name in sorted(entries):
            try:
                result["plugins"][name] = entries[name].load().offline
            except Exception as exc:
                result["traceback"] = crash(exc)
                break
        if selected in entries and "traceback" not in result:
            try:
                models = [dataclasses.asdict(m) for m in entries[selected].load()().list_models()]
                result["selected"] = {"models": models, "error": None}
            except Exception as exc:
                result["selected"] = {"models": [], "error": str(exc)}
    elif mode == "use" and selected in entries:
        try:
            entries[selected].load()
        except KeyError as exc:
            result["error"] = str(exc)
        except Exception as exc:
            result["traceback"] = crash(exc)
print(json.dumps(result))
"#;

// Availability and entry points describe the installed Python runtime, not Rust
// engine features. Only probe that boundary; config, catalogs and downloads are native.
// The interpreter LocalFlow is installed in answers, like the Python CLI; with no
// such candidate, the first one that ran the probe does (crashes included).
fn probe_python(mode: &str, provider: &str) -> Value {
    let results = crate::python::candidates().into_iter().map(|interpreter| {
        let output = crate::python::probe(&interpreter, PROBE)
            .args([mode, provider])
            .output()
            .ok()?;
        probe_result(output.status.code(), &output.stdout, &output.stderr)
    });
    choose(results)
}

/// The first answer from an interpreter with LocalFlow installed, else the
/// first answer at all. Stops probing at the first LocalFlow answer.
fn choose(results: impl IntoIterator<Item = Option<(bool, Value)>>) -> Value {
    let mut first = None;
    for (localflow, result) in results.into_iter().flatten() {
        if localflow {
            return result;
        }
        first.get_or_insert(result);
    }
    first.unwrap_or(Value::Null)
}

/// `None` when the interpreter could not start the probe (spawn or imports);
/// otherwise whether it has LocalFlow installed and its answer. Any failure
/// after the start is that answer, like Python's crash.
fn probe_result(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Option<(bool, Value)> {
    let stdout = stdout.strip_prefix(b"lf-stt-probe:")?;
    let (localflow, stdout) = match stdout {
        [b'1', b':', rest @ ..] => (true, rest),
        [b'0', b':', rest @ ..] => (false, rest),
        _ => return None,
    };
    if code == Some(0) {
        if let Ok(result) = serde_json::from_slice(stdout) {
            return Some((localflow, result));
        }
    }
    Some((
        localflow,
        json!({
            "traceback": String::from_utf8_lossy(stderr),
            "exit": code.filter(|&code| code != 0).unwrap_or(1),
        }),
    ))
}

/// Python lets a plug-in's exception escape click: traceback, exit 1.
fn crashed(probe: &Value, io: &mut Io<'_>) -> Result<()> {
    let Some(traceback) = probe["traceback"].as_str() else {
        return Ok(());
    };
    write!(io.err, "{traceback}")?;
    let code = probe["exit"]
        .as_i64()
        .and_then(|code| i32::try_from(code).ok());
    Err(CliError::Exit(code.unwrap_or(1)).into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{choose, effective_width, probe_result, progress_line};

    #[test]
    fn only_interpreters_that_cannot_run_the_probe_are_skipped() {
        assert_eq!(probe_result(Some(1), b"", b"SyntaxError"), None);
        assert_eq!(probe_result(Some(0), b"{}", b""), None);
        assert_eq!(probe_result(Some(0), b"lf-stt-probe:{}", b""), None);
        assert_eq!(
            probe_result(Some(0), b"lf-stt-probe:1:{\"plugins\": {}}\r\n", b""),
            Some((true, json!({"plugins": {}})))
        );
        assert_eq!(
            probe_result(Some(3), b"lf-stt-probe:0:", b"bye\n"),
            Some((false, json!({"traceback": "bye\n", "exit": 3})))
        );
        assert_eq!(
            probe_result(None, b"lf-stt-probe:1:", b""),
            Some((true, json!({"traceback": "", "exit": 1})))
        );
        assert_eq!(
            probe_result(Some(0), b"lf-stt-probe:0:", b"oops"),
            Some((false, json!({"traceback": "oops", "exit": 1})))
        );
    }

    #[test]
    fn the_localflow_interpreter_answers() {
        let other = json!({"plugins": {}});
        let install = json!({"plugins": {"demo": false}});
        let crash = json!({"traceback": "boom", "exit": 1});
        let first = json!({"plugins": {"first": null}});
        let answer = |localflow, value: &serde_json::Value| Some((localflow, value.clone()));
        assert_eq!(
            choose([None, answer(false, &other), answer(true, &install)]),
            install
        );
        assert_eq!(
            choose([answer(false, &other), answer(true, &crash), None]),
            crash
        );
        assert_eq!(
            choose([None, answer(false, &first), answer(false, &other)]),
            first
        );
        assert_eq!(choose([None, None]), serde_json::Value::Null);
        let mut probed = 0;
        let results = [Some((true, json!(1))), Some((true, json!(2)))]
            .into_iter()
            .inspect(|_| probed += 1);
        assert_eq!(choose(results), json!(1));
        assert_eq!(probed, 1);
    }

    #[test]
    fn legacy_windows_reserves_last_column_like_rich() {
        assert_eq!(effective_width(Some(80), false), 80);
        assert_eq!(effective_width(None, false), 80);
        assert_eq!(effective_width(Some(80), true), 79);
        assert_eq!(effective_width(None, true), 79);
        assert_eq!(
            progress_line(
                "fw-tiny",
                Some((1, 1)),
                effective_width(Some(80), true),
                true
            ),
            format!("Downloading fw-tiny {} 100% 0/0 MB 0:00:00", "-".repeat(39))
        );
        assert_eq!(
            progress_line(
                "fw-tiny",
                Some((1, 1)),
                effective_width(Some(80), false),
                false
            ),
            format!("Downloading fw-tiny {} 100% 0/0 MB 0:00:00", "━".repeat(40))
        );
    }

    #[test]
    fn progress_bar_uses_rich_ascii_glyphs_on_legacy_windows() {
        let progress = Some((1, 16));
        let rest = " ".repeat(37);
        assert_eq!(
            progress_line("m", progress, 80, false),
            format!("Downloading m ━━╸{rest}   6% 0/0 MB -:--:--")
        );
        assert_eq!(
            progress_line("m", progress, 80, true),
            format!("Downloading m -- {rest}   6% 0/0 MB -:--:--")
        );
    }
}
