use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use lf_core::config::Config;
use lf_server::{
    backend::{Backend, BackendRequest, BackendResponse, UnavailableBackend},
    router, ServerState,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn app() -> Router {
    router(Arc::new(ServerState::new(
        Config::default(),
        "test-token".into(),
        Arc::new(UnavailableBackend),
    )))
}

async fn get(app: Router, path: &str, host: &str, token: Option<&str>) -> (u16, Value) {
    let mut request = Request::builder().uri(path).header("Host", host);
    if let Some(token) = token {
        request = request.header("X-Localflow-Token", token);
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

#[tokio::test]
async fn auth_covers_every_path_and_uses_scope_not_host() {
    for path in [
        "/settings",
        "/api/config",
        "/api/status",
        "/meeting/status",
        "/prompts",
        "/docs",
        "/unknown",
        "/%61pi/config",
    ] {
        for host in [
            "127.0.0.1:8756",
            "127.0.0.1:8756/?",
            "127.0.0.1:8756/#",
            "[::1]:8756",
        ] {
            assert_eq!(
                get(app(), path, host, None).await,
                (401, json!({"detail": "invalid API token"})),
                "{path} {host}"
            );
        }
        assert_eq!(
            get(app(), path, "remote.example", None).await,
            (403, json!({"detail": "invalid host"}))
        );
    }
    assert_eq!(
        get(app(), "/healthz", "remote.example", None).await,
        (200, json!({"ok": true}))
    );
    assert_eq!(
        get(app(), "/favicon.ico", "remote.example", None).await.0,
        404
    );
    assert_eq!(
        get(
            app(),
            "/api/config?token=test-token",
            "remote.example",
            None
        )
        .await
        .0,
        200
    );
    assert_eq!(
        get(
            app(),
            "/api/config?token=test-token",
            "localhost",
            Some("wrong")
        )
        .await
        .0,
        401
    );
    assert_eq!(
        get(
            app(),
            "/api/config?token=wrong&token=test-token",
            "localhost",
            None
        )
        .await
        .0,
        200
    );
    assert_eq!(
        get(app(), "/%61pi/config", "localhost", Some("test-token"))
            .await
            .0,
        200
    );
}

#[tokio::test]
async fn unavailable_runtime_is_explicit_and_methods_remain_compatible() {
    assert_eq!(
        get(app(), "/meeting/status", "localhost", Some("test-token")).await,
        (503, json!({"detail": "meeting backend unavailable"}))
    );
    assert_eq!(
        get(app(), "/transcribe", "localhost", Some("test-token"))
            .await
            .0,
        405
    );
    assert_eq!(
        get(app(), "/missing", "localhost", Some("test-token")).await,
        (404, json!({"detail": "Not Found"}))
    );
    let request = Request::builder()
        .method("POST")
        .uri("/transcribe")
        .header("Host", "localhost")
        .header("X-Localflow-Token", "test-token")
        .header("Content-Length", lf_server::MAX_UPLOAD_BYTES + 1)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app().oneshot(request).await.unwrap().status(), 413);
}

#[tokio::test]
async fn encoded_trailing_slashes_redirect_once_and_preserve_query() {
    for path in ["/api/config/", "/api/config%2F", "/api/config%2f/%2F"] {
        let request = Request::builder()
            .uri(format!("{path}?keep=%2F"))
            .header("X-Localflow-Token", "test-token")
            .body(Body::empty())
            .unwrap();
        let response = app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 307);
        let target = response.headers()["Location"].to_str().unwrap();
        assert_eq!(target, "/api/config?keep=%2F");
        assert_eq!(
            get(app(), target, "localhost", Some("test-token")).await.0,
            200
        );
    }
}

#[tokio::test]
async fn pages_embed_only_proven_token_and_never_cache() {
    let app = app();
    for (path, token, expected) in [("/", None, false), ("/settings", Some("test-token"), true)] {
        let mut request = Request::builder().uri(path).header("Host", "localhost");
        if let Some(token) = token {
            request = request.header("X-Localflow-Token", token);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.headers()["cache-control"], "no-store");
        let page = String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert_eq!(page.contains("test-token"), expected);
        assert!(!page.contains("__LOCALFLOW_TOKEN__"));
        assert!(!page.contains("__LOCALFLOW_LAUNCH_TOKEN__"));
    }
    let issued = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/launch-code")
                .header("X-Localflow-Token", "test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let issued: Value =
        serde_json::from_slice(&to_bytes(issued.into_body(), 4096).await.unwrap()).unwrap();
    let form = format!("launch={}", issued["code"].as_str().unwrap());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/settings")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let page = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(page.contains("test-token"));
}

struct EchoBackend;
impl Backend for EchoBackend {
    fn handle(&self, r: BackendRequest) -> BackendResponse {
        BackendResponse {
            status: 202,
            body: json!({"path": r.path, "method": r.method.as_str(), "body": std::str::from_utf8(&r.body).unwrap(), "query": r.query}),
        }
    }
}

#[tokio::test]
async fn runtime_routes_forward_to_injected_backend() {
    let app = router(Arc::new(ServerState::new(
        Config::default(),
        "test-token".into(),
        Arc::new(EchoBackend),
    )));
    for path in [
        "/meeting/start",
        "/meeting/notes",
        "/meeting/chat",
        "/meeting/stop",
        "/meeting/dismiss",
        "/api/recall",
        "/prompts/id/approve",
        "/prompts/id/reject",
        "/transcribe",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("{path}?clean=1"))
                    .header("X-Localflow-Token", "test-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 202, "{path}");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["path"], path);
        assert_eq!(body["body"], "{}");
        assert_eq!(body["query"]["clean"], "1");
    }
}

/// The only test here that touches the perf ledger env var (process-global).
#[tokio::test]
async fn perf_summary_needs_the_token_reads_the_ledger_and_rejects_bad_windows() {
    use lf_core::perf_ledger::{self, Kind, PerfEvent, PerfLedger, Status};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("perf").join("events.jsonl");
    std::env::set_var(perf_ledger::ENV_PATH, &path);

    for host in ["localhost", "127.0.0.1:8756"] {
        assert_eq!(
            get(app(), "/api/perf", host, None).await,
            (401, json!({"detail": "invalid API token"}))
        );
        assert_eq!(
            get(app(), "/api/perf", host, Some("wrong")).await,
            (401, json!({"detail": "invalid API token"}))
        );
    }
    assert_eq!(
        get(app(), "/api/perf?window=7d", "remote.example", None).await,
        (403, json!({"detail": "invalid host"}))
    );

    let (status, empty) = get(app(), "/api/perf", "localhost", Some("test-token")).await;
    assert_eq!(status, 200);
    assert_eq!(
        (&empty["window"], &empty["total_events"]),
        (&json!("7d"), &json!(0))
    );
    assert!(!path.exists(), "reading must not create the ledger");

    let ledger = PerfLedger::new(&path);
    for (kind, status, total) in [
        (Kind::Dictate, Status::Ok, 120.0),
        (Kind::Dictate, Status::Error, 5.0),
        (Kind::SttLoad, Status::Ok, 800.0),
    ] {
        assert!(ledger.record(&PerfEvent {
            total_ms: Some(total),
            ..PerfEvent::new(kind, status)
        }));
    }
    for window in ["24h", "7d", "30d"] {
        let request = Request::builder()
            .uri(format!("/api/perf?window={window}&token=test-token"))
            .header("Host", "localhost")
            .body(Body::empty())
            .unwrap();
        let response = app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "application/json");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["window"], window);
        assert_eq!(value["total_events"], 3);
        assert_eq!(value["by_kind"]["dictate"]["failure_rate"], 0.5);
        assert_eq!(value["model_loads"][0]["last_ms"], 800.0);
        // The canonical compact body (Python repr floats), not serde's own.
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            perf_ledger::summary_json(&value)
        );
    }
    for bad in ["1y", "", "7D"] {
        assert_eq!(
            get(
                app(),
                &format!("/api/perf?window={bad}"),
                "localhost",
                Some("test-token")
            )
            .await,
            (400, json!({"detail": "window must be one of 24h, 7d, 30d"})),
            "{bad:?}"
        );
    }
    std::env::remove_var(perf_ledger::ENV_PATH);
}

/// Echoes the speaker flags of the config each request was handed.
struct Flags;
impl Backend for Flags {
    fn handle(&self, request: BackendRequest) -> BackendResponse {
        BackendResponse {
            status: 200,
            body: json!({
                "path": request.path,
                "recognition": request.config.field_bool("speaker_recognition"),
                "vault": request.config.field_str("vault_path"),
            }),
        }
    }
}

#[tokio::test]
async fn speaker_routes_follow_the_saved_config_while_other_routes_keep_the_loaded_one() {
    // The server's loaded config and the "saved file", edited while the
    // server runs (here also to a different vault, as `lf config set` may).
    let mut loaded = Config::default();
    loaded.set_field("vault_path", json!("/loaded/vault"));
    let saved = Arc::new(std::sync::Mutex::new(Config::default()));
    saved
        .lock()
        .unwrap()
        .set_field("vault_path", json!("/live/vault"));
    let file = saved.clone();
    let state = Arc::new(
        ServerState::new(loaded, "test-token".into(), Arc::new(Flags))
            .with_speaker_config_reader(Arc::new(move || Ok(file.lock().unwrap().clone()))),
    );
    let app = || router(state.clone());
    let send = |method: &str, path: &str, body: Vec<u8>| {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "127.0.0.1")
            .header("X-Localflow-Token", "test-token")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let app = app();
        async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status().as_u16();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or_default(),
            )
        }
    };
    let off = (403, json!({"detail": "speaker recognition is off"}));
    // Default off.
    assert_eq!(send("GET", "/api/speakers", vec![]).await, off);
    assert_eq!(send("POST", "/api/speakers", vec![b' '; 70_000]).await, off);
    // Opt in by editing the saved file: no restart, no PUT /api/config.
    {
        let mut config = saved.lock().unwrap();
        config.set_field("speaker_diarization", json!(true));
        config.set_field("speaker_recognition", json!(true));
    }
    assert_eq!(
        send("GET", "/api/speakers", vec![]).await,
        (
            200,
            json!({"path": "/api/speakers", "recognition": true, "vault": "/loaded/vault"})
        )
    );
    let meeting = format!("/api/meetings/{}/speakers", "a".repeat(32));
    assert_eq!(send("GET", &meeting, vec![]).await.1["recognition"], true);
    // A speaker rename and a meeting save use the same (loaded) vault; only
    // the diarization settings come from the saved file.
    let rename = send(
        "PUT",
        &format!("{meeting}/S1"),
        br#"{"name":"Dana"}"#.to_vec(),
    )
    .await;
    assert_eq!(
        rename,
        (
            200,
            json!({"path": format!("{meeting}/S1"), "recognition": true, "vault": "/loaded/vault"})
        )
    );
    let stop = send("POST", "/meeting/stop", vec![]).await;
    assert_eq!(
        stop,
        (
            200,
            json!({"path": "/meeting/stop", "recognition": false, "vault": "/loaded/vault"})
        )
    );
    // The pre-body contract is intact: gate first, then the size cap.
    assert_eq!(
        send("POST", "/api/speakers", vec![b' '; 70_000]).await,
        (413, json!({"detail": "upload too large"}))
    );
    // Legacy routes keep the server's loaded config.
    assert_eq!(
        send("GET", "/meeting/status", vec![]).await.1["recognition"],
        false
    );
    // Opt out again: refused before any body is read.
    saved
        .lock()
        .unwrap()
        .set_field("speaker_recognition", json!(false));
    assert_eq!(send("POST", "/api/speakers", vec![b' '; 70_000]).await, off);
    assert_eq!(send("GET", &meeting, vec![]).await.1["recognition"], false);
}
