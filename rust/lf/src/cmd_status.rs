//! `status`: what the Python `lf status` prints, asked from the running
//! LocalFlow server when it is up.
//!
//! The API token is only *read* (`lf_core::apitoken::read`); this command
//! never creates, rewrites or rotates it. With no server the local config is
//! reported and `meeting.server` says `down`.

use std::time::Duration;

use anyhow::Result;
use lf_core::{apitoken, config};
use serde_json::{json, Map, Value};

use crate::pyfmt::{self, py_space};
use crate::{outln, Io};

/// Header the Python server checks (localflow/server.py).
const TOKEN_HEADER: &str = "X-Localflow-Token";

/// `server_url.client_host`: the address a local client dials for a server
/// bound to `host` (wildcards become loopback, brackets are stripped).
pub(crate) fn client_host(host: &Value) -> String {
    let mut host = host
        .as_str()
        .unwrap_or("")
        .trim_matches(py_space)
        .to_string();
    if host.starts_with('[') && host.ends_with(']') && host.len() >= 2 {
        host = host[1..host.len() - 1].trim_matches(py_space).to_string();
    }
    match host.as_str() {
        "" | "0.0.0.0" | "*" => "127.0.0.1".into(),
        "::" | "::0" => "::1".into(),
        _ => host,
    }
}

/// `server_url.base_url`.
pub(crate) fn base_url(host: &Value, port: &Value) -> String {
    let mut host = client_host(host);
    if host.contains(':') {
        host = format!("[{}]", host.replace('%', "%25"));
    }
    format!("http://{host}:{}", pyfmt::value_str(port))
}

/// Python `sys.platform`-style name the status reports.
fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "win32"
    } else {
        "linux"
    }
}

fn field(config: &config::Config, name: &str) -> Value {
    config.fields.get(name).cloned().unwrap_or(Value::Null)
}

/// `api.status(config, platform, loaded=False)` from the local config. The
/// autostart state needs the platform layer, which is not part of the Rust
/// port; it is reported as unknown rather than guessed.
pub(crate) fn local_status(config: &config::Config) -> Value {
    json!({
        "platform": platform_name(),
        "config_path": config::try_config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        "stt": {
            "provider": field(config, "stt_backend"),
            "model": field(config, "model_size"),
            "device": field(config, "stt_device"),
            "language": field(config, "language"),
            "loaded": false,
        },
        "llm": {
            "provider": field(config, "llm_provider"),
            "base_url": field(config, "llm_base_url"),
            "model": field(config, "llm_model"),
            "cleanup_enabled": field(config, "cleanup_enabled"),
        },
        "hotkey": field(config, "hotkey"),
        "paste_method": field(config, "paste_method"),
        "sounds_enabled": field(config, "sounds_enabled"),
        "server": {"host": field(config, "server_host"), "port": field(config, "server_port")},
        "meetings_enabled": field(config, "meetings_enabled"),
        "autostart": "unknown (server not running)",
    })
}

/// Outcome of one GET against the server.
pub(crate) enum Fetch {
    Ok(Value),
    Down,
    Unauthorized,
    Failed(String),
}

fn fetch(client: &reqwest::blocking::Client, url: &str, token: Option<&str>) -> Fetch {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.header(TOKEN_HEADER, token);
    }
    match request.send() {
        Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
            Fetch::Unauthorized
        }
        Ok(response) if !response.status().is_success() => {
            Fetch::Failed(format!("HTTP {}", response.status().as_u16()))
        }
        Ok(response) => match response.json::<Value>() {
            Ok(value) => Fetch::Ok(value),
            Err(error) => Fetch::Failed(format!("invalid response: {error}")),
        },
        Err(error) if error.is_connect() || error.is_timeout() => Fetch::Down,
        Err(error) => Fetch::Failed(error.to_string()),
    }
}

/// Status object: the server's own `/api/status` when reachable (it knows
/// whether the model is loaded and the autostart state), the local config
/// otherwise, plus `meeting` from `/meeting/status`.
pub(crate) fn collect(config: &config::Config, base: &str, token: Option<&str>) -> Value {
    let client = match reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            let mut result = local_status(config);
            result["meeting"] = json!({"server": "down", "error": error.to_string()});
            return result;
        }
    };
    let mut result = match fetch(&client, &format!("{base}/api/status"), token) {
        Fetch::Ok(Value::Object(map)) => Value::Object(map),
        _ => local_status(config),
    };
    let meeting = match fetch(&client, &format!("{base}/meeting/status"), token) {
        Fetch::Ok(value) => value,
        Fetch::Down => json!({"server": "down"}),
        Fetch::Unauthorized => json!({
            "server": "unauthorized",
            "detail": if token.is_some() {
                "the API token was rejected"
            } else {
                "no API token yet; start the server once"
            },
        }),
        Fetch::Failed(detail) => json!({"server": "error", "detail": detail}),
    };
    result["meeting"] = meeting;
    result
}

/// `cli._flatten_status`: dotted keys for nested objects, depth first.
pub(crate) fn flatten(value: &Value, prefix: &str, out: &mut Vec<(String, Value)>) {
    if let Value::Object(map) = value {
        flatten_map(map, prefix, out);
    }
}

fn flatten_map(map: &Map<String, Value>, prefix: &str, out: &mut Vec<(String, Value)>) {
    for (key, item) in map {
        let name = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match item {
            Value::Object(inner) => flatten_map(inner, &name, out),
            other => out.push((name, other.clone())),
        }
    }
}

pub(crate) fn run(as_json: bool, io: &mut Io<'_>) -> Result<()> {
    let loaded = config::load_config()?;
    let base = match &io.server_url {
        Some(url) => url.clone(),
        None => base_url(
            &field(&loaded, "server_host"),
            &field(&loaded, "server_port"),
        ),
    };
    // Read-only: never `load_or_create`, which would rotate the server's token.
    let token = apitoken::read()?;
    let result = collect(&loaded, &base, token.as_deref());
    if as_json {
        return outln!(io.out, "{}", pyfmt::json_dumps(&result));
    }
    if result["meeting"]["server"] == json!("down") {
        outln!(
            io.err,
            "server not running — open LocalFlow.app (it starts the server) or run: lf serve"
        )?;
    }
    let mut lines = Vec::new();
    flatten(&result, "", &mut lines);
    for (key, value) in lines {
        outln!(io.out, "{key}: {}", pyfmt::value_str(&value))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_matches_server_url_module() {
        assert_eq!(
            base_url(&json!("127.0.0.1"), &json!(8756)),
            "http://127.0.0.1:8756"
        );
        assert_eq!(base_url(&json!("0.0.0.0"), &json!(1)), "http://127.0.0.1:1");
        assert_eq!(base_url(&json!(""), &json!(1)), "http://127.0.0.1:1");
        assert_eq!(base_url(&json!(" * "), &json!(1)), "http://127.0.0.1:1");
        assert_eq!(base_url(&json!("::"), &json!(1)), "http://[::1]:1");
        assert_eq!(base_url(&json!("[::1]"), &json!(1)), "http://[::1]:1");
        assert_eq!(
            base_url(&json!("fe80::1%en0"), &json!(2)),
            "http://[fe80::1%25en0]:2"
        );
        assert_eq!(base_url(&json!(5), &json!(3)), "http://127.0.0.1:3");
        assert_eq!(
            base_url(&json!("host.local"), &json!(9)),
            "http://host.local:9"
        );
    }

    #[test]
    fn flatten_uses_dotted_keys_in_order() {
        let mut lines = Vec::new();
        flatten(
            &json!({"b": 1, "a": {"y": null, "x": {"z": true}}, "c": "s"}),
            "",
            &mut lines,
        );
        let keys: Vec<&str> = lines.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["b", "a.y", "a.x.z", "c"]);
    }
}
