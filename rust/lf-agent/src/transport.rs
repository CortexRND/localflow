use anyhow::{ensure, Context, Result};
use lf_core::config::Config;
use reqwest::{blocking::Client, redirect::Policy, Url};
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
        let response = request.send().context("Couldn't reach local Ollama")?;
        ensure!(
            response.status().as_u16() == 200,
            "Local Ollama request failed"
        );
        if let Some(size) = response.content_length() {
            ensure!(
                size <= MAX_RESPONSE_BYTES as u64,
                "Ollama response too large"
            );
        }
        let mut body = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .context("Couldn't read Ollama response")?;
        ensure!(
            body.len() <= MAX_RESPONSE_BYTES,
            "Ollama response too large"
        );
        serde_json::from_slice(&body).context("Ollama sent invalid JSON")
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
    let Ok(url) = Url::parse(s.trim()) else {
        return false;
    };
    if !["http", "https"].contains(&url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !["", "/"].contains(&url.path())
    {
        return false;
    }
    // URL parsing normalizes legacy integer/octal IPv4 forms; Python refuses them.
    let authority = s
        .trim()
        .split_once("://")
        .map(|(_, s)| s.split('/').next().unwrap_or(""))
        .unwrap_or("");
    let host = if authority.starts_with('[') {
        authority
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[')
    } else {
        authority.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}
pub fn local_ollama_url(config: &Config) -> String {
    let url = config.field_str("llm_base_url").unwrap_or("");
    if config.field_str("llm_provider") == Some("ollama") && is_loopback_url(url) {
        url.trim().trim_end_matches('/').into()
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
    let s = s.trim().to_lowercase();
    let s = s
        .strip_prefix("registry.ollama.ai/library/")
        .or_else(|| s.strip_prefix("registry.ollama.ai/"))
        .unwrap_or(&s);
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
