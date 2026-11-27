//! `agents-api v1` over loopback HTTP: the routes the Agents page, the
//! launcher and Settings → Agents use (status, providers, sessions,
//! changes, events, launch/cancel/delete/reply, provider checks and keys),
//! plus `PUT /api/config` for the `[agents]` settings.
//!
//! A key's value goes into exactly one request body (`PUT …/secret`) and
//! is never copied into an error: failures carry only the status, the
//! envelope's code and its sanitised `detail`.
//!
//! Same conventions as the rest of the window: the configured server
//! address, the API token read from the explicit token path and sent only
//! in the `X-Localflow-Token` header (never in a URL), no redirects, and a
//! bounded timeout. Every call blocks: run it on the background executor.
//! Errors are [`AgentsError`] values with safe human text only: a server
//! `detail` is sanitised and masked, and nothing here prints or logs.
//!
//! One HTTP client serves every request (its connection pool is reused by
//! the 5 s status probe and the page's reads); each request sets its own
//! timeout.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use lf_core::config::Config;
use lf_ui::backends::http::configured_base_url;
use reqwest::{StatusCode, Url};
use serde_json::Value;

use crate::controllers::agents::{
    self, AgentsBackend, AgentsError, Changes, Event, EventsPage, LaunchRequest, Provider, Session,
    SessionPage, Status,
};
use crate::controllers::agents_settings::{self, AgentSettings, SecretValue};

/// Ordinary reads.
const TIMEOUT: Duration = Duration::from_secs(5);
/// The longest long-poll the contract allows.
pub const MAX_WAIT: u32 = 25;

/// The one client every [`HttpAgents`] shares: no redirects, a connect
/// bound; read timeouts are per request. `None` if it couldn't be built.
fn client() -> Option<&'static reqwest::blocking::Client> {
    static CLIENT: OnceLock<Option<reqwest::blocking::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .connect_timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()
        })
        .as_ref()
}

pub struct HttpAgents {
    base: String,
    token_path: PathBuf,
}

impl HttpAgents {
    pub fn new(config: &Config, token_path: PathBuf) -> Self {
        HttpAgents {
            base: configured_base_url(config),
            token_path,
        }
    }

    fn get(
        &self,
        path: &[&str],
        query: &[(&str, String)],
        timeout: Duration,
    ) -> Result<Value, (Option<StatusCode>, AgentsError)> {
        self.send(
            Root::Agents,
            reqwest::Method::GET,
            path,
            query,
            None,
            timeout,
        )
    }

    /// One request. Reads fail as [`AgentsError::Refused`]; writes (any
    /// other method) as [`AgentsError::Rejected`] with the envelope's code
    /// (`/api/config` has none: see [`config_code`]). A request body is
    /// never copied into an error.
    fn send(
        &self,
        root: Root,
        method: reqwest::Method,
        path: &[&str],
        query: &[(&str, String)],
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, (Option<StatusCode>, AgentsError)> {
        let offline = || (None, AgentsError::Offline);
        let write = method != reqwest::Method::GET;
        let mut url = Url::parse(&self.base).map_err(|_| offline())?;
        url.path_segments_mut()
            .map_err(|_| offline())?
            .clear()
            .extend(root.segments())
            .extend(path);
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let token = lf_core::apitoken::load_at(&self.token_path).map_err(|_| offline())?;
        let mut request = client()
            .ok_or_else(offline)?
            .request(method, url)
            .timeout(timeout);
        if !token.is_empty() {
            request = request.header("X-Localflow-Token", token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().map_err(|_| offline())?;
        let status = response.status();
        if status.is_success() {
            return response.json::<Value>().map_err(|_| {
                (
                    Some(status),
                    AgentsError::Refused("Invalid response from LocalFlow server".into()),
                )
            });
        }
        const TOKEN: &str =
            "the service didn't accept LocalFlow's API token. Restart LocalFlow and try again.";
        let envelope = response.json::<Value>().ok();
        let field = |name: &str| {
            envelope
                .as_ref()
                .and_then(|v| v.get(name))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let detail = if status == StatusCode::UNAUTHORIZED {
            TOKEN.to_string()
        } else {
            field("detail").unwrap_or_else(|| match (root, status) {
                // FastAPI's own 422 carries a list, not a sentence.
                (Root::Config, StatusCode::UNPROCESSABLE_ENTITY) => CONFIG_REFUSED.into(),
                _ => status
                    .canonical_reason()
                    .unwrap_or("request failed")
                    .to_lowercase(),
            })
        };
        let code = match root {
            Root::Agents => field("code").unwrap_or_else(|| "internal".into()),
            Root::Config => config_code(status).into(),
        };
        let error = if write {
            AgentsError::rejected(status.as_u16(), &code, &detail)
        } else if status == StatusCode::UNAUTHORIZED {
            AgentsError::Refused(TOKEN.into())
        } else {
            AgentsError::Refused(agents::safe_detail(&detail))
        };
        Err((Some(status), error))
    }

    fn read(&self, path: &[&str], query: &[(&str, String)]) -> Result<Value, AgentsError> {
        self.get(path, query, TIMEOUT).map_err(|(_, e)| e)
    }

    fn write(
        &self,
        method: reqwest::Method,
        path: &[&str],
        body: Option<&Value>,
    ) -> Result<Value, AgentsError> {
        self.send(Root::Agents, method, path, &[], body, TIMEOUT)
            .map_err(|(_, e)| e)
    }
}

/// Which API a request goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Root {
    /// `/api/agents/…` (v1 envelopes: `{detail, code}`).
    Agents,
    /// `/api/config` (FastAPI errors: `{detail}` only).
    Config,
}

impl Root {
    fn segments(self) -> &'static [&'static str] {
        match self {
            Root::Agents => &["api", "agents"],
            Root::Config => &["api", "config"],
        }
    }
}

/// `PUT /api/config` answers errors without a `code`: name it by status.
fn config_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "validation",
        StatusCode::FORBIDDEN => "local_only",
        StatusCode::UNAUTHORIZED => "auth_failed",
        _ => "internal",
    }
}

/// `/api/config`'s own 422 (a malformed body) in words.
const CONFIG_REFUSED: &str = "LocalFlow didn't accept that setting";

/// A deep provider check. The service bounds it at 20 s itself; the client
/// waits a little longer so that answer (not a client timeout) arrives.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(25);

fn invalid() -> AgentsError {
    AgentsError::Refused("Invalid response from LocalFlow server".into())
}

/// A `Provider` reply (a check or a key change).
fn provider_of(reply: &Value) -> Result<Provider, AgentsError> {
    agents::parse_provider(reply).ok_or_else(invalid)
}

/// `{session}` → the session.
fn session_of(reply: &Value) -> Result<Session, AgentsError> {
    reply
        .get("session")
        .and_then(agents::parse_session)
        .ok_or_else(invalid)
}

impl AgentsBackend for HttpAgents {
    fn simulated(&self) -> bool {
        false
    }

    /// A 404 here means the service has no agents API (hide the feature).
    fn status(&self) -> Result<Status, AgentsError> {
        match self.get(&["status"], &[], TIMEOUT) {
            Ok(reply) => Ok(agents::parse_status(&reply)),
            Err((Some(StatusCode::NOT_FOUND), _)) => Err(AgentsError::Absent),
            Err((_, error)) => Err(error),
        }
    }

    fn providers(&self) -> Result<Vec<Provider>, AgentsError> {
        self.read(&["providers"], &[])
            .map(|reply| agents::parse_providers(&reply))
    }

    fn sessions(&self, limit: u32, before: Option<&str>) -> Result<SessionPage, AgentsError> {
        let mut query = vec![("limit", limit.to_string())];
        if let Some(cursor) = before {
            query.push(("before", cursor.to_string()));
        }
        self.read(&["sessions"], &query)
            .map(|reply| agents::parse_session_page(&reply))
    }

    fn session(&self, id: &str) -> Result<Session, AgentsError> {
        session_of(&self.read(&["sessions", id], &[])?)
    }

    fn launch(&self, request: &LaunchRequest) -> Result<Session, AgentsError> {
        let body = request.body();
        session_of(&self.write(reqwest::Method::POST, &["sessions"], Some(&body))?)
    }

    fn cancel(&self, id: &str) -> Result<Session, AgentsError> {
        session_of(&self.write(reqwest::Method::POST, &["sessions", id, "cancel"], None)?)
    }

    fn remove(&self, id: &str) -> Result<(), AgentsError> {
        let reply = self.write(reqwest::Method::DELETE, &["sessions", id], None)?;
        match reply.get("deleted").and_then(Value::as_bool) {
            Some(true) => Ok(()),
            _ => Err(invalid()),
        }
    }

    fn message(&self, id: &str, text: &str) -> Result<(Session, Event), AgentsError> {
        let body = serde_json::json!({ "text": text });
        let reply = self.write(
            reqwest::Method::POST,
            &["sessions", id, "messages"],
            Some(&body),
        )?;
        let event = reply
            .get("event")
            .and_then(agents::parse_event)
            .ok_or_else(invalid)?;
        Ok((session_of(&reply)?, event))
    }

    fn events(
        &self,
        id: &str,
        after: u64,
        limit: u32,
        wait: u32,
    ) -> Result<EventsPage, AgentsError> {
        let wait = wait.min(MAX_WAIT);
        let query = [
            ("after", after.to_string()),
            ("limit", limit.clamp(1, 500).to_string()),
            ("wait", wait.to_string()),
        ];
        self.get(
            &["sessions", id, "events"],
            &query,
            TIMEOUT + Duration::from_secs(u64::from(wait)),
        )
        .map(|reply| agents::parse_events(&reply))
        .map_err(|(_, e)| e)
    }

    fn changes(&self, since: u64, wait: u32) -> Result<Changes, AgentsError> {
        let wait = wait.min(MAX_WAIT);
        let query = [("since", since.to_string()), ("wait", wait.to_string())];
        self.get(
            &["changes"],
            &query,
            TIMEOUT + Duration::from_secs(u64::from(wait)),
        )
        .map(|reply| agents::parse_changes(&reply))
        .map_err(|(_, e)| e)
    }

    /// The same route as [`HttpAgents::status`], read with paths.
    fn settings(&self) -> Result<AgentSettings, AgentsError> {
        match self.get(&["status"], &[], TIMEOUT) {
            Ok(reply) => Ok(agents_settings::parse_settings(&reply)),
            Err((Some(StatusCode::NOT_FOUND), _)) => Err(AgentsError::Absent),
            Err((_, error)) => Err(error),
        }
    }

    /// Only the status matters: the success body (the whole config) is
    /// not read.
    fn set_config(&self, patch: &Value) -> Result<(), AgentsError> {
        self.send(
            Root::Config,
            reqwest::Method::PUT,
            &[],
            &[],
            Some(patch),
            TIMEOUT,
        )
        .map(|_| ())
        .map_err(|(_, e)| e)
    }

    fn check_provider(&self, id: &str, profile: &str) -> Result<Provider, AgentsError> {
        let body = if profile.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::json!({ "profile": profile })
        };
        let reply = self
            .send(
                Root::Agents,
                reqwest::Method::POST,
                &["providers", id, "check"],
                &[],
                Some(&body),
                CHECK_TIMEOUT,
            )
            .map_err(|(_, e)| e)?;
        provider_of(&reply)
    }

    fn set_secret(
        &self,
        id: &str,
        profile: &str,
        name: &str,
        value: &SecretValue,
    ) -> Result<Provider, AgentsError> {
        let mut body = serde_json::json!({ "name": name, "value": value.expose() });
        if !profile.is_empty() {
            body["profile"] = Value::String(profile.to_string());
        }
        let result = self.write(
            reqwest::Method::PUT,
            &["providers", id, "secret"],
            Some(&body),
        );
        // Drop the body's copy of the value now, not at the end of scope.
        if let Some(Value::String(v)) = body.get_mut("value") {
            let mut bytes = std::mem::take(v).into_bytes();
            bytes.fill(0);
            std::hint::black_box(&bytes);
        }
        provider_of(&result?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};

    /// One canned reply on a loopback port; returns the raw request.
    fn serve(status: &'static str, body: String) -> (u16, std::thread::JoinHandle<String>) {
        serve_after(Duration::ZERO, status, body)
    }

    /// [`serve`], answering only after `delay`.
    fn serve_after(
        delay: Duration,
        status: &'static str,
        body: String,
    ) -> (u16, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read the head, then as much body as Content-Length says.
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = stream.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&raw).to_string();
            std::thread::sleep(delay);
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            request
        });
        (port, handle)
    }

    fn client(port: u16, dir: &tempfile::TempDir) -> HttpAgents {
        let token = dir.path().join("api-token");
        std::fs::write(&token, "tok-123").unwrap();
        let mut config = Config::defaults_for(false);
        config.set_field("server_port", json!(port));
        HttpAgents::new(&config, token)
    }

    #[test]
    fn reads_send_the_token_header_and_parse_v1_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let (port, handle) = serve(
            "200 OK",
            json!({"api_version": 1, "enabled": true, "revision": 4, "counts": {}}).to_string(),
        );
        let status = client(port, &dir).status().unwrap();
        assert!(status.enabled && status.revision == 4);
        let request = handle.join().unwrap();
        assert!(
            request.starts_with("GET /api/agents/status HTTP/1.1"),
            "{request}"
        );
        assert!(request
            .to_lowercase()
            .contains("x-localflow-token: tok-123"));
        assert!(!request.contains("token="), "never in the URL");

        let (port, handle) = serve(
            "200 OK",
            json!({"revision": 9, "sessions": [{"id": "s1", "state": "running"}],
                   "deleted": ["s0"], "reset": false, "counts": {}, "waited": true})
            .to_string(),
        );
        let changes = client(port, &dir).changes(3, 99).unwrap();
        let request = handle.join().unwrap();
        assert!(
            request.starts_with("GET /api/agents/changes?since=3&wait=25 "),
            "wait is capped: {request}"
        );
        assert_eq!(changes.revision, 9);
        assert_eq!(changes.deleted, ["s0"]);
        assert!(changes.waited);

        let (port, handle) = serve(
            "200 OK",
            json!({"session": {"id": "a/b", "state": "completed", "prompt": "x"}}).to_string(),
        );
        let session = client(port, &dir).session("a/b").unwrap();
        assert!(handle
            .join()
            .unwrap()
            .starts_with("GET /api/agents/sessions/a%2Fb "));
        assert_eq!(session.prompt.as_deref(), Some("x"));

        let (port, handle) = serve(
            "200 OK",
            json!({"sessions": [], "next_cursor": null, "revision": 1, "counts": {}}).to_string(),
        );
        assert!(client(port, &dir)
            .sessions(50, Some("c1"))
            .unwrap()
            .sessions
            .is_empty());
        assert!(handle
            .join()
            .unwrap()
            .starts_with("GET /api/agents/sessions?limit=50&before=c1 "));
    }

    fn session_body(id: &str, state: &str) -> Value {
        json!({"session": {
            "id": id, "provider": "mock", "profile": "", "state": state, "origin": "api",
            "rule_id": "", "title": "T", "prompt": "p", "repo": "", "cwd": "", "parent_id": "",
            "root_id": id, "external_id": "", "external_url": "", "sandbox": true,
            "created_at": "2026-11-20T12:04:00Z", "started_at": null,
            "updated_at": "2026-11-20T12:04:00Z", "finished_at": null, "timeout_seconds": 3600,
            "last_seq": 1, "revision": 2, "result_text": "", "result_truncated": false,
            "error": null, "usage": null, "cancel_requested": false,
        }})
    }

    /// The request body, after the blank line.
    fn body_of(request: &str) -> Value {
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    #[test]
    fn writes_send_exact_bodies_and_parse_v1_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let (port, handle) = serve(
            "202 Accepted",
            session_body("0123456789ab", "queued").to_string(),
        );
        let launched = client(port, &dir)
            .launch(&LaunchRequest {
                id: "0123456789ab".into(),
                prompt: "Do the thing".into(),
                provider: "mock".into(),
                profile: Some(String::new()),
                repo: Some("acme/web".into()),
            })
            .unwrap();
        assert_eq!(launched.runtime, agents::RuntimeState::Queued);
        assert!(launched.sandbox);
        let request = handle.join().unwrap();
        assert!(
            request.starts_with("POST /api/agents/sessions HTTP/1.1"),
            "{request}"
        );
        let body = body_of(&request);
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["prompt", "provider", "id", "repo", "origin"],
            "no empty profile, no cwd"
        );
        assert_eq!(body["origin"], "api");
        assert!(keys.iter().all(|k| agents::LAUNCH_FIELDS.contains(k)));

        let (port, handle) = serve(
            "200 OK",
            session_body("0123456789ab", "cancelling").to_string(),
        );
        let stopped = client(port, &dir).cancel("0123456789ab").unwrap();
        assert_eq!(stopped.runtime, agents::RuntimeState::Cancelling);
        let request = handle.join().unwrap();
        assert!(request.starts_with("POST /api/agents/sessions/0123456789ab/cancel "));
        assert!(request.ends_with("\r\n\r\n"), "an empty body: {request}");

        let (port, handle) = serve("200 OK", json!({"deleted": true}).to_string());
        client(port, &dir).remove("0123456789ab").unwrap();
        assert!(handle
            .join()
            .unwrap()
            .starts_with("DELETE /api/agents/sessions/0123456789ab "));

        let mut reply = session_body("0123456789ab", "running");
        reply["event"] = json!({"seq": 2, "ts": "2026-11-20T12:05:00Z", "type": "message",
            "stream": "user", "text": "yes", "state": null, "data": {}});
        let (port, handle) = serve("202 Accepted", reply.to_string());
        let (session, event) = client(port, &dir).message("0123456789ab", "yes").unwrap();
        assert_eq!(session.runtime, agents::RuntimeState::Running);
        assert_eq!((event.seq, event.stream), (2, agents::Stream::User));
        let request = handle.join().unwrap();
        assert!(request.starts_with("POST /api/agents/sessions/0123456789ab/messages "));
        assert_eq!(body_of(&request), json!({"text": "yes"}), "text only");

        let (port, handle) = serve(
            "200 OK",
            json!({"events": [], "next_after": 4, "last_seq": 4, "has_more": false,
                   "state": "running", "terminal": false, "waited": true})
            .to_string(),
        );
        let page = client(port, &dir)
            .events("0123456789ab", 4, 200, 60)
            .unwrap();
        assert!(page.waited && page.next_after == 4);
        assert!(handle.join().unwrap().starts_with(
            "GET /api/agents/sessions/0123456789ab/events?after=4&limit=200&wait=25 "
        ));
    }

    #[test]
    fn rejected_writes_carry_the_envelope_code() {
        let dir = tempfile::tempdir().unwrap();
        let request = LaunchRequest {
            id: "0123456789ab".into(),
            prompt: "x".into(),
            provider: "mock".into(),
            profile: None,
            repo: None,
        };
        for (status, code) in [
            ("409 Conflict", "provider_not_ready"),
            ("413 Payload Too Large", "prompt_too_long"),
            ("429 Too Many Requests", "queue_full"),
            ("502 Bad Gateway", "upstream_error"),
        ] {
            let (port, handle) = serve(
                status,
                json!({"detail": "nope: token=abcdefghijklmnop", "code": code}).to_string(),
            );
            let Err(AgentsError::Rejected(e)) = client(port, &dir).launch(&request) else {
                panic!("{code}: expected a rejection");
            };
            handle.join().unwrap();
            assert_eq!(e.code, code);
            assert!(!e.detail.contains("abcdefghijklmnop"), "{}", e.detail);
        }
    }

    fn provider_body(code: &str, checked: &str) -> Value {
        json!({"id": "devin", "profile": "", "label": "Devin", "builtin": true,
               "capabilities": {"kind": "cloud", "resume": true, "live_message": false,
                                "cancel": false, "streaming": false, "needs_cwd": false,
                                "reattach": false},
               "secrets": [{"name": "agent_devin_api_key", "set": code == "ok",
                            "required": true}],
               "readiness": {"ready": code == "ok", "code": code, "detail": "",
                             "checked": checked}})
    }

    #[test]
    fn settings_routes_send_exact_methods_paths_and_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let (port, handle) = serve(
            "200 OK",
            json!({"api_version": 1, "enabled": false, "sandbox": true,
                   "default_provider": "claude", "default_repo": "web",
                   "repos": [{"alias": "web", "path": "/srv/web", "exists": true}]})
            .to_string(),
        );
        let settings = client(port, &dir).settings().unwrap();
        assert!(handle
            .join()
            .unwrap()
            .starts_with("GET /api/agents/status HTTP/1.1"));
        assert!(!settings.enabled && settings.sandbox);
        assert_eq!(settings.repos[0].path, "/srv/web");

        let (port, handle) = serve("200 OK", json!({"agents": {}}).to_string());
        let patch = json!({"agents": {"repos": ["web=/srv/web"], "default_repo": ""}});
        client(port, &dir).set_config(&patch).unwrap();
        let request = handle.join().unwrap();
        assert!(request.starts_with("PUT /api/config HTTP/1.1"), "{request}");
        assert!(request
            .to_lowercase()
            .contains("x-localflow-token: tok-123"));
        assert_eq!(body_of(&request), patch, "exactly the patch");

        let (port, handle) = serve("200 OK", provider_body("ok", "deep").to_string());
        let checked = client(port, &dir).check_provider("devin", "").unwrap();
        let request = handle.join().unwrap();
        assert!(request.starts_with("POST /api/agents/providers/devin/check "));
        assert_eq!(body_of(&request), json!({}));
        assert!(checked.deep);
        assert_eq!(checked.readiness, agents::Readiness::Connected);

        let (port, handle) = serve("200 OK", provider_body("ok", "deep").to_string());
        client(port, &dir).check_provider("http", "p1").unwrap();
        assert_eq!(body_of(&handle.join().unwrap()), json!({"profile": "p1"}));

        let (port, handle) = serve("200 OK", provider_body("ok", "shallow").to_string());
        let value = SecretValue::new("sk-planted-secret-0123456789".into());
        let saved = client(port, &dir)
            .set_secret("devin", "", "agent_devin_api_key", &value)
            .unwrap();
        let request = handle.join().unwrap();
        assert!(request.starts_with("PUT /api/agents/providers/devin/secret "));
        assert_eq!(
            body_of(&request),
            json!({"name": "agent_devin_api_key", "value": "sk-planted-secret-0123456789"}),
            "name and value only, no empty profile"
        );
        assert!(!request
            .split("\r\n\r\n")
            .next()
            .unwrap()
            .contains("planted"));
        assert!(saved.secrets[0].set);

        let (port, handle) = serve("200 OK", provider_body("ok", "shallow").to_string());
        client(port, &dir)
            .set_secret("http", "p1", "agent_http_p1", &SecretValue::removal())
            .unwrap();
        assert_eq!(
            body_of(&handle.join().unwrap()),
            json!({"name": "agent_http_p1", "value": "", "profile": "p1"})
        );
    }

    /// A secret never comes back out of a failed request, whatever the
    /// service answers (the body isn't echoed into any error).
    #[test]
    fn a_refused_key_carries_the_code_and_never_the_value() {
        let dir = tempfile::tempdir().unwrap();
        for (status, body) in [
            (
                "503 Service Unavailable",
                json!({"detail": "The keyring is unavailable; the key was not saved.",
                       "code": "secrets_unavailable"}),
            ),
            (
                "403 Forbidden",
                json!({"detail": "This change is only allowed from this computer",
                       "code": "local_only"}),
            ),
            (
                "400 Bad Request",
                json!({"detail": "echo sk-planted-secret-0123456789", "code": "validation"}),
            ),
            ("500 Internal Server Error", json!({"nope": true})),
        ] {
            let (port, handle) = serve(status, body.to_string());
            let value = SecretValue::new("sk-planted-secret-0123456789".into());
            let error = client(port, &dir)
                .set_secret("devin", "", "agent_devin_api_key", &value)
                .unwrap_err();
            handle.join().unwrap();
            for text in [
                format!("{error:?}"),
                error.message(),
                error.reason(),
                crate::controllers::agents_settings::key_error(&error),
            ] {
                assert!(!text.contains("planted-secret"), "{status}: {text}");
            }
            assert!(!format!("{value:?}").contains("planted"));
        }
        let (port, handle) = serve(
            "503 Service Unavailable",
            json!({"detail": "x", "code": "secrets_unavailable"}).to_string(),
        );
        let error = client(port, &dir)
            .set_secret("devin", "", "a", &SecretValue::new("v".into()))
            .unwrap_err();
        handle.join().unwrap();
        assert_eq!(error.code(), Some("secrets_unavailable"));
    }

    /// `/api/config` errors are FastAPI's `{detail}`: mapped by status.
    #[test]
    fn config_errors_map_by_status() {
        let dir = tempfile::tempdir().unwrap();
        for (status, body, code, detail) in [
            (
                "400 Bad Request",
                json!({"detail": "invalid value for config key 'agents.repos'"}),
                "validation",
                "invalid value for config key 'agents.repos'",
            ),
            (
                "403 Forbidden",
                json!({"detail": "agents.claude_bin, agents.repos and agents.allow_remote can only be changed on this computer"}),
                "local_only",
                "agents.claude_bin, agents.repos and agents.allow_remote can only be changed on this computer",
            ),
            (
                "422 Unprocessable Entity",
                json!({"detail": [{"loc": ["body"], "msg": "field required"}]}),
                "validation",
                "LocalFlow didn't accept that setting",
            ),
            ("500 Internal Server Error", json!({}), "internal", "internal server error"),
        ] {
            let (port, handle) = serve(status, body.to_string());
            let Err(AgentsError::Rejected(e)) =
                client(port, &dir).set_config(&json!({"agents": {"enabled": true}}))
            else {
                panic!("{status}: expected a rejection");
            };
            handle.join().unwrap();
            assert_eq!((e.code.as_str(), e.detail.as_str()), (code, detail), "{status}");
        }
        let (port, handle) = serve("401 Unauthorized", "{}".into());
        let error = client(port, &dir)
            .set_config(&json!({"agents": {}}))
            .unwrap_err();
        handle.join().unwrap();
        assert!(!error.message().contains("tok-123"));
        assert_eq!(error.code(), Some("auth_failed"));
    }

    /// A deep check may take longer than an ordinary read's 5 s.
    #[test]
    fn a_deep_check_waits_past_the_read_timeout() {
        assert!(CHECK_TIMEOUT >= Duration::from_secs(20));
        let dir = tempfile::tempdir().unwrap();
        let (port, handle) = serve_after(
            TIMEOUT + Duration::from_millis(600),
            "200 OK",
            provider_body("auth_failed", "deep").to_string(),
        );
        let checked = client(port, &dir).check_provider("devin", "").unwrap();
        handle.join().unwrap();
        assert_eq!(checked.readiness, agents::Readiness::NeedsSignIn);
        assert!(checked.deep);
    }

    #[test]
    fn errors_are_safe_human_text() {
        let dir = tempfile::tempdir().unwrap();
        let (port, handle) = serve("404 Not Found", r#"{"detail":"Not Found"}"#.into());
        assert_eq!(client(port, &dir).status(), Err(AgentsError::Absent));
        handle.join().unwrap();

        let (port, handle) = serve(
            "409 Conflict",
            json!({"detail": "provider not ready: api_key=sk-abcdefghijklmnopqrstuv",
                   "code": "provider_not_ready"})
            .to_string(),
        );
        let Err(AgentsError::Refused(detail)) = client(port, &dir).providers() else {
            panic!("expected a refusal");
        };
        handle.join().unwrap();
        assert!(detail.starts_with("provider not ready"));
        assert!(!detail.contains("abcdefghijklmnop"), "{detail}");

        let (port, handle) = serve("401 Unauthorized", "{}".into());
        let Err(AgentsError::Refused(detail)) = client(port, &dir).sessions(1, None) else {
            panic!("expected a refusal");
        };
        handle.join().unwrap();
        assert!(!detail.contains("tok-123"));

        let (port, handle) = serve("200 OK", "not json".into());
        assert!(matches!(
            client(port, &dir).status(),
            Err(AgentsError::Refused(_))
        ));
        handle.join().unwrap();

        // Nothing listening, or no token file: offline, not an error body.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = listener.local_addr().unwrap().port();
        drop(listener);
        assert_eq!(client(closed, &dir).status(), Err(AgentsError::Offline));
        let mut config = Config::defaults_for(false);
        config.set_field("server_port", json!(closed));
        let missing = HttpAgents::new(&config, dir.path().join("nope"));
        assert_eq!(missing.changes(0, 0), Err(AgentsError::Offline));
    }
}
