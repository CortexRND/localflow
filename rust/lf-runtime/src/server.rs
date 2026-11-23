//! The dictation side of the HTTP server: `POST /transcribe` (`server.transcribe`
//! and `_transcribe_sync`) as an `lf_server::backend::Backend`. Every other
//! backend route goes to `fallback` (by default `UnavailableBackend`).
//!
//! Wire it with `lf_server::ServerState::load_with_backend(Arc::new(backend))`.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_core::config::Config;
use lf_core::dictionary::stt_prompt;
use lf_core::history::{HistoryStore, NewEntry};
use lf_server::backend::{Backend, BackendRequest, BackendResponse, UnavailableBackend};
use serde_json::json;

use crate::engine::{rms_dbfs, SpeechEngine};
use crate::pipeline::render_text_with;
use crate::run::{default_llm_factory, default_vocabulary};
use crate::settings::{bool_field, f64_field, i64_field};
use crate::worker::{LlmFactory, VocabularyFn};

pub const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
pub const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);
/// Demuxers for what browsers' MediaRecorder and iOS Shortcuts produce. Keeps
/// playlist/concat demuxers (which open other URLs or files) out of reach.
pub const FFMPEG_INPUT_FORMATS: &str = "matroska,webm,ogg,mov,mp4,m4a,wav,mp3,aac,flac,caf";

/// Upload bytes and `max_recording_seconds` -> 16 kHz mono f32 samples.
pub type Decoder = Arc<dyn Fn(&[u8], i64) -> Result<Vec<f32>, BackendResponse> + Send + Sync>;
pub type HistoryFactory = Arc<dyn Fn(&Config) -> Option<HistoryStore> + Send + Sync>;

pub struct DictationBackend {
    /// Shared engine; its loaded state is exposed without loading it.
    pub speech: Arc<dyn SpeechEngine>,
    pub vocabulary: VocabularyFn,
    pub build_llm: LlmFactory,
    pub history: HistoryFactory,
    pub decode: Decoder,
    pub fallback: Arc<dyn Backend>,
}

impl DictationBackend {
    /// The user's dictionary, hot phrases, history, LLM settings and ffmpeg.
    pub fn new(speech: Arc<dyn SpeechEngine>, config: &Config) -> Self {
        DictationBackend {
            speech,
            vocabulary: default_vocabulary(config),
            build_llm: default_llm_factory(),
            history: Arc::new(|config| {
                HistoryStore::open(None, None, bool_field(config, "history_enabled", true)).ok()
            }),
            decode: Arc::new(ffmpeg_decode),
            fallback: Arc::new(UnavailableBackend),
        }
    }

    fn transcribe(&self, request: &BackendRequest) -> BackendResponse {
        let start = Instant::now();
        let clean = match request.query.get("clean") {
            None => false,
            Some(v) => match v.trim().parse::<i64>() {
                Ok(n) => n != 0,
                Err(_) => {
                    return validation_error(
                        "int_parsing",
                        ["query", "clean"],
                        "Input should be a valid integer, unable to parse string as an integer",
                        json!(v),
                    )
                }
            },
        };
        let content_type = request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let Some(data) = upload_file(content_type, &request.body) else {
            return validation_error("missing", ["body", "file"], "Field required", json!(null));
        };
        if data.len() > MAX_UPLOAD_BYTES {
            return BackendResponse::error(413, "upload too large");
        }
        let config = &request.config;
        let audio = match (self.decode)(&data, i64_field(config, "max_recording_seconds", 1200)) {
            Ok(a) => a,
            Err(response) => return response,
        };
        if audio.is_empty() {
            return BackendResponse::error(400, "decoded audio is empty");
        }
        match self.transcribe_sync(&audio, clean, config) {
            Ok(text) => BackendResponse {
                status: 200,
                body: json!({"text": text, "ms": start.elapsed().as_millis() as i64}),
            },
            Err(e) => {
                log::error!("transcription failed: {e}");
                BackendResponse::error(500, "Internal Server Error")
            }
        }
    }

    fn transcribe_sync(
        &self,
        audio: &[f32],
        clean: bool,
        config: &Config,
    ) -> Result<String, String> {
        let threshold = f64_field(config, "silence_threshold_dbfs", -50.0);
        if rms_dbfs(audio) < threshold {
            log::info!("silence gate: audio below {threshold:.1} dBFS");
            self.record(config, audio, "", "", "silent");
            return Ok(String::new());
        }
        let vocab = (self.vocabulary)(config);
        let raw = match self
            .speech
            .transcribe(audio, stt_prompt(&vocab.words).as_deref())
        {
            Ok(raw) => raw,
            Err(e) => {
                self.record(config, audio, "", "", "failed");
                return Err(e);
            }
        };
        if raw.is_empty() {
            self.record(config, audio, &raw, "", "silent");
            return Ok(raw);
        }
        let llm = if clean {
            let fast = config
                .uses_light_cleanup()
                .then(|| config.fast_model_config())
                .flatten();
            (self.build_llm)(fast.as_ref().unwrap_or(config))
        } else {
            None
        };
        let text = render_text_with(&raw, config, &vocab, llm.as_deref(), None, 0);
        let status = if text.is_empty() { "silent" } else { "ok" };
        self.record(config, audio, &raw, &text, status);
        Ok(text)
    }

    fn record(&self, config: &Config, audio: &[f32], raw: &str, text: &str, status: &str) {
        let Some(history) = (self.history)(config) else {
            return;
        };
        let entry = NewEntry {
            app_name: Some("phone".into()),
            category: Some("other".into()),
            duration_s: audio.len() as f64 / 16000.0,
            raw_text: raw.into(),
            final_text: text.into(),
            status: status.into(),
            ..NewEntry::new("dictate")
        };
        if let Err(e) = history.add(&entry) {
            log::error!("could not save server dictation history: {e}");
        }
    }
}

impl Backend for DictationBackend {
    fn handle(&self, request: BackendRequest) -> BackendResponse {
        if request.path == "/transcribe" {
            self.transcribe(&request)
        } else {
            self.fallback.handle(request)
        }
    }
    fn stt_available(&self, provider: &str) -> bool {
        self.fallback.stt_available(provider)
    }
    fn transcriber_loaded(&self) -> bool {
        self.speech.loaded()
    }
    fn autostart_status(&self) -> String {
        self.fallback.autostart_status()
    }
}

fn validation_error(
    kind: &str,
    loc: [&str; 2],
    msg: &str,
    input: serde_json::Value,
) -> BackendResponse {
    BackendResponse {
        status: 422,
        body: json!({"detail": [{"type": kind, "loc": loc, "msg": msg, "input": input}]}),
    }
}

/// The `file` part of a multipart/form-data body.
pub fn upload_file(content_type: &str, body: &[u8]) -> Option<Vec<u8>> {
    let boundary = multer::parse_boundary(content_type).ok()?;
    let chunk = multer::bytes::Bytes::copy_from_slice(body);
    let stream = futures_util::stream::once(std::future::ready(Ok::<_, std::io::Error>(chunk)));
    let mut multipart = multer::Multipart::new(stream, boundary);
    futures_executor::block_on(async {
        while let Ok(Some(field)) = multipart.next_field().await {
            if field.name() == Some("file") {
                return field.bytes().await.ok().map(|b| b.to_vec());
            }
        }
        None
    })
}

/// Decode any upload ffmpeg understands into 16 kHz mono f32 (`_decode_audio`).
pub fn ffmpeg_decode(data: &[u8], max_seconds: i64) -> Result<Vec<f32>, BackendResponse> {
    let spawned = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-protocol_whitelist",
            "pipe",
            "-format_whitelist",
        ])
        .arg(FFMPEG_INPUT_FORMATS)
        .args(["-i", "pipe:0", "-t"])
        .arg(max_seconds.max(1).to_string())
        .args(["-f", "f32le", "-ac", "1", "-ar", "16000", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(BackendResponse::error(
                500,
                "ffmpeg not installed (brew install ffmpeg)",
            ))
        }
        Err(e) => return Err(BackendResponse::error(500, e.to_string())),
    };
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = data.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + FFMPEG_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BackendResponse::error(400, "audio decode timed out"));
            }
        }
    };
    let _ = writer.join();
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if !status.success() || stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&stderr);
        let message = stderr
            .trim()
            .lines()
            .last()
            .unwrap_or("ffmpeg failed to decode audio");
        return Err(BackendResponse::error(
            400,
            message.chars().take(300).collect::<String>(),
        ));
    }
    Ok(stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}
