//! The local LocalFlow service (Ask, Meetings, Prompts) or its sandbox fakes.
//!
//! Real mode talks to `http://{server_host}:{server_port}` with the API token
//! read from the explicit token path, through the existing `lf_ui` HTTP
//! adapters. Sandbox mode uses the existing in-memory fakes and never opens a
//! socket. Every call here is blocking: run it on the background executor.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_core::config::Config;
use lf_providers::factory::BuildOptions;
use lf_providers::{LlmProvider, SecretStore};
use lf_ui::backends::ask::{AskBackend, Citation, Turn, NO_ANSWER};
use lf_ui::backends::http::{
    configured_base_url, HttpAskBackend, HttpMeetingClient, HttpPromptQueue, HttpTransport,
};
use lf_ui::backends::meeting::{FakeMeetingClient, MeetingClient, MeetingClientError, SERVER_DOWN};
use lf_ui::backends::prompts::{FakePromptQueue, PromptQueue, QueueError, UnavailablePromptQueue};
use serde_json::Value;

use crate::controllers::agents::AgentsBackend;
use crate::controllers::agents_client::HttpAgents;
use crate::controllers::ask_agent::{AgentBackend, RECALL_NUM_CTX};
use crate::controllers::mcp_client::{self, McpError, ToolTransport};
use crate::controllers::people::People;
use crate::data::{open_history, DataRoot};
use crate::sandbox::agents::FakeAgents;

/// Whether the local service answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    /// Not checked yet.
    Unknown,
    /// Sandbox: answered by in-process fakes.
    Simulated,
    /// `GET /healthz` answered.
    Online,
    /// Nothing answered at the address.
    Offline,
}

impl Health {
    pub fn usable(&self) -> bool {
        matches!(self, Health::Simulated | Health::Online)
    }
}

#[derive(Clone)]
pub enum Remote {
    Http {
        token_path: PathBuf,
    },
    Sandbox {
        root: DataRoot,
        prompts: Arc<FakePromptQueue>,
        meetings: Arc<FakeMeetingClient>,
        agents: Arc<FakeAgents>,
    },
}

impl Remote {
    pub fn for_root(root: &DataRoot) -> Self {
        if root.is_sandbox() {
            Remote::Sandbox {
                root: root.clone(),
                prompts: Arc::new(crate::sandbox::seed_prompt_queue()),
                meetings: Arc::new(crate::sandbox::meeting_client(root)),
                // On only in a sandbox seeded for the `agents-*` scenes,
                // shared between its launcher and Hub windows through one
                // file there (a scene switches to the pinned in-memory
                // seed: `FakeAgents::use_fixture`); off in a plain one so
                // its pages and captures stay as they are.
                agents: Arc::new(match &root.sandbox_dir {
                    Some(dir) if crate::sandbox::has_agents_fixture(root) => {
                        FakeAgents::shared(dir.join(crate::sandbox::agents::STATE_FILE))
                    }
                    _ => FakeAgents::seeded(false),
                }),
            }
        } else {
            Remote::Http {
                token_path: root.token_path.clone(),
            }
        }
    }

    pub fn base_url(&self, config: &Config) -> String {
        match self {
            Remote::Http { .. } => configured_base_url(config),
            Remote::Sandbox { .. } => "sandbox (in-process, no network)".into(),
        }
    }

    fn transport(&self, config: &Config) -> anyhow::Result<HttpTransport> {
        match self {
            Remote::Http { token_path } => {
                let token_path = token_path.clone();
                HttpTransport::new(&configured_base_url(config), move || {
                    lf_core::apitoken::load_at(&token_path)
                })
            }
            Remote::Sandbox { .. } => anyhow::bail!("sandbox has no transport"),
        }
    }

    /// `GET /healthz` with a short timeout.
    pub fn health(&self, config: &Config) -> Health {
        match self {
            Remote::Sandbox { .. } => Health::Simulated,
            Remote::Http { .. } => {
                let url = format!("{}/healthz", configured_base_url(config));
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_millis(1500))
                    .redirect(reqwest::redirect::Policy::none())
                    .build();
                match client.and_then(|c| c.get(url).send()) {
                    Ok(response) if response.status().is_success() => Health::Online,
                    _ => Health::Offline,
                }
            }
        }
    }

    /// `GET /api/speakers` (People, read-only). Only call while
    /// [`crate::controllers::people::enabled`]: both servers refuse it
    /// otherwise. No other speaker-registry route is ever called from here
    /// (in particular not `/api/speakers/export`, the one with vectors).
    pub fn people(&self, config: &Config) -> People {
        match self {
            Remote::Sandbox { root, .. } => {
                if crate::controllers::people::enabled(config) {
                    People::List(crate::sandbox::people(root))
                } else {
                    People::Off
                }
            }
            Remote::Http { token_path } => {
                let url = format!("{}/api/speakers", configured_base_url(config));
                let Ok(token) = lf_core::apitoken::load_at(token_path) else {
                    return People::Offline;
                };
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .redirect(reqwest::redirect::Policy::none())
                    .build();
                let mut request = match client {
                    Ok(client) => client.get(url),
                    Err(_) => return People::Offline,
                };
                if !token.is_empty() {
                    request = request.header("X-Localflow-Token", token);
                }
                match request.send() {
                    Err(_) => People::Offline,
                    Ok(response) if response.status() == reqwest::StatusCode::FORBIDDEN => {
                        People::Off
                    }
                    Ok(response) if response.status().is_success() => {
                        match response.json::<Value>() {
                            Ok(reply) => People::List(crate::controllers::people::parse(&reply)),
                            Err(_) => {
                                People::Error("Invalid response from LocalFlow server".into())
                            }
                        }
                    }
                    Ok(response) => {
                        let status = response.status();
                        let detail = response
                            .json::<Value>()
                            .ok()
                            .and_then(|v| {
                                v.get("detail").and_then(Value::as_str).map(str::to_owned)
                            })
                            .filter(|d| !d.is_empty())
                            .unwrap_or_else(|| {
                                status.canonical_reason().unwrap_or("request failed").into()
                            });
                        People::Error(
                            detail
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ")
                                .chars()
                                .take(200)
                                .collect(),
                        )
                    }
                }
            }
        }
    }

    /// The agents service (`agents-api v1`): loopback HTTP, or the
    /// sandbox's in-process fake.
    pub fn agents(&self, config: &Config) -> Arc<dyn AgentsBackend> {
        match self {
            Remote::Sandbox { agents, .. } => agents.clone(),
            Remote::Http { token_path } => Arc::new(HttpAgents::new(config, token_path.clone())),
        }
    }

    /// The sandbox's fake agents service (fixtures and tests drive it).
    pub fn sandbox_agents(&self) -> Option<Arc<FakeAgents>> {
        match self {
            Remote::Sandbox { agents, .. } => Some(agents.clone()),
            Remote::Http { .. } => None,
        }
    }

    pub fn prompts(&self, config: &Config) -> Arc<dyn PromptQueue> {
        match self {
            Remote::Sandbox { prompts, .. } => prompts.clone(),
            Remote::Http { .. } => match self.transport(config) {
                Ok(transport) => Arc::new(HttpPromptQueue(transport)),
                Err(_) => Arc::new(UnavailablePromptQueue),
            },
        }
    }

    pub fn meetings(&self, config: &Config) -> Arc<dyn MeetingClient> {
        match self {
            Remote::Sandbox { meetings, .. } => meetings.clone(),
            Remote::Http { .. } => match self.transport(config) {
                Ok(transport) => Arc::new(HttpMeetingClient(transport)),
                Err(_) => Arc::new(DownMeetingClient),
            },
        }
    }

    /// Ask's tool-using backend. Real mode: LocalFlow's MCP server, the
    /// configured model and `ask` as the fallback. Sandbox: in-process fakes
    /// (`latency` paces the fake tools for the UI; tests pass zero).
    pub fn agent(
        &self,
        config: Config,
        secrets: Arc<dyn SecretStore + Send + Sync>,
        latency: Duration,
    ) -> Box<dyn AgentBackend> {
        match self {
            Remote::Sandbox { root, prompts, .. } => Box::new(crate::sandbox::SandboxAgent::new(
                root.clone(),
                prompts.clone(),
                latency,
            )),
            Remote::Http { .. } => Box::new(LocalAgent {
                remote: self.clone(),
                config,
                secrets,
            }),
        }
    }

    /// Ask one question (stateless: the page keeps the conversation).
    pub fn ask(&self, config: &Config, question: &str, time_filter: &str) -> Turn {
        self.ask_within(config, question, time_filter, None)
    }

    /// [`Remote::ask`] whose `/api/recall` request is bounded by `budget`
    /// (whole seconds, rounded down; `None` keeps the default timeout).
    pub fn ask_within(
        &self,
        config: &Config,
        question: &str,
        time_filter: &str,
        budget: Option<Duration>,
    ) -> Turn {
        match self {
            Remote::Sandbox { root, .. } => sandbox_answer(root, question),
            Remote::Http { .. } => match self.transport(config) {
                Ok(transport) => {
                    // GPUI Ask renders sources-only turns (model unavailable).
                    let mut backend = HttpAskBackend::new(transport).with_sources_only_replies();
                    if let Some(budget) = budget {
                        backend = backend.with_recall_timeout(budget.as_secs());
                    }
                    backend.ask(question, time_filter, None::<&BTreeSet<String>>, None)
                }
                Err(_) => {
                    let mut turn = Turn::new(question);
                    turn.error = lf_ui::backends::ask::ERROR_UNREACHABLE.into();
                    turn
                }
            },
        }
    }
}

/// Real-mode Ask backend.
///
/// The model is built exactly like the service's recall model
/// (`build_llm(config, timeout=…, num_ctx=8192)` in `localflow/server.py`):
/// the same configured provider, endpoint and endpoint-bound key, never a
/// different one. Only the timeout differs: it is set per request (seconds,
/// the provider's connect and overall HTTP timeout) from what is left of the
/// run's deadline.
pub struct LocalAgent {
    remote: Remote,
    config: Config,
    secrets: Arc<dyn SecretStore + Send + Sync>,
}

impl AgentBackend for LocalAgent {
    fn simulated(&self) -> bool {
        false
    }

    fn connect(
        &self,
        cancel: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<Box<dyn ToolTransport>, McpError> {
        mcp_client::connect_until(&mcp_client::discover(), cancel, deadline)
            .map(|session| Box::new(session) as Box<dyn ToolTransport>)
    }

    fn model(&self, timeout_secs: i64) -> Result<Box<dyn LlmProvider>, String> {
        let options = BuildOptions {
            timeout: Some(timeout_secs.max(1)),
            num_ctx: Some(RECALL_NUM_CTX),
            temperature: None,
            with_api_key: true,
        };
        lf_providers::factory::build_llm(&self.config, &options, self.secrets.as_ref())
            .map_err(|e| e.message())
    }

    fn recall(&self, question: &str, filter: &str, budget: Duration) -> Turn {
        self.remote
            .ask_within(&self.config, question, filter, Some(budget))
    }
}

/// Meeting client used when no transport could be built.
struct DownMeetingClient;

impl MeetingClient for DownMeetingClient {
    fn status(&self) -> Result<Value, MeetingClientError> {
        Err(MeetingClientError(SERVER_DOWN.into()))
    }
    fn templates(&self) -> Result<Value, MeetingClientError> {
        Err(MeetingClientError(SERVER_DOWN.into()))
    }
    fn start(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: Option<bool>,
    ) -> Result<Value, MeetingClientError> {
        Err(MeetingClientError(SERVER_DOWN.into()))
    }
    fn save_notes(&self, _: &str) -> Result<Value, MeetingClientError> {
        Err(MeetingClientError(SERVER_DOWN.into()))
    }
    fn stop(&self) -> Result<Value, MeetingClientError> {
        Err(MeetingClientError(SERVER_DOWN.into()))
    }
    fn chat(&self, _: &str) -> Result<Value, MeetingClientError> {
        Err(MeetingClientError(SERVER_DOWN.into()))
    }
    fn dismiss(&self) -> Result<(), MeetingClientError> {
        Err(MeetingClientError(SERVER_DOWN.into()))
    }
}

/// Sandbox Ask: a keyword search over the sandbox history, clearly labelled
/// as such. No model runs and nothing leaves the process.
pub fn sandbox_answer(root: &DataRoot, question: &str) -> Turn {
    let question = question.trim();
    let mut turn = Turn::new(question);
    if question.is_empty() {
        turn.error = lf_ui::backends::ask::ERROR_EMPTY_QUESTION.into();
        return turn;
    }
    let words: Vec<String> = question
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 3)
        .map(str::to_lowercase)
        .collect();
    let Ok(store) = open_history(root) else {
        turn.error = lf_ui::backends::ask::ERROR_GENERIC.into();
        return turn;
    };
    let entries = store.recent(200, None, 0).unwrap_or_default();
    let hits: Vec<_> = entries
        .iter()
        .filter(|e| {
            let text = e.final_text.to_lowercase();
            words.iter().any(|w| text.contains(w))
        })
        .take(3)
        .collect();
    if hits.is_empty() {
        turn.answer = NO_ANSWER.into();
        return turn;
    }
    turn.answer = format!(
        "Sandbox answer (keyword match, no model): {} example dictation{} mention this. \
         With the LocalFlow service running, Ask summarises matches with your cleanup model.",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" }
    );
    turn.citations = hits
        .iter()
        .enumerate()
        .map(|(i, e)| Citation {
            n: i as i64 + 1,
            source: "dictation".into(),
            title: e.app_name.clone().unwrap_or_default(),
            app: e.app_name.clone().unwrap_or_default(),
            when: lf_ui::hub_model::history_when(e.ts)
                .format("%Y-%m-%dT%H:%M")
                .to_string(),
            snippet: e.final_text.chars().take(140).collect(),
            reference: e.id.to_string(),
        })
        .collect();
    turn
}

/// Human message for a queue error.
pub fn queue_error_text(error: &QueueError) -> String {
    let text = error.to_string();
    if text.contains(SERVER_DOWN) || text.is_empty() {
        SERVER_DOWN.to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::{meetings, prompts};
    use serde_json::json;

    /// A loopback port with nothing listening.
    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn real_mode_reports_a_stopped_service_honestly() {
        assert_send_sync::<Remote>();
        let dir = tempfile::tempdir().unwrap();
        let remote = Remote::Http {
            token_path: dir.path().join("api-token"),
        };
        let mut config = Config::defaults_for(false);
        config.set_field("server_port", json!(closed_port()));
        assert_eq!(remote.health(&config), Health::Offline);
        assert!(remote.base_url(&config).starts_with("http://127.0.0.1:"));
        let queue = remote.prompts(&config);
        assert!(
            prompts::load(queue.as_ref()).is_err(),
            "never an empty queue"
        );
        let client = remote.meetings(&config);
        assert!(matches!(
            meetings::poll(client.as_ref(), true).phase,
            meetings::Phase::Offline(_)
        ));
        let turn = remote.ask(&config, "what did I say?", "Any time");
        assert!(!turn.error.is_empty() && turn.answer.is_empty());
        assert_eq!(remote.people(&config), People::Offline);
    }

    /// A real server's People reply: 403 is "off", a list is allow-listed.
    #[test]
    fn people_reads_the_list_and_a_403_honestly() {
        use std::io::{Read, Write};
        let serve = |status: &'static str, body: &'static str| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
                request
            });
            (port, handle)
        };
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("api-token");
        std::fs::write(&token, "secret-token").unwrap();
        let remote = Remote::Http { token_path: token };
        let mut config = Config::defaults_for(false);

        let (port, handle) = serve(
            "403 Forbidden",
            r#"{"detail":"speaker recognition is off"}"#,
        );
        config.set_field("server_port", json!(port));
        assert_eq!(remote.people(&config), People::Off);
        let request = handle.join().unwrap();
        assert!(
            request.starts_with("GET /api/speakers HTTP/1.1"),
            "{request}"
        );
        assert!(request
            .to_lowercase()
            .contains("x-localflow-token: secret-token"));

        let (port, handle) = serve(
            "200 OK",
            r#"{"speakers":[{"id":"vp1","name":"Priya","created":"2026-11-20T09:00:00Z","samples":2,"model_id":"m"}],"embedder":"m"}"#,
        );
        config.set_field("server_port", json!(port));
        let People::List(people) = remote.people(&config) else {
            panic!("expected a list");
        };
        handle.join().unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].name, "Priya");
        assert_eq!(
            people[0].voice,
            crate::controllers::people::VoiceModel::Current
        );
    }

    #[test]
    fn sandbox_remote_answers_from_example_history_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::sandbox::prepare(&dir.path().join("sb"), false).unwrap();
        let remote = Remote::for_root(&root);
        let config = Config::defaults_for(false);
        assert_eq!(remote.health(&config), Health::Simulated);
        let turn = remote.ask(&config, "Kubernetes rollout", "Any time");
        assert!(turn.answer.starts_with("Sandbox answer"));
        assert_eq!(turn.citations[0].source, "dictation");
        assert!(remote
            .ask(&config, "zebra quantum", "Any time")
            .citations
            .is_empty());
        // Default sandbox: both speaker flags off, so People is "off".
        assert_eq!(remote.people(&config), People::Off);
    }
}
