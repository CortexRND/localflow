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
