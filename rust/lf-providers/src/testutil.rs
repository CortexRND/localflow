//! Offline test doubles: a scripted HTTP/1.1 server on `std::net::TcpListener`
//! and in-memory secret/config stores with failure injection.

use crate::store::{ConfigStore, SecretStore};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct MockResponse {
    pub status: u16,
    pub body: String,
    pub delay_ms: u64,
}

impl MockResponse {
    pub fn json(status: u16, body: &str) -> Self {
        MockResponse {
            status,
            body: body.to_string(),
            delay_ms: 0,
        }
    }

    pub fn delayed(mut self, ms: u64) -> Self {
        self.delay_ms = ms;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("request body is JSON")
    }
}

pub struct MockServer {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

fn reason(status: u16) -> &'static str {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("Unknown")
}

fn handle(stream: TcpStream, response: &MockResponse, log: &Arc<Mutex<Vec<Recorded>>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((key, value)) = header.split_once(':') {
            let (key, value) = (key.trim().to_string(), value.trim().to_string());
            if key.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(0);
            }
            headers.push((key, value));
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    log.lock().unwrap().push(Recorded {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    if response.delay_ms > 0 {
        thread::sleep(Duration::from_millis(response.delay_ms));
    }
    let mut stream = stream;
    let reply = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        reason(response.status),
        response.body.len(),
        response.body
    );
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
}

impl MockServer {
    /// Serve `responses` in order, one connection each.
    pub fn start(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        thread::spawn(move || {
            for response in responses {
                match listener.accept() {
                    Ok((stream, _)) => handle(stream, &response, &log),
                    Err(_) => return,
                }
            }
        });
        MockServer { port, requests }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request(&self, index: usize) -> Recorded {
        self.requests()
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("no request #{index}"))
    }
}

/// Destination port zero is reserved; binding port zero allocates a different
/// port, so no parallel mock server can accidentally answer this endpoint.
pub const UNREACHABLE_URL: &str = "http://127.0.0.1:0";

type SetHook = Box<dyn Fn(&str, &str) -> anyhow::Result<()>>;
type DeleteHook = Box<dyn Fn(&str) -> anyhow::Result<()>>;

/// In-memory keyring. Missing names read as "", like `get_secret`.
#[derive(Default)]
pub struct MemSecrets {
    pub map: RefCell<BTreeMap<String, String>>,
    pub fail_set: RefCell<Option<SetHook>>,
    pub fail_delete: RefCell<Option<DeleteHook>>,
    pub gets: RefCell<Vec<String>>,
    pub writes: RefCell<Vec<String>>,
    /// Like a real keyring: deleting a missing item is an error.
    pub strict_delete: std::cell::Cell<bool>,
}

impl MemSecrets {
    pub fn with(entries: &[(&str, &str)]) -> Self {
        let store = MemSecrets::default();
        for (name, value) in entries {
            store
                .map
                .borrow_mut()
                .insert(name.to_string(), value.to_string());
        }
        store
    }

    pub fn snapshot(&self) -> BTreeMap<String, String> {
        self.map.borrow().clone()
    }

    pub fn on_set(&self, hook: impl Fn(&str, &str) -> anyhow::Result<()> + 'static) {
        *self.fail_set.borrow_mut() = Some(Box::new(hook));
    }

    pub fn on_delete(&self, hook: impl Fn(&str) -> anyhow::Result<()> + 'static) {
        *self.fail_delete.borrow_mut() = Some(Box::new(hook));
    }
}

impl SecretStore for MemSecrets {
    fn get(&self, name: &str) -> String {
        self.gets.borrow_mut().push(name.to_string());
        self.map.borrow().get(name).cloned().unwrap_or_default()
    }

    fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        if let Some(hook) = self.fail_set.borrow().as_ref() {
            hook(name, value)?;
        }
        self.writes.borrow_mut().push(format!("set {name}"));
        self.map
            .borrow_mut()
            .insert(name.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, name: &str) -> anyhow::Result<()> {
        if let Some(hook) = self.fail_delete.borrow().as_ref() {
            hook(name)?;
        }
        self.writes.borrow_mut().push(format!("delete {name}"));
        if self.map.borrow_mut().remove(name).is_none() && self.strict_delete.get() {
            anyhow::bail!("not found");
        }
        Ok(())
    }
}

pub type TestConfig = BTreeMap<String, serde_json::Value>;

/// Python `Config()` defaults for the fields this crate reads.
pub fn default_config() -> TestConfig {
    let mut config = TestConfig::new();
    for (key, value) in [
        ("llm_provider", serde_json::json!("ollama")),
        ("llm_base_url", serde_json::json!("http://localhost:11434")),
        ("llm_model", serde_json::json!("llama3.2:3b")),
        ("llm_key_ref", serde_json::json!("")),
        ("cleanup_timeout", serde_json::json!(120)),
        ("cleanup_num_ctx", serde_json::json!(8192)),
        (
            "fireworks_model",
            serde_json::json!("accounts/fireworks/models/kimi-k3"),
        ),
        ("setup_version", serde_json::json!(1)),
    ] {
        config.insert(key.to_string(), value);
    }
    config
}

pub fn config_with(pairs: &[(&str, &str)]) -> TestConfig {
    let mut config = default_config();
    for (key, value) in pairs {
        config.insert(key.to_string(), serde_json::json!(value));
    }
    config
}

/// In-memory config file; `fail_save` simulates "disk full".
pub struct MemConfigs {
    pub saved: RefCell<TestConfig>,
    pub fail_save: RefCell<bool>,
    pub saves: RefCell<usize>,
}

impl MemConfigs {
    pub fn new(config: TestConfig) -> Self {
        MemConfigs {
            saved: RefCell::new(config),
            fail_save: RefCell::new(false),
            saves: RefCell::new(0),
        }
    }

    pub fn current(&self) -> TestConfig {
        self.saved.borrow().clone()
    }
}

impl ConfigStore<TestConfig> for MemConfigs {
    fn load(&self) -> anyhow::Result<TestConfig> {
        Ok(self.current())
    }

    fn save(&self, config: &TestConfig) -> anyhow::Result<()> {
        if *self.fail_save.borrow() {
            anyhow::bail!("disk full");
        }
        *self.saves.borrow_mut() += 1;
        *self.saved.borrow_mut() = config.clone();
        Ok(())
    }
}
