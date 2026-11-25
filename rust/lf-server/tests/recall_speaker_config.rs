//! Own test binary: it points the process-wide config directory at a tempdir.
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use lf_server::{
    backend::{Backend, BackendRequest, BackendResponse},
    router, ServerState,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "recall-speaker-token";

struct EchoConfig;
impl Backend for EchoConfig {
    fn handle(&self, r: BackendRequest) -> BackendResponse {
        let field = |name| r.config.field(name).cloned();
        BackendResponse {
            status: 200,
            body: json!([field("speaker_diarization"), field("diarization_backend")]),
        }
    }
}

#[tokio::test]
async fn recall_follows_saved_speaker_setting() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", home.path());
    std::env::set_var("WIN_PD_OVERRIDE_LOCAL_APPDATA", home.path());
    std::env::remove_var("LOCALFLOW_CONFIG");
    let dir = lf_core::config::try_config_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("api-token"), TOKEN).unwrap();
    let config = dir.join("config.toml");
    let save = |speakers: bool, backend: &str| {
        let toml = format!(
            "[meetings]\nspeaker_diarization = {speakers}\ndiarization_backend = \"{backend}\"\n"
        );
        std::fs::write(&config, toml).unwrap()
    };
    save(true, "mock");

    let app = router(Arc::new(
        ServerState::load_with_backend(Arc::new(EchoConfig)).unwrap(),
    ));
    let recall = || async {
        let request = Request::post("/api/recall")
            .header("Host", "127.0.0.1:8756")
            .header("X-Localflow-Token", TOKEN)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"question":"who?"}"#))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice::<Value>(&body).unwrap()
    };

    // Only the speaker flag is read live; other saved edits keep the startup value.
    assert_eq!(recall().await, json!([true, "mock"]));
    save(false, "sherpa-onnx");
    assert_eq!(recall().await, json!([false, "mock"]));
    save(true, "sherpa-onnx");
    assert_eq!(recall().await, json!([true, "mock"]));
    std::fs::write(&config, "[meetings\nspeaker_diarization = true\n").unwrap();
    assert_eq!(recall().await, json!([false, "mock"]));
}
