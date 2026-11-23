//! Authenticated clients for the local HTTP host. Tokens are read afresh for
//! each request; neither credentials nor request/response bodies are logged.
//! Recall is stateless: the current server contract accepts question/since/k/llm,
//! not conversation context, a fixed clock, or source selection. Restricted
//! source requests are refused rather than silently searching extra sources.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use lf_core::{apitoken, config::Config};
use reqwest::{blocking::Client, Method, StatusCode, Url};
use serde_json::{json, Map, Value};

use super::ask::{
    time_filter_since, AskBackend, Citation, Turn, ERROR_EMPTY_QUESTION, ERROR_GENERIC,
    ERROR_NO_SOURCES, ERROR_UNREACHABLE, NO_ANSWER, SOURCES,
};
use super::meeting::{MeetingClient, MeetingClientError, MeetingResult, SERVER_DOWN};
use super::prompts::{PromptEntry, PromptQueue, PromptTransitionError, QueueError, WorkPrompt};
use crate::pystr::PyStr;

pub const FILTERS_UNAVAILABLE: &str =
    "Source filtering isn't available through the local recall server.";

pub fn configured_base_url(config: &Config) -> String {
    let host = config
        .field_str("server_host")
        .unwrap_or("127.0.0.1")
        .trim();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .trim();
    let host = match host {
        "" | "*" | "0.0.0.0" => "127.0.0.1",
        "::" | "::0" => "::1",
        host => host,
    };
    let host = if host.contains(':') {
        format!("[{}]", host.replace('%', "%25"))
    } else {
        host.into()
    };
    let port = config
        .field("server_port")
        .and_then(Value::as_u64)
        .unwrap_or(8756);
    format!("http://{host}:{port}")
}

#[derive(Clone)]
pub struct HttpTransport {
    client: Client,
    base: Url,
    token: Arc<dyn Fn() -> anyhow::Result<String> + Send + Sync>,
}

struct RequestError {
    status: Option<StatusCode>,
    message: String,
}

impl HttpTransport {
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        Self::new(&configured_base_url(config), apitoken::load)
    }

    /// Injectable read-only token source for isolated hosts and tests.
    pub fn new(
        base: &str,
        token: impl Fn() -> anyhow::Result<String> + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        let base = Url::parse(base).map_err(|_| anyhow::anyhow!("invalid local server URL"))?;
        anyhow::ensure!(
            matches!(base.scheme(), "http" | "https")
                && base.host_str().is_some()
                && base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none(),
            "invalid local server URL"
        );
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base,
            token: Arc::new(token),
        })
    }

    fn request(
        &self,
        method: Method,
        path: &[&str],
        query: &[(&str, &str)],
        body: Option<Value>,
        timeout: u64,
    ) -> Result<Value, RequestError> {
        let unavailable = || RequestError {
            status: None,
            message: SERVER_DOWN.into(),
        };
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| unavailable())?
            .clear()
            .extend(path);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        let token = (self.token)().map_err(|_| unavailable())?;
        let mut request = self
            .client
            .request(method, url)
            .timeout(Duration::from_secs(timeout));
        if !token.is_empty() {
            request = request.header("X-Localflow-Token", token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().map_err(|_| unavailable())?;
        let status = response.status();
        if !status.is_success() {
            let detail = response
                .json::<Value>()
                .ok()
                .and_then(|v| v.get("detail").filter(|v| !v.is_null()).cloned())
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                })
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("request failed").into());
            return Err(RequestError {
                status: Some(status),
                message: detail,
            });
        }
        if status == StatusCode::NO_CONTENT {
            return Ok(json!({}));
        }
        response.json().map_err(|_| RequestError {
            status: Some(status),
            message: "Invalid response from LocalFlow server".into(),
        })
    }
}

pub struct HttpMeetingClient(pub HttpTransport);

impl HttpMeetingClient {
    /// Short, read-only fetch for the runtime's adaptive menu bar poller.
    pub fn status_fetch(&self) -> Arc<dyn Fn() -> Result<Value, String> + Send + Sync> {
        let transport = self.0.clone();
        Arc::new(move || {
            transport
                .request(
                    Method::GET,
                    &["meeting", "status"],
                    &[],
                    None,
                    lf_runtime::menubar::REQUEST_TIMEOUT.as_secs(),
                )
                .map_err(|error| error.message)
        })
    }

    fn request(
        &self,
        method: Method,
        route: &str,
        body: Option<Value>,
        timeout: u64,
    ) -> MeetingResult<Value> {
        self.0
            .request(method, &["meeting", route], &[], body, timeout)
            .map_err(|e| MeetingClientError(e.message))
    }
}

impl MeetingClient for HttpMeetingClient {
    fn status(&self) -> MeetingResult<Value> {
        self.request(Method::GET, "status", None, 3)
    }
    fn templates(&self) -> MeetingResult<Value> {
        self.request(Method::GET, "templates", None, 3)
    }
    fn start(
        &self,
        title: &str,
        category: &str,
        template: &str,
        user_notes: &str,
        auto_title: Option<bool>,
    ) -> MeetingResult<Value> {
        let mut body = json!({"title": title, "category": category, "template": template, "user_notes": user_notes});
        if let Some(value) = auto_title {
            body["auto_title"] = Value::Bool(value);
        }
        self.request(Method::POST, "start", Some(body), 10)
    }
    fn save_notes(&self, notes: &str) -> MeetingResult<Value> {
        self.request(Method::POST, "notes", Some(json!({"notes": notes})), 3)
    }
    fn stop(&self) -> MeetingResult<Value> {
        self.request(Method::POST, "stop", None, 600)
    }
    fn chat(&self, question: &str) -> MeetingResult<Value> {
        self.request(
            Method::POST,
            "chat",
            Some(json!({"question": question})),
            300,
        )
    }
    fn dismiss(&self) -> MeetingResult<()> {
        self.request(Method::POST, "dismiss", None, 3).map(|_| ())
    }
}

pub struct HttpPromptQueue(pub HttpTransport);

impl PromptQueue for HttpPromptQueue {
    fn add(&self, _: &[WorkPrompt], _: &[String], _: &str) -> Result<Vec<PromptEntry>, QueueError> {
        Err(QueueError::Io(
            "The local server has no add-prompts route".into(),
        ))
    }
    fn list(&self, status: Option<&str>) -> Result<Vec<PromptEntry>, QueueError> {
        let query: Vec<_> = status.map(|v| ("status", v)).into_iter().collect();
        let response = self
            .0
            .request(Method::GET, &["prompts"], &query, None, 3)
            .map_err(|e| QueueError::Io(e.message))?;
        serde_json::from_value(response.get("entries").cloned().unwrap_or(Value::Null))
            .map_err(|_| QueueError::Io("Invalid prompt queue response".into()))
    }
    fn get(&self, entry_id: &str) -> Result<Option<PromptEntry>, QueueError> {
        Ok(self
            .list(None)?
            .into_iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(entry_id)))
    }
    fn set_status(
        &self,
        entry_id: &str,
        status: &str,
        expect: Option<&[&str]>,
        fields: &Map<String, Value>,
    ) -> Result<Option<PromptEntry>, QueueError> {
        let (verb, allowed) = match status {
            "approved" => ("approve", super::prompts::APPROVE_FROM.as_slice()),
            "rejected" => ("reject", super::prompts::REJECT_FROM.as_slice()),
            _ => {
                return Err(QueueError::InvalidStatus(
                    "The local server only supports approve/reject".into(),
                ))
            }
        };
        if !fields.is_empty() || expect.is_some_and(|expected| expected != allowed) {
            return Err(QueueError::Io(
                "Unsupported remote prompt transition".into(),
            ));
        }
        if entry_id.is_empty() || [".", ".."].contains(&entry_id) || entry_id.contains('/') {
            return Ok(None);
        }
        match self
            .0
            .request(Method::POST, &["prompts", entry_id, verb], &[], None, 3)
        {
            Ok(value) => value
                .as_object()
                .cloned()
                .map(Some)
                .ok_or_else(|| QueueError::Io("Invalid prompt response".into())),
            Err(e) if e.status == Some(StatusCode::NOT_FOUND) => Ok(None),
            Err(e) if e.status == Some(StatusCode::CONFLICT) => {
                Err(QueueError::Remote(PromptTransitionError {
                    status_code: 409,
                    detail: e.message,
                }))
            }
            Err(e) => Err(QueueError::Io(e.message)),
        }
    }
}

pub struct HttpAskBackend {
    http: HttpTransport,
    turns: Vec<Turn>,
    /// Whole-request timeout of `/api/recall`, in seconds.
    recall_timeout: u64,
    /// Decode `"answer": null` with citations as a sources-only turn
    /// ([`Turn::sources_only`]). Off by default: callers that render the
    /// answer text without knowing that case keep the previous behaviour
    /// (a generic error).
    sources_only: bool,
}

/// Default `/api/recall` timeout in seconds.
pub const RECALL_TIMEOUT_SECS: u64 = 300;

impl HttpAskBackend {
    pub fn new(http: HttpTransport) -> Self {
        Self {
            http,
            turns: Vec::new(),
            recall_timeout: RECALL_TIMEOUT_SECS,
            sources_only: false,
        }
    }

    /// Opt in to sources-only turns: when the model couldn't answer
    /// (`"answer": null`), keep the matching citations instead of failing.
    /// Only for callers that render [`Turn::sources_only`] turns.
    pub fn with_sources_only_replies(mut self) -> Self {
        self.sources_only = true;
        self
    }

    /// A shorter `/api/recall` timeout (seconds, at least 1, at most the
    /// default), for a caller with its own deadline.
    pub fn with_recall_timeout(mut self, secs: u64) -> Self {
        self.recall_timeout = secs.clamp(1, RECALL_TIMEOUT_SECS);
        self
    }
}

impl AskBackend for HttpAskBackend {
    fn turns(&self) -> Vec<Turn> {
        self.turns.clone()
    }
    fn set_turns(&mut self, turns: Vec<Turn>) {
        self.turns = turns;
    }
    fn clear(&mut self) {
        self.turns.clear();
    }
    fn ask(
        &mut self,
        question: &str,
        time_filter: &str,
        sources: Option<&BTreeSet<String>>,
        _now: Option<f64>,
    ) -> Turn {
        let question = question.py_strip();
        let mut turn = Turn::new(question);
        if question.is_empty() {
            turn.error = ERROR_EMPTY_QUESTION.into();
            return turn;
        }
        if sources.is_some_and(BTreeSet::is_empty) {
            turn.error = ERROR_NO_SOURCES.into();
        } else if sources.is_some_and(|s| {
            s.len() != SOURCES.len() || SOURCES.iter().any(|(k, _)| !s.contains(*k))
        }) {
            // The HTTP recall contract has no source selector. Never broaden a
            // user's restricted search silently (including what reaches an LLM).
            turn.error = FILTERS_UNAVAILABLE.into();
        } else if let Some(since) = time_filter_since(time_filter) {
            match self.http.request(
                Method::POST,
                &["api", "recall"],
                &[],
                Some(json!({"question":question,"since":since,"k":8,"llm":true})),
                self.recall_timeout,
            ) {
                Ok(value) => {
                    // A `citations` list whose every item decodes; `None` when
                    // it is missing, not a list or has a malformed item.
                    let citations = || -> Option<Vec<Citation>> {
                        value
                            .get("citations")?
                            .as_array()?
                            .iter()
                            .cloned()
                            .map(serde_json::from_value)
                            .collect::<Result<Vec<Citation>, _>>()
                            .ok()
                    };
                    match value.get("answer") {
                        Some(Value::String(answer)) => {
                            let answer = answer.py_strip();
                            if answer.is_empty()
                                || answer.to_lowercase().contains(
                                    NO_ANSWER.trim_end_matches('.').to_lowercase().as_str(),
                                )
                            {
                                turn.answer = NO_ANSWER.into();
                            } else if let Some(citations) = citations() {
                                turn.answer = answer.into();
                                turn.citations = citations;
                            } else {
                                turn.error = ERROR_GENERIC.into();
                            }
                        }
                        // An explicit `null` (opted in): recall matched
                        // passages but the model couldn't answer (both the
                        // Rust and Python servers). Sources only, no prose:
                        // see `Turn::sources_only`. With no passages there is
                        // nothing to show, which is the no-answer reply.
                        // Without the opt-in, `null` stays a generic error.
                        Some(Value::Null) if self.sources_only => match citations() {
                            Some(citations) if citations.is_empty() => {
                                turn.answer = NO_ANSWER.into();
                            }
                            Some(citations) => turn.citations = citations,
                            None => turn.error = ERROR_GENERIC.into(),
                        },
                        _ => turn.error = ERROR_GENERIC.into(),
                    }
                }
                Err(_) => turn.error = ERROR_UNREACHABLE.into(),
            }
        } else {
            turn.error = ERROR_GENERIC.into();
        }
        self.turns.push(turn.clone());
        turn
    }
}

#[cfg(test)]
mod tests;
