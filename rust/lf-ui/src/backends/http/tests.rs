use super::*;
use lf_server::backend::{Backend, BackendRequest, BackendResponse};
use std::collections::HashMap;
use std::future::IntoFuture;
use std::sync::{
    atomic::{AtomicU16, AtomicU64, Ordering},
    Mutex,
};

type Recorded = (Method, String, HashMap<String, String>, Value);

#[derive(Default)]
struct FakeBackend {
    status: AtomicU16,
    delay_ms: AtomicU64,
    calls: Mutex<Vec<Recorded>>,
    response: Mutex<Option<Value>>,
    /// `detail` of an error status (default: the runtime's 503 text).
    detail: Mutex<Option<String>>,
}

impl Backend for FakeBackend {
    fn handle(&self, request: BackendRequest) -> BackendResponse {
        std::thread::sleep(Duration::from_millis(self.delay_ms.load(Ordering::SeqCst)));
        let body = if request.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&request.body).unwrap()
        };
        self.calls.lock().unwrap().push((
            request.method,
            request.path.clone(),
            request.query.clone(),
            body,
        ));
        let status = self.status.load(Ordering::SeqCst);
        if status >= 400 {
            let detail = self.detail.lock().unwrap().clone();
            return BackendResponse::error(
                status,
                detail.unwrap_or_else(|| "runtime backend unavailable".into()),
            );
        }
        let body = self.response.lock().unwrap().clone().unwrap_or_else(|| match request.path.as_str() {
            "/prompts" => json!({"entries": if request.query.get("status").is_some_and(|s| s == "empty") { vec![] } else { vec![json!({"id":"one", "status":"pending"})] }}),
            "/prompts/one/approve" => json!({"id":"one", "status":"approved"}),
            "/prompts/one/reject" => json!({"id":"one", "status":"rejected"}),
            "/api/recall" => json!({"answer":"The decision [1].", "citations":[{"n":1,"source":"dictation","title":"Test","app":"Test","when":"2026-11-16","snippet":"decision","ref":"42"}]}),
            _ => json!({"ok":true}),
        });
        BackendResponse {
            status: if status == 0 { 200 } else { status },
            body,
        }
    }
}

struct Server {
    base: String,
    backend: Arc<FakeBackend>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn new() -> Self {
        let backend = Arc::new(FakeBackend::default());
        let state = Arc::new(lf_server::ServerState::new(
            Config::defaults_for(true),
            "test-token".into(),
            backend.clone(),
        ));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (stop, receiver) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                tokio::select! {
                    result = axum::serve(listener, lf_server::router(state)).into_future() => result.unwrap(),
                    _ = receiver => (),
                }
            });
        });
        Self {
            base,
            backend,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
    fn http(&self) -> HttpTransport {
        HttpTransport::new(&self.base, || Ok("test-token".into())).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn runtime_poller_fetches_authenticated_status_and_reports_down() {
    use lf_runtime::menubar::{MeetingStatusPoller, MenuState, PollResult};

    let server = Server::new();
    let status = json!({"session": null, "detected": false});
    *server.backend.response.lock().unwrap() = Some(status.clone());
    let client = HttpMeetingClient(server.http());
    let fetch = Arc::new(Mutex::new(client.status_fetch()));
    let active_fetch = Arc::clone(&fetch);
    let menu = MenuState::new(None, true);
    let updates = menu.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let _poller = MeetingStatusPoller::start(
        Arc::new(move || {
            let fetch = active_fetch.lock().unwrap().clone();
            fetch()
        }),
        Arc::new(move |result| {
            updates.change(|model| model.apply_status(&result));
            send.send(result).unwrap();
        }),
    );
    let next = || receive.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(next(), PollResult::Ok(status));
    assert_eq!(menu.snapshot().status_line, "watching · idle");
    let calls = server.backend.calls.lock().unwrap();
    assert_eq!(calls[0].0, Method::GET);
    assert_eq!(calls[0].1, "/meeting/status");
    drop(calls);

    server.backend.status.store(503, Ordering::SeqCst);
    _poller.poke();
    assert_eq!(next(), PollResult::Down);
    assert_eq!(menu.snapshot().status_line, "server: down (lf serve)");

    server.backend.status.store(0, Ordering::SeqCst);
    *server.backend.response.lock().unwrap() = Some(Value::Null);
    _poller.poke();
    assert_eq!(next(), PollResult::Ok(json!({})));

    let down =
        HttpMeetingClient(HttpTransport::new("http://127.0.0.1:0", || Ok(String::new())).unwrap());
    *fetch.lock().unwrap() = down.status_fetch();
    _poller.poke();
    assert_eq!(next(), PollResult::Down);
}

#[test]
fn runtime_status_fetch_uses_two_second_deadline() {
    use lf_runtime::menubar::{MeetingStatusPoller, PollResult};

    let server = Server::new();
    server.backend.delay_ms.store(2500, Ordering::SeqCst);
    let client = HttpMeetingClient(server.http());
    let (send, receive) = std::sync::mpsc::channel();
    let _poller = MeetingStatusPoller::start(
        client.status_fetch(),
        Arc::new(move |result| {
            let _ = send.send(result);
        }),
    );
    assert_eq!(
        receive.recv_timeout(Duration::from_secs(5)).unwrap(),
        PollResult::Down
    );
}

#[test]
fn meeting_routes_methods_and_payloads_match_python() {
    let server = Server::new();
    let client = HttpMeetingClient(server.http());
    assert_eq!(client.status().unwrap(), json!({"ok":true}));
    client.templates().unwrap();
    client
        .start("Test", "Work", "default", "notes", Some(false))
        .unwrap();
    client
        .start("Meeting", "Other", "default", "", None)
        .unwrap();
    client.save_notes("draft").unwrap();
    client.stop().unwrap();
    client.chat("What changed?").unwrap();
    client.dismiss().unwrap();
    let calls = server.backend.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|c| (c.0.clone(), c.1.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (Method::GET, "/meeting/status"),
            (Method::GET, "/meeting/templates"),
            (Method::POST, "/meeting/start"),
            (Method::POST, "/meeting/start"),
            (Method::POST, "/meeting/notes"),
            (Method::POST, "/meeting/stop"),
            (Method::POST, "/meeting/chat"),
            (Method::POST, "/meeting/dismiss"),
        ]
    );
    assert_eq!(
        calls[2].3,
        json!({"title":"Test","category":"Work","template":"default","user_notes":"notes","auto_title":false})
    );
    assert!(calls[3].3.get("auto_title").is_none());
    assert_eq!(calls[4].3, json!({"notes":"draft"}));
    assert_eq!(calls[6].3, json!({"question":"What changed?"}));
}

const MEETING_ID: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn speaker_routes_methods_paths_and_bodies_match_python() {
    let server = Server::new();
    let client = HttpMeetingClient(server.http());
    let speakers =
        json!({"meeting_id": MEETING_ID, "speakers": [{"id": "S1", "label": "Speaker 1"}]});
    *server.backend.response.lock().unwrap() = Some(speakers.clone());
    assert_eq!(client.speakers(MEETING_ID).unwrap(), Some(speakers.clone()));
    assert_eq!(
        client.rename_speaker(MEETING_ID, "S1", "Ana").unwrap(),
        speakers
    );
    assert_eq!(
        client.merge_speakers(MEETING_ID, "S2", "S1").unwrap(),
        speakers
    );
    client.diarization_status().unwrap();
    let calls = server.backend.calls.lock().unwrap();
    let seen: Vec<(Method, &str, &Value)> = calls
        .iter()
        .map(|c| (c.0.clone(), c.1.as_str(), &c.3))
        .collect();
    let speakers_path = format!("/api/meetings/{MEETING_ID}/speakers");
    let rename_path = format!("/api/meetings/{MEETING_ID}/speakers/S1");
    let merge_path = format!("/api/meetings/{MEETING_ID}/speakers/merge");
    assert_eq!(
        seen,
        vec![
            (Method::GET, speakers_path.as_str(), &Value::Null),
            (Method::PUT, rename_path.as_str(), &json!({"name": "Ana"})),
            (
                Method::POST,
                merge_path.as_str(),
                &json!({"from": "S2", "into": "S1"})
            ),
            (Method::GET, "/api/diarization/status", &Value::Null),
        ]
    );
}

#[test]
fn speakers_404_is_no_data_and_other_errors_keep_the_server_detail() {
    let server = Server::new();
    let client = HttpMeetingClient(server.http());
    let fail = |status: u16, detail: Option<&str>| {
        server.backend.status.store(status, Ordering::SeqCst);
        *server.backend.detail.lock().unwrap() = detail.map(str::to_string);
    };
    fail(404, Some("no speaker data for this meeting"));
    assert_eq!(client.speakers(MEETING_ID).unwrap(), None);
    // An edit's 404 is an error the person sees, in the server's words.
    fail(404, Some("unknown speaker"));
    assert_eq!(
        client
            .rename_speaker(MEETING_ID, "S9", "Ana")
            .unwrap_err()
            .0,
        "unknown speaker"
    );
    fail(400, Some("invalid speaker name"));
    assert_eq!(
        client
            .rename_speaker(MEETING_ID, "S1", "***")
            .unwrap_err()
            .0,
        "invalid speaker name"
    );
    fail(409, Some("transcript is outside the meeting vault"));
    assert_eq!(
        client.merge_speakers(MEETING_ID, "S2", "S1").unwrap_err().0,
        "transcript is outside the meeting vault"
    );
    // A transient failure is an error, never "no data" (U4 must not cache it).
    fail(503, None);
    assert!(client.speakers(MEETING_ID).is_err());
    let down =
        HttpMeetingClient(HttpTransport::new("http://127.0.0.1:0", || Ok(String::new())).unwrap());
    assert_eq!(down.speakers(MEETING_ID).unwrap_err().0, SERVER_DOWN);
    assert_eq!(down.diarization_status().unwrap_err().0, SERVER_DOWN);
}

#[test]
fn hostile_speaker_ids_never_reach_the_server() {
    let server = Server::new();
    let client = HttpMeetingClient(server.http());
    for id in [
        "",
        "..",
        "../x",
        "a/b",
        "%2F",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcde/",
    ] {
        assert_eq!(client.speakers(id).unwrap(), None, "{id:?}");
        assert_eq!(
            client.rename_speaker(id, "S1", "Ana").unwrap_err().0,
            "no speaker data for this meeting"
        );
        assert_eq!(
            client.merge_speakers(id, "S1", "S2").unwrap_err().0,
            "no speaker data for this meeting"
        );
    }
    for sid in ["", ".", "..", "a/b", "%2F", "S 1", "?x", "#"] {
        assert_eq!(
            client.rename_speaker(MEETING_ID, sid, "Ana").unwrap_err().0,
            "unknown speaker",
            "{sid:?}"
        );
    }
    assert!(server.backend.calls.lock().unwrap().is_empty());
}

#[test]
fn prompts_use_server_side_transitions_and_empty_lists() {
    use crate::backends::prompts::{approve_prompt, reject_prompt};
    let server = Server::new();
    let queue = HttpPromptQueue(server.http());
    assert_eq!(queue.list(None).unwrap().len(), 1);
    assert_eq!(queue.list(Some("pending")).unwrap().len(), 1);
    assert!(queue.list(Some("empty")).unwrap().is_empty());
    assert!(queue.get("missing").unwrap().is_none());
    assert!(queue.get("one").unwrap().is_some());
    assert_eq!(approve_prompt(&queue, "one").unwrap()["status"], "approved");
    assert_eq!(reject_prompt(&queue, "one").unwrap()["status"], "rejected");
    assert!(queue
        .set_status("one", "dispatched", None, &Map::new())
        .is_err());
    assert!(queue
        .set_status("one", "approved", Some(&["dispatched"]), &Map::new())
        .is_err());
    assert!(queue.add(&[], &[], "").is_err());
    server.backend.status.store(404, Ordering::SeqCst);
    assert!(queue
        .set_status("missing", "approved", None, &Map::new())
        .unwrap()
        .is_none());
    server.backend.status.store(409, Ordering::SeqCst);
    assert!(approve_prompt(&queue, "one").is_err());
    let calls = server.backend.calls.lock().unwrap();
    assert_eq!(
        calls[1].2.get("status").map(String::as_str),
        Some("pending")
    );
    assert!(calls
        .iter()
        .any(|c| c.0 == Method::POST && c.1 == "/prompts/one/approve"));
}

#[test]
fn recall_timeout_is_300s_by_default_and_can_only_be_shortened() {
    let server = Server::new();
    assert_eq!(HttpAskBackend::new(server.http()).recall_timeout, 300);
    assert_eq!(
        HttpAskBackend::new(server.http())
            .with_recall_timeout(0)
            .recall_timeout,
        1
    );
    assert_eq!(
        HttpAskBackend::new(server.http())
            .with_recall_timeout(10_000)
            .recall_timeout,
        RECALL_TIMEOUT_SECS
    );
    // A shortened timeout really bounds the request.
    server.backend.delay_ms.store(2500, Ordering::SeqCst);
    let mut ask = HttpAskBackend::new(server.http()).with_recall_timeout(1);
    let started = std::time::Instant::now();
    assert_eq!(
        ask.ask("q", "Any time", None, None).error,
        ERROR_UNREACHABLE
    );
    assert!(started.elapsed() < Duration::from_millis(2400));
}

#[test]
fn recall_maps_answer_citations_validation_and_conversation() {
    let server = Server::new();
    let mut ask = HttpAskBackend::new(server.http());
    assert_eq!(
        ask.ask("  ", "Any time", None, None).error,
        ERROR_EMPTY_QUESTION
    );
    assert!(ask.turns().is_empty());
    let turn = ask.ask(" Decision? ", "Past week", None, None);
    assert_eq!(turn.answer, "The decision [1].");
    assert!(turn.error.is_empty());
    assert_eq!(turn.citations[0].reference, "42");
    assert_eq!(
        server.backend.calls.lock().unwrap()[0].3,
        json!({"question":"Decision?", "since":"7d", "k":8, "llm":true})
    );
    let empty = BTreeSet::new();
    assert_eq!(
        ask.ask("q", "Any time", Some(&empty), None).error,
        ERROR_NO_SOURCES
    );
    let selected = BTreeSet::from(["notes".into()]);
    assert_eq!(
        ask.ask("q", "Any time", Some(&selected), None).error,
        FILTERS_UNAVAILABLE
    );
    assert_eq!(ask.ask("q", "Invalid", None, None).error, ERROR_GENERIC);
    assert_eq!(server.backend.calls.lock().unwrap().len(), 1);
    let turns = ask.turns();
    ask.clear();
    assert!(ask.turns().is_empty());
    ask.set_turns(turns.clone());
    assert_eq!(ask.turns(), turns);
}

#[test]
fn unavailable_and_unauthorized_are_errors_not_panics() {
    let server = Server::new();
    server.backend.status.store(503, Ordering::SeqCst);
    let http = server.http();
    assert_eq!(
        HttpMeetingClient(http.clone()).status().unwrap_err().0,
        "runtime backend unavailable"
    );
    assert!(HttpPromptQueue(http.clone()).list(None).is_err());
    assert_eq!(
        crate::hub_model::prompt_entries(&HttpPromptQueue(http.clone())),
        Err(crate::hub_model::PROMPTS_UNREADABLE)
    );
    assert_eq!(
        HttpAskBackend::new(http)
            .ask("q", "Any time", None, None)
            .error,
        ERROR_UNREACHABLE
    );
    let unauthenticated = HttpTransport::new(&server.base, || Ok(String::new())).unwrap();
    assert!(HttpMeetingClient(unauthenticated).status().is_err());
    server.backend.status.store(204, Ordering::SeqCst);
    HttpMeetingClient(server.http()).dismiss().unwrap();
}

#[test]
fn recall_empty_results_and_malformed_response_shapes() {
    let server = Server::new();
    let mut ask = HttpAskBackend::new(server.http());
    *server.backend.response.lock().unwrap() = Some(json!({"answer":NO_ANSWER,"citations":[]}));
    assert_eq!(ask.ask("unknown", "Any time", None, None).answer, NO_ANSWER);
    *server.backend.response.lock().unwrap() = Some(json!({"answer":"   ","citations":[]}));
    assert_eq!(ask.ask("unknown", "Any time", None, None).answer, NO_ANSWER);
    *server.backend.response.lock().unwrap() = Some(json!({"answer":"Answer", "citations":[{}]}));
    assert_eq!(ask.ask("q", "Any time", None, None).error, ERROR_GENERIC);
    *server.backend.response.lock().unwrap() = Some(json!({}));
    assert_eq!(ask.ask("q", "Any time", None, None).error, ERROR_GENERIC);
    assert!(HttpPromptQueue(server.http()).list(None).is_err());
}

#[test]
fn recall_null_answer_with_citations_is_sources_only() {
    let server = Server::new();
    let cite = json!({"n":1,"source":"dictation","title":"Slack","app":"Slack",
        "when":"2026-11-20T10:15","snippet":"Kubernetes rollout Friday","ref":"12"});
    // Default (the AppKit page): unchanged, `null` is still a generic error.
    let mut legacy = HttpAskBackend::new(server.http());
    *server.backend.response.lock().unwrap() =
        Some(json!({"answer": null, "citations": [cite.clone()]}));
    let turn = legacy.ask("rollout?", "Any time", None, None);
    assert_eq!(turn.error, ERROR_GENERIC);
    assert!(turn.citations.is_empty() && !turn.sources_only());
    // Opted in (GPUI Ask).
    let mut ask = HttpAskBackend::new(server.http()).with_sources_only_replies();
    // The model was unavailable: passages only (HTTP 200).
    *server.backend.response.lock().unwrap() =
        Some(json!({"answer": null, "citations": [cite.clone()]}));
    let turn = ask.ask("rollout?", "Any time", None, None);
    assert!(turn.sources_only(), "{turn:?}");
    assert_eq!((turn.answer.as_str(), turn.error.as_str()), ("", ""));
    assert_eq!(turn.citations.len(), 1);
    assert_eq!(turn.citations[0].snippet, "Kubernetes rollout Friday");
    assert_eq!(turn.citations[0].reference, "12");
    assert_eq!(ask.turns().last(), Some(&turn));
    // Null with nothing matched is the no-answer reply, never sources-only.
    *server.backend.response.lock().unwrap() = Some(json!({"answer": null, "citations": []}));
    let turn = ask.ask("q", "Any time", None, None);
    assert_eq!(turn.answer, NO_ANSWER);
    assert!(!turn.sources_only());
    // Missing or invalid answers and malformed citations stay errors.
    for body in [
        json!({"answer": null}),
        json!({"answer": null, "citations": "x"}),
        json!({"answer": null, "citations": [{}]}),
        json!({"answer": null, "citations": [cite.clone(), {"n": "one"}]}),
        json!({"citations": [cite.clone()]}),
        json!({"answer": 5, "citations": [cite.clone()]}),
        json!({"answer": ["a"], "citations": [cite.clone()]}),
    ] {
        *server.backend.response.lock().unwrap() = Some(body.clone());
        let turn = ask.ask("q", "Any time", None, None);
        assert_eq!(turn.error, ERROR_GENERIC, "{body}");
        assert!(turn.citations.is_empty() && !turn.sources_only(), "{body}");
    }
    // Normal answers are unchanged.
    *server.backend.response.lock().unwrap() =
        Some(json!({"answer": "Friday [1].", "citations": [cite]}));
    let turn = ask.ask("q", "Any time", None, None);
    assert_eq!(turn.answer, "Friday [1].");
    assert!(!turn.sources_only());
}

#[test]
fn token_source_is_read_each_request_without_creation_or_rotation() {
    let server = Server::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let token_path = path.clone();
    let client = HttpMeetingClient(
        HttpTransport::new(&server.base, move || apitoken::load_at(&token_path)).unwrap(),
    );
    assert!(client.status().is_err());
    assert!(!path.exists());
    std::fs::write(&path, "test-token").unwrap();
    client.status().unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "test-token");
    std::fs::write(&path, "changed-token").unwrap();
    assert!(client.status().is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed-token");
}

#[test]
fn disconnected_server_maps_to_python_server_down() {
    let client =
        HttpMeetingClient(HttpTransport::new("http://127.0.0.1:0", || Ok(String::new())).unwrap());
    assert_eq!(client.status().unwrap_err().0, SERVER_DOWN);
}

#[test]
fn configured_urls_match_python_for_wildcards_ipv6_and_zones() {
    for host in [
        "",
        " ",
        "*",
        "0.0.0.0",
        "::",
        "::0",
        "[::]",
        "::1",
        "[::1]",
        "fe80::1%en0",
        "localhost",
    ] {
        let mut config = Config::defaults_for(true);
        config.set_field("server_host", json!(host));
        config.set_field("server_port", json!(9000));
        assert_eq!(
            configured_base_url(&config),
            lf_server::server_url::base_url(host, 9000)
        );
    }
    assert!(HttpTransport::new("http://user:password@localhost", || Ok(String::new())).is_err());
}
