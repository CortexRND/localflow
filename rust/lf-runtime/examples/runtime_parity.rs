//! Parity driver for the runtime half of `tests/test_rust_parity.py`.
//!
//! Same protocol as `lf/examples/parity.rs`: a JSON array of `{"op": ...}`
//! requests on stdin, `{"ok": value}` / `{"error": text}` results on stdout.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use lf_core::config::{config_to_dict, Config};
use lf_providers::base::{Health, ModelInfo, ProviderError};
use lf_providers::LlmProvider;
use lf_runtime::context::{category_for, AppInfo};
use lf_runtime::live::{merge_live, runtime_changes, runtime_fields};
use lf_runtime::menubar::{PollResult, PollSchedule};
use lf_runtime::singleinstance;
use lf_runtime::status::title_prefix;
use lf_runtime::transform::{command_prompt, run_transform, transform_prompt};
use serde_json::{json, Value};

fn main() {
    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        std::process::exit(2);
    }
    let Ok(Value::Array(requests)) = serde_json::from_str::<Value>(&input) else {
        eprintln!("runtime_parity: expected a JSON array of requests");
        std::process::exit(2);
    };
    let responses: Vec<Value> = requests
        .iter()
        .map(|r| match handle(r) {
            Ok(v) => json!({ "ok": v }),
            Err(e) => json!({ "error": e.to_string() }),
        })
        .collect();
    let mut out = io::stdout().lock();
    let _ = out.write_all(
        serde_json::to_string(&responses)
            .unwrap_or_default()
            .as_bytes(),
    );
    let _ = out.flush();
}

fn text<'a>(r: &'a Value, name: &str) -> Result<&'a str> {
    r.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string {name:?}"))
}

fn strings(r: &Value, name: &str) -> Vec<String> {
    r.get(name)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Config::default() with the request's flat field overrides.
fn config(r: &Value, name: &str) -> Config {
    let mut c = Config::default();
    if let Some(map) = r.get(name).and_then(Value::as_object) {
        for (k, v) in map {
            c.set_field(k, v.clone());
        }
    }
    c
}

/// Replies with a canned completion (Transformer post-processing).
struct Canned(String);

impl LlmProvider for Canned {
    fn id(&self) -> &'static str {
        "canned"
    }
    fn offline(&self) -> bool {
        true
    }
    fn complete(&self, _s: &str, _u: &str, _m: Option<i64>) -> Result<String, ProviderError> {
        Ok(self.0.clone())
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
    fn healthcheck(&self) -> Health {
        Health::ok()
    }
}

fn handle(r: &Value) -> Result<Value> {
    match text(r, "op")? {
        "category_for" => {
            let app = r.get("app").filter(|a| !a.is_null()).map(|a| AppInfo {
                name: a
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                bundle_id: a
                    .get("bundle_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            });
            Ok(json!(category_for(app.as_ref(), &strings(r, "overrides"))))
        }
        "title_prefix" => Ok(json!(title_prefix(text(r, "state")?))),
        "command_prompt" => Ok(json!(command_prompt(
            text(r, "instruction")?,
            text(r, "selection")?
        ))),
        "transform_prompt" => transform_prompt(text(r, "preset")?, text(r, "text")?)
            .map(Value::String)
            .map_err(|e| anyhow!(e)),
        "transform_run" => Ok(json!(run_transform(
            &Canned(text(r, "reply")?.to_string()),
            text(r, "prompt")?
        ))),
        "merge_live" => {
            let running = config(r, "running");
            let fresh = config(r, "fresh");
            let reloaded: BTreeSet<&str> =
                if r.get("reloaded").and_then(Value::as_bool).unwrap_or(false) {
                    runtime_fields()
                } else {
                    BTreeSet::new()
                };
            let (merged, changed) = merge_live(&running, &fresh, &reloaded);
            Ok(json!({
                "changed": changed.into_iter().collect::<Vec<_>>(),
                "groups": runtime_changes(&running, &fresh).into_iter().collect::<Vec<_>>(),
                "merged": config_to_dict(&merged),
            }))
        }
        "poll_schedule" => {
            // Steps: ["start", t] | ["finish", t, status|null] | ["poke"]; returns next_due after each.
            let mut s = PollSchedule::default();
            let mut out = Vec::new();
            for step in r
                .get("steps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let kind = step.get(0).and_then(Value::as_str).unwrap_or_default();
                let t = step.get(1).and_then(Value::as_f64).unwrap_or(0.0);
                match kind {
                    "start" => s.started(t),
                    "finish" => {
                        let result = match step.get(2) {
                            Some(Value::Null) | None => PollResult::Down,
                            Some(status) => PollResult::Ok(status.clone()),
                        };
                        s.finished(&result, t)
                    }
                    "poke" => s.poke(),
                    other => bail!("unknown step {other:?}"),
                }
                out.push(json!(s.next_due()));
            }
            Ok(Value::Array(out))
        }
        "lock_try" => {
            let path = Path::new(text(r, "path")?);
            let held = singleinstance::acquire_at(path)?;
            Ok(json!(held.is_some()))
        }
        "lock_holder" => {
            let (pid, ts) = singleinstance::read_holder_at(Path::new(text(r, "path")?));
            Ok(json!([pid, ts]))
        }
        other => bail!("unknown op {other:?}"),
    }
}
