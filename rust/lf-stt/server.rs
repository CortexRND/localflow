//! Optional HTTP backend adapter. The host supplies authentication and a coherent
//! config snapshot; this adapter runs only on its blocking pool.
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::stream;
use lf_core::config::Config;
use lf_core::dictionary::{apply_replacements, stt_prompt, DictionaryStore};
use lf_core::formatting::{apply_spoken_formatting, apply_style};
use lf_core::history::{HistoryStore, NewEntry};
use lf_core::hotphrases::{render_dictation, HotPhraseStore, RenderSteps};
use lf_core::phrases_model::PhrasesSettings;
use lf_core::symbols::{apply_spoken_symbols, discover_commands};
use lf_providers::cleanup::Cleaner;
use lf_providers::factory::{build_llm, BuildOptions};
use lf_providers::store::CoreSecrets;
use lf_server::backend::{Backend, BackendRequest, BackendResponse};
use serde_json::{json, Value};
use wait_timeout::ChildExt;

use crate::{factory, EngineSpec, SttSlot};

const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
const INPUT_FORMATS: &str = "matroska,webm,ogg,mov,mp4,m4a,wav,mp3,aac,flac,caf";
type EngineFactory = dyn Fn(&Config) -> Result<EngineSpec> + Send + Sync;

pub struct SttBackend<B: Backend> {
    inner: B,
    slot: Mutex<SttSlot>,
    make_engine: Box<EngineFactory>,
    dictionary: DictionaryStore,
    phrases: HotPhraseStore,
    history_path: Option<PathBuf>,
}

impl<B: Backend> SttBackend<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            slot: Mutex::new(SttSlot::new()),
            make_engine: Box::new(|config| factory::from_config(config, None)),
            dictionary: DictionaryStore::new(None),
            phrases: HotPhraseStore::default_store(),
            history_path: None,
        }
    }

    fn record(&self, config: &Config, audio: &[f32], raw: &str, text: &str, status: &str) {
        let result = (|| -> Result<()> {
            let history = HistoryStore::open(
                self.history_path.clone(),
                None,
                flag(config, "history_enabled"),
            )?;
            let mut entry = NewEntry::new("dictate");
            entry.app_name = Some("phone".into());
            entry.category = Some("other".into());
            entry.duration_s = audio.len() as f64 / 16000.0;
            entry.raw_text = raw.into();
            entry.final_text = text.into();
            entry.status = status.into();
            history.add(&entry)?;
            Ok(())
        })();
        if result.is_err() {
            log::warn!("could not save server dictation history");
        }
    }

    fn transcribe(&self, config: &Config, audio: &[f32], clean: bool) -> Result<String> {
        let threshold = config
            .fields
            .get("silence_threshold_dbfs")
            .and_then(Value::as_f64)
            .unwrap_or(-50.0);
        if lf_audio::rms_dbfs(audio) < threshold {
            self.record(config, audio, "", "", "silent");
            return Ok(String::new());
        }
        let words = self.dictionary.words();
        let replacements = self.dictionary.replacements();
        let mut raw = String::new();
        let result = (|| -> Result<String> {
            {
                let mut slot = self
                    .slot
                    .lock()
                    .map_err(|_| anyhow::anyhow!("speech slot poisoned"))?;
                slot.select((self.make_engine)(config)?);
                raw = slot.transcribe(audio, stt_prompt(&words).as_deref())?.text;
            }
            if raw.is_empty() {
                return Ok(String::new());
            }
            let settings = PhrasesSettings::from_config(config);
            let phrases = if settings.hot_phrases {
                self.phrases.enabled()?
            } else {
                Vec::new()
            };
            let commands = discover_commands(&settings.command_names, &settings.skills_dirs);
            let style_name = settings
                .styles_enabled
                .then_some(settings.style_other.as_str())
                .filter(|s| !s.is_empty());
            let level = if flag(config, "cleanup_enabled") {
                config.field_str("cleanup_level").unwrap_or("none")
            } else {
                "none"
            };
            let replace = |text: &str| apply_replacements(text, &replacements);
            let symbols = |text: &str| apply_spoken_symbols(text, &commands);
            let style = |text: &str| apply_style(text, style_name.unwrap_or(""), &words);
            let clean_step =
                |text: &str| match build_llm(config, &BuildOptions::default(), &CoreSecrets) {
                    Ok(llm) => Cleaner::new(llm.as_ref()).clean(text, level, style_name, &words),
                    Err(_) => text.into(),
                };
            Ok(render_dictation(
                &raw,
                &phrases,
                &RenderSteps {
                    replace: Some(&replace),
                    fmt: settings
                        .spoken_punctuation
                        .then_some(&apply_spoken_formatting),
                    clean: (clean && level != "none").then_some(&clean_step),
                    symbols: settings.spoken_symbols.then_some(&symbols),
                    style: style_name.map(|_| &style as &dyn Fn(&str) -> String),
                },
            ))
        })();
        match &result {
            Ok(text) => self.record(
                config,
                audio,
                &raw,
                text,
                if text.is_empty() { "silent" } else { "ok" },
            ),
            Err(_) => self.record(config, audio, &raw, "", "failed"),
        }
        result
    }

    fn upload(&self, request: BackendRequest) -> BackendResponse {
        let start = Instant::now();
        if request.method != "POST" {
            return BackendResponse::error(405, "Method Not Allowed");
        }
        let clean = match request.query.get("clean") {
            None => false,
            Some(value) => match value.trim().parse::<i64>() {
                Ok(number) => number != 0,
                Err(_) => {
                    return BackendResponse {
                        status: 422,
                        body: json!({"detail": [{"type": "int_parsing", "loc": ["query", "clean"], "msg": "Input should be a valid integer, unable to parse string as an integer", "input": value}]}),
                    }
                }
            },
        };
        let data = match read_upload(&request) {
            Ok(data) => data,
            Err(response) => return response,
        };
        let seconds = request
            .config
            .fields
            .get("max_recording_seconds")
            .and_then(Value::as_i64)
            .unwrap_or(1200)
            .max(1);
        let audio = match decode_audio(&data, seconds) {
            Ok(audio) if audio.is_empty() => {
                return BackendResponse::error(400, "decoded audio is empty")
            }
            Ok(audio) => audio,
            Err(response) => return response,
        };
        match self.transcribe(&request.config, &audio, clean) {
            Ok(text) => BackendResponse {
                status: 200,
                body: json!({"text": text, "ms": start.elapsed().as_millis() as u64}),
            },
            Err(_) => BackendResponse::error(500, "Internal Server Error"),
        }
    }
}

impl<B: Backend> Backend for SttBackend<B> {
    fn handle(&self, request: BackendRequest) -> BackendResponse {
        if request.path == "/transcribe" {
            self.upload(request)
        } else {
            self.inner.handle(request)
        }
    }
    fn stt_available(&self, provider: &str) -> bool {
        matches!(factory::canonical_backend(provider), Ok("whisper-cpp") if cfg!(feature = "whisper"))
            || matches!(factory::canonical_backend(provider), Ok("parakeet-onnx") if cfg!(feature = "parakeet"))
    }
    fn transcriber_loaded(&self) -> bool {
        self.slot.lock().map(|slot| slot.loaded()).unwrap_or(false)
    }
    fn autostart_status(&self) -> String {
        self.inner.autostart_status()
    }
}

fn flag(config: &Config, name: &str) -> bool {
    config
        .fields
        .get(name)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn read_upload(request: &BackendRequest) -> std::result::Result<Vec<u8>, BackendResponse> {
    if request
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n > MAX_UPLOAD_BYTES)
    {
        return Err(BackendResponse::error(413, "upload too large"));
    }
    let missing = || BackendResponse {
        status: 422,
        body: json!({"detail": [{"type": "missing", "loc": ["body", "file"], "msg": "Field required", "input": null}]}),
    };
    let content_type = request
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("multipart/form-data") {
        return Err(missing());
    }
    let boundary = multer::parse_boundary(content_type)
        .map_err(|_| BackendResponse::error(400, "Missing boundary in multipart."))?;
    futures_executor::block_on(async {
        let body = request.body.clone();
        let input = stream::once(async move { Ok::<_, std::convert::Infallible>(body) });
        let mut multipart = multer::Multipart::new(input, boundary);
        let mut data = None;
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|_| BackendResponse::error(400, "There was an error parsing the body"))?
        {
            if field.name() == Some("file") {
                if field.file_name().is_none() {
                    let value = field.text().await.map_err(|_| {
                        BackendResponse::error(400, "There was an error parsing the body")
                    })?;
                    return Err(BackendResponse {
                        status: 422,
                        body: json!({"detail": [{"type": "value_error", "loc": ["body", "file"], "msg": "Value error, Expected UploadFile, received: <class 'str'>", "input": value, "ctx": {"error": {}}}]}),
                    });
                }
                let bytes = field.bytes().await.map_err(|_| {
                    BackendResponse::error(400, "There was an error parsing the body")
                })?;
                if bytes.len() > MAX_UPLOAD_BYTES {
                    return Err(BackendResponse::error(413, "upload too large"));
                }
                data = Some(bytes.to_vec());
            }
        }
        data.ok_or_else(missing)
    })
}

fn decode_audio(data: &[u8], seconds: i64) -> std::result::Result<Vec<f32>, BackendResponse> {
    let result = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-protocol_whitelist",
            "pipe",
            "-format_whitelist",
            INPUT_FORMATS,
            "-i",
            "pipe:0",
            "-t",
            &seconds.to_string(),
            "-f",
            "f32le",
            "-ac",
            "1",
            "-ar",
            "16000",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = result.map_err(|error| {
        BackendResponse::error(
            500,
            if error.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg not installed (brew install ffmpeg)"
            } else {
                "Internal Server Error"
            },
        )
    })?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let result = std::thread::scope(|scope| -> Result<_> {
        let writer = scope.spawn(|| {
            let _ = stdin.write_all(data);
            drop(stdin);
        });
        let output = scope.spawn(|| {
            let mut buffer = Vec::new();
            stdout.read_to_end(&mut buffer).map(|_| buffer)
        });
        let errors = scope.spawn(|| {
            let mut buffer = Vec::new();
            stderr.read_to_end(&mut buffer).map(|_| buffer)
        });
        let status = child.wait_timeout(Duration::from_secs(120));
        if !matches!(status, Ok(Some(_))) {
            let _ = child.kill();
            let _ = child.wait();
        }
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("audio writer failed"))?;
        let output = output
            .join()
            .map_err(|_| anyhow::anyhow!("audio reader failed"))??;
        let errors = errors
            .join()
            .map_err(|_| anyhow::anyhow!("audio reader failed"))??;
        Ok((status?, output, errors))
    })
    .map_err(|_| BackendResponse::error(500, "Internal Server Error"))?;
    let (status, output, errors) = result;
    let status = status.ok_or_else(|| BackendResponse::error(400, "audio decode timed out"))?;
    if !status.success() || output.is_empty() {
        let text = String::from_utf8_lossy(&errors);
        let message = text
            .trim()
            .lines()
            .last()
            .unwrap_or("ffmpeg failed to decode audio");
        return Err(BackendResponse::error(
            400,
            message.chars().take(300).collect::<String>(),
        ));
    }
    Ok(output
        .as_chunks::<4>()
        .0
        .iter()
        .map(|s| f32::from_le_bytes(*s))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LoadInfo, SpeechEngine, Timings, Transcript};
    use lf_server::backend::UnavailableBackend;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct FakeEngine {
        loads: Arc<AtomicUsize>,
        unloads: Arc<AtomicUsize>,
        loaded: bool,
        fail: bool,
        text: String,
    }
    impl SpeechEngine for FakeEngine {
        fn id(&self) -> &str {
            "whisper-cpp"
        }
        fn load(&mut self) -> Result<LoadInfo> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.loaded = true;
            Ok(LoadInfo::default())
        }
        fn unload(&mut self) {
            if self.loaded {
                self.unloads.fetch_add(1, Ordering::SeqCst);
            }
            self.loaded = false;
        }
        fn loaded(&self) -> bool {
            self.loaded
        }
        fn transcribe(&mut self, _: &[f32], prompt: Option<&str>) -> Result<Transcript> {
            assert!(prompt.is_some_and(|s| s.contains("LocalFlow")));
            anyhow::ensure!(!self.fail, "fake inference error");
            Ok(Transcript {
                text: self.text.clone(),
                timings: Timings::default(),
            })
        }
    }
    struct Inner;
    impl Backend for Inner {
        fn handle(&self, request: BackendRequest) -> BackendResponse {
            BackendResponse {
                status: 202,
                body: json!({"path":request.path,"query":request.query,"body":request.body.as_ref()}),
            }
        }
        fn autostart_status(&self) -> String {
            "enabled".into()
        }
        fn transcriber_loaded(&self) -> bool {
            true
        }
    }

    fn isolated(
        root: &std::path::Path,
        fail: bool,
        text: &str,
    ) -> (SttBackend<Inner>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let mut backend = SttBackend::new(Inner);
        backend.slot = Mutex::new(SttSlot::with_lease_root(root.join("leases")));
        backend.dictionary = DictionaryStore::new(Some(root.join("dictionary.json")));
        backend.dictionary.add_word("LocalFlow").unwrap();
        backend
            .dictionary
            .add_replacement("local flow", "LocalFlow")
            .unwrap();
        backend.phrases = HotPhraseStore::new(root.join("phrases.json"));
        backend.history_path = Some(root.join("history.sqlite3"));
        let loads = Arc::new(AtomicUsize::new(0));
        let unloads = Arc::new(AtomicUsize::new(0));
        let (l, u) = (loads.clone(), unloads.clone());
        let text = text.to_string();
        backend.make_engine = Box::new(move |config| {
            let model = config.field_str("model_size").unwrap_or("base").to_string();
            Ok(EngineSpec {
                key: model.clone(),
                backend: "whisper-cpp".into(),
                model_size: model,
                engine: Box::new(FakeEngine {
                    loads: l.clone(),
                    unloads: u.clone(),
                    loaded: false,
                    fail,
                    text: text.clone(),
                }),
            })
        });
        (backend, loads, unloads)
    }

    fn request(path: &str, data: &[u8]) -> BackendRequest {
        let mut config = Config::default();
        config.fields.insert("styles_enabled".into(), json!(false));
        config.fields.insert("skills_dirs".into(), json!([]));
        let mut body = b"--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"clip.wav\"\r\nContent-Type: audio/wav\r\n\r\n".to_vec();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n--boundary--\r\n");
        let mut request = BackendRequest {
            method: "POST".parse().unwrap(),
            path: path.into(),
            query: Default::default(),
            headers: Default::default(),
            body: body.into(),
            config,
        };
        request.headers.insert(
            "content-type",
            "multipart/form-data; boundary=boundary".parse().unwrap(),
        );
        request
    }

    fn wav(rate: u32, channels: u16, amplitude: i16) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(
                &mut buffer,
                hound::WavSpec {
                    channels,
                    sample_rate: rate,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            for _ in 0..rate / 10 * channels as u32 {
                writer.write_sample(amplitude).unwrap();
            }
            writer.finalize().unwrap();
        }
        buffer.into_inner()
    }

    #[test]
    fn delegation_and_availability_respect_compiled_features() {
        let backend = SttBackend::new(Inner);
        for path in [
            "/meeting/status",
            "/prompts",
            "/api/recall",
            "/api/autostart",
            "/other",
        ] {
            let result = backend.handle(request(path, b"unchanged"));
            assert_eq!(result.status, 202);
            assert_eq!(result.body["path"], path);
        }
        assert_eq!(backend.autostart_status(), "enabled");
        assert!(!backend.transcriber_loaded());
        for provider in [
            "auto",
            "mlx",
            "mlx-whisper",
            "faster-whisper",
            "whisper-cpp",
        ] {
            assert_eq!(backend.stt_available(provider), cfg!(feature = "whisper"));
        }
        for provider in ["parakeet", "parakeet-onnx"] {
            assert_eq!(backend.stt_available(provider), cfg!(feature = "parakeet"));
        }
        assert!(!backend.stt_available("plugin"));
        let _ = SttBackend::new(UnavailableBackend);
    }

    #[test]
    fn meetings_backend_stacks_in_both_orders_without_claiming_transcribe() {
        use lf_meetings::dispatch::PromptQueue;
        use lf_meetings_backend::MeetingsBackend;

        fn prompts() -> BackendRequest {
            let mut request = request("/prompts", b"");
            request.method = "GET".parse().unwrap();
            request
        }
        let root = tempfile::tempdir().unwrap();
        let (template, loads, _) = isolated(root.path(), false, "stt outer");
        let SttBackend {
            slot,
            make_engine,
            dictionary,
            phrases,
            history_path,
            ..
        } = template;
        let outer = SttBackend {
            inner: MeetingsBackend::new()
                .unwrap()
                .with_queue(PromptQueue::new(root.path().join("queue.json")))
                .with_inner(Arc::new(Inner)),
            slot,
            make_engine,
            dictionary,
            phrases,
            history_path,
        };
        assert_eq!(outer.handle(prompts()).body, json!({"entries":[]}));
        assert_eq!(outer.handle(request("/other", b"unchanged")).status, 202);
        assert_eq!(
            outer
                .handle(request("/transcribe", &wav(16000, 1, 16000)))
                .body["text"],
            "stt outer"
        );
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(outer.transcriber_loaded());
        assert_eq!(outer.autostart_status(), "enabled");

        let root = tempfile::tempdir().unwrap();
        let (stt, loads, _) = isolated(root.path(), false, "stt inner");
        let outer = MeetingsBackend::new()
            .unwrap()
            .with_queue(PromptQueue::new(root.path().join("queue.json")))
            .with_inner(Arc::new(stt));
        assert_eq!(outer.handle(prompts()).body, json!({"entries":[]}));
        assert_eq!(outer.handle(request("/other", b"unchanged")).status, 202);
        assert!(!outer.transcriber_loaded());
        assert_eq!(
            outer
                .handle(request("/transcribe", &wav(16000, 1, 16000)))
                .body["text"],
            "stt inner"
        );
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(outer.transcriber_loaded());
        assert_eq!(outer.autostart_status(), "enabled");
        assert_eq!(
            outer.stt_available("faster-whisper"),
            cfg!(feature = "whisper")
        );
    }

    #[test]
    fn silence_normal_history_model_switch_and_drop() {
        let root = tempfile::tempdir().unwrap();
        let (backend, loads, unloads) = isolated(root.path(), false, "hello local flow comma.");
        let silent = backend.handle(request("/transcribe", &wav(48000, 2, 0)));
        assert_eq!(silent.status, 200);
        assert_eq!(silent.body["text"], "");
        assert!(silent.body["ms"].is_u64());
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        let history = HistoryStore::open(backend.history_path.clone(), None, true).unwrap();
        assert_eq!(history.recent(1, None, 0).unwrap()[0].status, "silent");
        let normal = backend.handle(request("/transcribe", &wav(48000, 2, 16000)));
        assert_eq!(normal.status, 200);
        assert_eq!(normal.body["text"], "hello LocalFlow,");
        assert!(backend.transcriber_loaded());
        assert_eq!(
            history.recent(1, None, 0).unwrap()[0].raw_text,
            "hello local flow comma."
        );
        assert_eq!(
            history.recent(1, None, 0).unwrap()[0].app_name.as_deref(),
            Some("phone")
        );
        let mut next = request("/transcribe", &wav(16000, 1, 16000));
        next.config
            .fields
            .insert("model_size".into(), json!("small"));
        assert_eq!(backend.handle(next).status, 200);
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        assert_eq!(unloads.load(Ordering::SeqCst), 1);
        drop(backend);
        assert_eq!(unloads.load(Ordering::SeqCst), 2);
        assert!(
            crate::stt_lease::live_leases(Some(&root.path().join("leases")), |_| true).is_empty()
        );
    }

    #[test]
    fn failed_inference_and_empty_transcript_are_recorded() {
        for (fail, expected_status) in [(false, "silent"), (true, "failed")] {
            let root = tempfile::tempdir().unwrap();
            let (backend, _, _) = isolated(root.path(), fail, "");
            let response = backend.handle(request("/transcribe", &wav(16000, 1, 16000)));
            assert_eq!(response.status, if fail { 500 } else { 200 });
            if fail {
                assert_eq!(response.body, json!({"detail":"Internal Server Error"}));
            }
            let history = HistoryStore::open(backend.history_path.clone(), None, true).unwrap();
            assert_eq!(
                history.recent(1, None, 0).unwrap()[0].status,
                expected_status
            );
        }
    }

    #[test]
    fn postprocessing_preserves_phrases_and_applies_style_and_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let (backend, _, _) = isolated(root.path(), false, "trigger");
        backend
            .phrases
            .add("trigger", "VERBATIM\nlocal flow", true)
            .unwrap();
        let mut config = Config::default();
        config.fields.insert("skills_dirs".into(), json!([]));
        assert_eq!(
            backend.transcribe(&config, &[0.25; 1600], true).unwrap(),
            "VERBATIM\nlocal flow"
        );

        let root = tempfile::tempdir().unwrap();
        let (backend, _, _) = isolated(root.path(), false, "hello local flow period.");
        config.fields.insert("style_other".into(), json!("casual"));
        assert_eq!(
            backend.transcribe(&config, &[0.25; 1600], false).unwrap(),
            "hello LocalFlow"
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|n| n.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            socket.read_exact(&mut body).unwrap();
            let response = r#"{"response":"cleaned local flow"}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            tx.send(serde_json::from_slice::<Value>(&body).unwrap())
                .unwrap();
        });
        config.fields.insert("llm_provider".into(), json!("ollama"));
        config
            .fields
            .insert("llm_base_url".into(), json!(format!("http://{address}")));
        config.fields.insert("styles_enabled".into(), json!(false));
        config.fields.insert("cleanup_timeout".into(), json!(5));
        assert_eq!(
            backend.transcribe(&config, &[0.25; 1600], true).unwrap(),
            "cleaned local flow"
        );
        let body = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(body["prompt"].as_str().unwrap().contains("LocalFlow"));
        assert!(body["prompt"]
            .as_str()
            .unwrap()
            .contains("hello LocalFlow."));
        server.join().unwrap();
    }

    #[test]
    fn upload_validation_decode_errors_and_sanitized_ffmpeg_invocation() {
        let backend = SttBackend::new(Inner);
        let bad = backend.handle(request("/transcribe", b"invalid audio"));
        assert_eq!(bad.status, 400);
        assert!(bad.body["detail"]
            .as_str()
            .unwrap()
            .contains("Invalid data found when processing input"));
        let mut missing = request("/transcribe", b"");
        missing.body = "--boundary--\r\n".into();
        let response = backend.handle(missing);
        assert_eq!(response.status, 422);
        assert_eq!(response.body["detail"][0]["loc"], json!(["body", "file"]));
        let mut large = request("/transcribe", b"");
        large.headers.insert(
            "content-length",
            (MAX_UPLOAD_BYTES + 1).to_string().parse().unwrap(),
        );
        assert_eq!(
            backend.handle(large).body,
            json!({"detail":"upload too large"})
        );
        let mut invalid = request("/transcribe", b"");
        invalid.query.insert("clean".into(), "bad".into());
        assert_eq!(backend.handle(invalid).status, 422);
        let mut boundary = request("/transcribe", b"");
        boundary
            .headers
            .insert("content-type", "multipart/form-data".parse().unwrap());
        assert_eq!(
            backend.handle(boundary).body,
            json!({"detail":"Missing boundary in multipart."})
        );
        assert!(decode_audio(b"#EXTM3U\nfile:///etc/passwd", 1).is_err());
        let samples = match decode_audio(&wav(48000, 1, 16000), 1) {
            Ok(samples) => samples,
            Err(_) => panic!("WAV decoding failed"),
        };
        assert_eq!(samples.len(), 1600);
        assert!((samples[0] - 16000.0 / 32768.0).abs() < 0.001);
    }
}
