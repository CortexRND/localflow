//! JSON-line parity driver. Each line is a request array; responses are flushed
//! immediately. Lease guards survive requests and are dropped on stdin EOF.
use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use lf_core::config::Config;
use lf_stt::{model_storage as ms, stt_lease as sl};
use serde_json::{json, Value};

fn handle(request: &Value, guards: &mut Vec<sl::LeaseGuard>) -> Result<Value> {
    let text = |key| {
        request[key]
            .as_str()
            .ok_or_else(|| anyhow!("missing string: {key}"))
    };
    let root = request["root"].as_str().map(Path::new);
    let cache = request["cache"].as_str().map(Path::new);
    let mut config = Config::default();
    if let Some(fields) = request["config"].as_object() {
        for (key, value) in fields {
            config.set_field(key, value.clone());
        }
    }
    let mlx = request["mlx"]
        .as_bool()
        .unwrap_or_else(ms::default_mlx_available);
    let leases: Option<Vec<sl::Lease>> = request
        .get("leases")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()?;
    Ok(match text("op")? {
        "rms_dbfs" => {
            let pcm: Vec<f32> = serde_json::from_value(request["pcm"].clone())?;
            let value = lf_audio::rms_dbfs(&pcm);
            if value.is_finite() {
                json!(value)
            } else {
                json!("-inf")
            }
        }
        "runtime_repo" => json!(sl::runtime_repo(text("backend")?, text("size")?)),
        "selection_backend" => json!(sl::selection_backend(request["backend"].as_str())),
        "hf_hub_cache" => json!(ms::hf_hub_cache(Some(&serde_json::from_value(
            request["env"].clone()
        )?))),
        "catalog" => json!(ms::catalog()),
        "repo_folder_name" => json!(ms::repo_folder_name(text("repo")?)),
        "selected_repos" => match ms::selected_repos(&config, || mlx) {
            ms::SelectedRepos::Known(repos) => json!(repos),
            ms::SelectedRepos::Unknown => json!("UNKNOWN"),
        },
        "format_bytes" => json!(ms::format_bytes(
            request["size"]
                .as_i64()
                .ok_or_else(|| anyhow!("missing integer size"))?
        )),
        "inventory" => json!(ms::inventory(&config, cache, || mlx, leases.as_deref())),
        "delete_unused" => json!(ms::delete_unused(
            text("repo")?,
            &config,
            cache,
            || mlx,
            leases.as_deref()
        )?),
        "folder_size" => json!(ms::folder_size(Path::new(text("path")?))),
        "record" => {
            let guard = sl::record(
                text("backend")?,
                text("size")?,
                root,
                request["scoped"].as_bool().unwrap_or(false),
            )
            .ok_or_else(|| anyhow!("record failed"))?;
            let result = json!({"path": guard.path(), "pid": std::process::id()});
            guards.push(guard);
            result
        }
        "release" => {
            let path = Path::new(text("path")?);
            if let Some(index) = guards.iter().position(|g| g.path() == path) {
                guards.remove(index);
            } else {
                sl::release(path);
            }
            Value::Null
        }
        "read_lease" => json!(sl::read(
            Path::new(text("path")?),
            request["pid"]
                .as_u64()
                .ok_or_else(|| anyhow!("missing pid"))?
        )),
        "live_leases" => {
            let ids: Option<Vec<u64>> = request
                .get("alive")
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()?;
            json!(sl::live_leases(root, |pid| ids
                .as_ref()
                .map(|ids| ids.contains(&pid))
                .unwrap_or_else(|| sl::pid_alive(pid))))
        }
        "pid_alive" => json!(sl::pid_alive(
            request["pid"]
                .as_u64()
                .ok_or_else(|| anyhow!("missing pid"))?
        )),
        op => bail!("unknown op: {op}"),
    })
}

fn main() -> Result<()> {
    let mut guards = Vec::new();
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let requests: Vec<Value> = serde_json::from_str(&line?)?;
        let responses: Vec<_> = requests
            .iter()
            .map(|request| match handle(request, &mut guards) {
                Ok(value) => json!({"ok": value}),
                Err(error) => json!({"error": error.to_string()}),
            })
            .collect();
        serde_json::to_writer(&mut out, &responses)?;
        writeln!(out)?;
        out.flush()?;
    }
    Ok(())
}
