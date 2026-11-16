//! Mirrors the `/transcribe` cases of tests/test_server.py through the
//! `lf_server` backend interface.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lf_core::config::Config;
use lf_core::history::HistoryStore;
use lf_providers::base::{Health, ModelInfo, ProviderError};
use lf_providers::LlmProvider;
use lf_runtime::engine::SpeechEngine;
use lf_runtime::pipeline::Vocabulary;
use lf_runtime::server::{ffmpeg_decode, upload_file, DictationBackend};
use lf_server::backend::{Backend, BackendRequest, BackendResponse};
use serde_json::json;

struct Speech {
    text: String,
    calls: Mutex<usize>,
}

impl SpeechEngine for Speech {
    fn load(&self) -> Result<(), String> {
        Ok(())
    }
    fn transcribe(&self, _pcm: &[f32], _prompt: Option<&str>) -> Result<String, String> {
        *self.calls.lock().unwrap() += 1;
        Ok(self.text.clone())
    }
    fn unload(&self) {}
}

struct Upper;

impl LlmProvider for Upper {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn offline(&self) -> bool {
        true
    }
    fn complete(&self, _s: &str, user: &str, _m: Option<i64>) -> Result<String, ProviderError> {
        Ok(user.strip_prefix("Text:\n").unwrap_or(user).to_uppercase())
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
    fn healthcheck(&self) -> Health {
        Health::ok()
    }
}

struct Rig {
    backend: DictationBackend,
    speech: Arc<Speech>,
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
}

fn rig(text: &str, samples: Vec<f32>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.db");
    let clips = dir.path().join("clips");
    let speech = Arc::new(Speech {
        text: text.into(),
        calls: Mutex::new(0),
    });
    let history_db = db.clone();
    let backend = DictationBackend {
        speech: speech.clone(),
        vocabulary: Arc::new(|_| Vocabulary {
            replacements: vec![("lokal flow".into(), "LocalFlow".into())],
            ..Vocabulary::default()
        }),
        build_llm: Arc::new(|_| Some(Box::new(Upper) as Box<dyn LlmProvider>)),
        history: Arc::new(move |_| {
            HistoryStore::open(Some(history_db.clone()), Some(clips.clone()), true).ok()
        }),
        decode: Arc::new(move |_, _| Ok(samples.clone())),
        fallback: Arc::new(lf_server::backend::UnavailableBackend),
    };
    Rig {
        backend,
        speech,
        _dir: dir,
        db,
    }
}

const BOUNDARY: &str = "XyZ";

fn multipart(field: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"a.m4a\"\r\nContent-Type: audio/mp4\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn request(path: &str, query: &[(&str, &str)], body: Vec<u8>) -> BackendRequest {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "content-type",
        format!("multipart/form-data; boundary={BOUNDARY}")
            .parse()
            .unwrap(),
    );
    let mut config = Config::default();
    config.set_field("spoken_punctuation", json!(false));
    BackendRequest {
        method: http::Method::POST,
        path: path.into(),
        query: query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>(),
        headers,
        body: body.into(),
        config,
    }
}

fn loud() -> Vec<f32> {
    (0..16000)
        .map(|i| if i % 2 == 0 { 0.5 } else { -0.5 })
        .collect()
}

fn history(r: &Rig) -> Vec<(String, String, String, String)> {
    HistoryStore::open(Some(r.db.clone()), None, true)
        .unwrap()
        .recent(10, None, 0)
        .unwrap()
        .into_iter()
        .map(|e| {
            (
                e.app_name.unwrap_or_default(),
                e.raw_text,
                e.final_text,
                e.status,
            )
        })
        .collect()
}

fn ok_text(response: BackendResponse) -> String {
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(response.body["ms"].is_i64());
    response.body["text"].as_str().unwrap().to_string()
}

#[test]
fn transcribes_through_the_pipeline_and_records_phone_history() {
    let r = rig("try lokal flow", loud());
    let text = ok_text(
        r.backend
            .handle(request("/transcribe", &[], multipart("file", b"x"))),
    );
    assert_eq!(text, "try LocalFlow");
    let rows = history(&r);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0],
        (
            "phone".into(),
            "try lokal flow".into(),
            "try LocalFlow".into(),
            "ok".into()
        )
    );
}

#[test]
fn clean_runs_the_llm_even_on_short_text() {
    let r = rig("short one", loud());
    let response = r.backend.handle(request(
        "/transcribe",
        &[("clean", "1")],
        multipart("file", b"x"),
    ));
    assert_eq!(ok_text(response), "SHORT ONE");
    let r = rig("short one", loud());
    let response = r.backend.handle(request(
        "/transcribe",
        &[("clean", "0")],
        multipart("file", b"x"),
    ));
    assert_eq!(ok_text(response), "short one");
}

#[test]
fn silence_skips_the_model() {
    let r = rig("never", vec![0.0; 16000]);
    let text = ok_text(
        r.backend
            .handle(request("/transcribe", &[], multipart("file", b"x"))),
    );
    assert_eq!(text, "");
    assert_eq!(*r.speech.calls.lock().unwrap(), 0);
    assert_eq!(history(&r)[0].3, "silent");
}

#[test]
fn request_errors_match_fastapi() {
    let r = rig("x", loud());
    let missing = r
        .backend
        .handle(request("/transcribe", &[], multipart("other", b"x")));
    assert_eq!(missing.status, 422);
    assert_eq!(missing.body["detail"][0]["loc"], json!(["body", "file"]));
    let bad = r.backend.handle(request(
        "/transcribe",
        &[("clean", "yes")],
        multipart("file", b"x"),
    ));
    assert_eq!(bad.status, 422);
    assert_eq!(bad.body["detail"][0]["loc"], json!(["query", "clean"]));
    let empty = rig("x", Vec::new());
    let response = empty
        .backend
        .handle(request("/transcribe", &[], multipart("file", b"x")));
    assert_eq!(response.status, 400);
    assert_eq!(response.body, json!({"detail": "decoded audio is empty"}));
}

#[test]
fn other_routes_use_the_fallback() {
    let r = rig("x", loud());
    let response = r
        .backend
        .handle(request("/meeting/status", &[], Vec::new()));
    assert_eq!(response.status, 503);
    assert_eq!(
        response.body,
        json!({"detail": "meeting backend unavailable"})
    );
    assert!(r.backend.transcriber_loaded());
    assert_eq!(r.backend.autostart_status(), "unavailable");
}

#[test]
fn multipart_reads_the_file_part_only() {
    let ct = format!("multipart/form-data; boundary={BOUNDARY}");
    assert_eq!(
        upload_file(&ct, &multipart("file", b"abc\r\ndef")),
        Some(b"abc\r\ndef".to_vec())
    );
    assert_eq!(upload_file(&ct, &multipart("audio", b"abc")), None);
    assert_eq!(upload_file("application/json", b"{}"), None);
}

fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16000u32.to_le_bytes());
    out.extend_from_slice(&32000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[test]
fn ffmpeg_decodes_wav_and_reports_bad_input() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let samples: Vec<i16> = (0..8000)
        .map(|i| if i % 2 == 0 { 8000 } else { -8000 })
        .collect();
    let decoded = ffmpeg_decode(&wav(&samples), 1200).unwrap_or_else(|r| panic!("{}", r.body));
    assert_eq!(decoded.len(), 8000);
    assert!((decoded[0] - 8000.0 / 32768.0).abs() < 1e-3);
    let truncated = ffmpeg_decode(&wav(&samples), 0).unwrap_or_else(|r| panic!("{}", r.body));
    assert_eq!(truncated.len(), 8000, "-t is at least one second");
    let bad = ffmpeg_decode(b"not audio", 1200).unwrap_err();
    assert_eq!(bad.status, 400);
    assert!(bad.body["detail"].as_str().unwrap().chars().count() <= 300);
}
