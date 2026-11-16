use anyhow::{ensure, Context, Result};
use lf_core::config::Config;
use reqwest::{blocking::Client, redirect::Policy};
use serde_json::Value;
use std::{io::Read, net::IpAddr, time::Duration};

pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub trait ChatClient {
    fn tags(&self, endpoint: &str) -> Result<Value>;
    fn chat(&self, endpoint: &str, payload: &Value) -> Result<Value>;
}
pub struct OllamaClient {
    client: Client,
}
impl OllamaClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .no_proxy()
                .redirect(Policy::none())
                .build()?,
        })
    }
    fn request(&self, endpoint: &str, payload: Option<&Value>) -> Result<Value> {
        ensure!(is_loopback_url(endpoint), "Ollama must be loopback");
        let request = match payload {
            Some(p) => self
                .client
                .post(format!("{}/api/chat", endpoint.trim_end_matches('/')))
                .json(p)
                .timeout(Duration::from_secs(60)),
            None => self
                .client
                .get(format!("{}/api/tags", endpoint.trim_end_matches('/')))
                .timeout(Duration::from_secs(5)),
        };
        let model = payload.and_then(|p| p["model"].as_str()).unwrap_or("");
        let response = request.send().map_err(|e| {
            if payload.is_some() && e.is_timeout() {
                anyhow::anyhow!("The local model ({model}) took too long to answer. Try again.")
            } else {
                anyhow::anyhow!(
                    "Couldn't reach Ollama at {endpoint}. Start Ollama, then try again."
                )
            }
        })?;
        let status = response.status().as_u16();
        let what = if payload.is_some() {
            "chat"
        } else if status == 200 {
            "model list"
        } else {
            "error"
        };
        let mut body = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|e| {
                let timed_out = e.kind() == std::io::ErrorKind::TimedOut || e.get_ref().and_then(|e| e.downcast_ref::<reqwest::Error>()).is_some_and(reqwest::Error::is_timeout);
                if timed_out { anyhow::anyhow!("Ollama took too long to send its {what} response. Try again.") }
                else { anyhow::anyhow!("Lost the connection to Ollama while reading its {what} response. Check that Ollama is running, then try again.") }
            })?;
        ensure!(
            body.len() <= MAX_RESPONSE_BYTES,
            "Ollama sent an oversized {what} response."
        );
        let decoded: Option<Value> = serde_json::from_slice(&body).ok();
        if payload.is_none() {
            ensure!(
                status == 200,
                "Ollama at {endpoint} couldn't list its models."
            );
            return decoded
                .filter(|v| !v.is_null())
                .context("Ollama sent an invalid model list response.");
        }
        if status != 200 {
            let detail = decoded
                .as_ref()
                .and_then(|v| v.get("error"))
                .filter(|v| super::truthy(v))
                .map(super::python_str)
                .unwrap_or_else(|| format!("HTTP {status}"));
            let detail = super::safe_text(&detail, 200);
            ensure!(!detail.to_lowercase().contains("does not support tools"), "The local model {model} can't use tools. Install one that can (`ollama pull gemma4`) and try again.");
            anyhow::bail!("Ollama couldn't run {model}: {detail}");
        }
        decoded
            .filter(Value::is_object)
            .context("Ollama sent an invalid chat response.")
    }
}
impl ChatClient for OllamaClient {
    fn tags(&self, endpoint: &str) -> Result<Value> {
        self.request(endpoint, None)
    }
    fn chat(&self, endpoint: &str, payload: &Value) -> Result<Value> {
        self.request(endpoint, Some(payload))
    }
}
pub fn is_loopback_url(s: &str) -> bool {
    let s = s
        .trim_matches(super::validation::python_whitespace)
        .trim_start_matches(|c: char| c <= '\u{20}')
        .replace(['\n', '\r', '\t'], "");
    let Some((scheme, rest)) = s.split_once("://") else {
        return false;
    };
    if !["http", "https"].contains(&scheme.to_lowercase().as_str()) {
        return false;
    }
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    if !fragment.is_empty() || !query.is_empty() {
        return false;
    }
    let (authority, path) = rest
        .split_once('/')
        .map(|(a, p)| (a, format!("/{p}")))
        .unwrap_or((rest, String::new()));
    if !["", "/"].contains(&path.as_str()) {
        return false;
    }
    let authority = if let Some((creds, host)) = authority.rsplit_once('@') {
        if !["", ":"].contains(&creds) {
            return false;
        }
        host
    } else {
        authority
    };
    let (host, port) = if let Some(s) = authority.strip_prefix('[') {
        let Some((host, rest)) = s.split_once(']') else {
            return false;
        };
        if host
            .split('%')
            .next()
            .unwrap_or("")
            .parse::<std::net::Ipv6Addr>()
            .is_err()
        {
            return false;
        }
        (host, rest.split_once(':').map(|(_, p)| p).unwrap_or(""))
    } else {
        authority.split_once(':').unwrap_or((authority, ""))
    };
    if !port.is_empty()
        && (!port.bytes().all(|b| b.is_ascii_digit()) || port.parse::<u16>().is_err())
    {
        return false;
    }
    if let Some((_, scope)) = host.split_once('%') {
        if !host.contains(':') || scope.is_empty() || scope.contains('%') {
            return false;
        }
    }
    host.eq_ignore_ascii_case("localhost")
        || host
            .split('%')
            .next()
            .unwrap_or("")
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())))
}
pub fn local_ollama_url(config: &Config) -> String {
    let url = config.field_str("llm_base_url").unwrap_or("");
    if config.field_str("llm_provider") == Some("ollama") && is_loopback_url(url) {
        url.trim_matches(super::validation::python_whitespace)
            .trim_end_matches('/')
            .into()
    } else {
        DEFAULT_OLLAMA_URL.into()
    }
}
pub fn valid_model_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 120
        && s.bytes().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._:/@+-".contains(&c))
}
fn tag_key(s: &str) -> String {
    let s = s
        .trim_matches(super::validation::python_whitespace)
        .to_lowercase();
    let s = s.strip_prefix("registry.ollama.ai/library/").unwrap_or(&s);
    let s = s.strip_prefix("registry.ollama.ai/").unwrap_or(s);
    if s.contains(':') {
        s.into()
    } else {
        format!("{s}:latest")
    }
}
pub fn choose_model(installed: &[String], preferred: Option<&str>) -> Result<String> {
    ensure!(
        !installed.is_empty(),
        "Ollama has no models installed. Run `ollama pull gemma4`, then try again."
    );
    if let Some(wanted) = preferred {
        if let Some(model) = installed.iter().find(|m| tag_key(m) == tag_key(wanted)) {
            return Ok(model.clone());
        }
    }
    let mut gemma: Vec<_> = installed
        .iter()
        .filter(|m| tag_key(m).starts_with("gemma4"))
        .collect();
    gemma.sort();
    Ok(gemma.first().copied().unwrap_or(&installed[0]).clone())
}
