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
use std::time::Duration;

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

use crate::controllers::ask_agent::{AgentBackend, RECALL_NUM_CTX};
use crate::controllers::mcp_client::{self, McpError, ToolTransport};
use crate::data::{open_history, DataRoot};

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
    },
}

impl Remote {
    pub fn for_root(root: &DataRoot) -> Self {
        if root.is_sandbox() {
            Remote::Sandbox {
                root: root.clone(),
                prompts: Arc::new(crate::sandbox::seed_prompt_queue()),
                meetings: Arc::new(crate::sandbox::meeting_client(root)),
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
        match self {
            Remote::Sandbox { root, .. } => sandbox_answer(root, question),
            Remote::Http { .. } => match self.transport(config) {
                Ok(transport) => {
                    let mut backend = HttpAskBackend::new(transport);
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

    fn connect(&self, cancel: &AtomicBool) -> Result<Box<dyn ToolTransport>, McpError> {
        mcp_client::connect(&mcp_client::discover(), cancel)
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

    fn recall(&self, question: &str, filter: &str) -> Turn {
        self.remote.ask(&self.config, question, filter)
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
    }
}
