//! Worker-only setup services, with portable injection seams.
use crate::setup_flow::SttModel;
use lf_providers::{ollama::OllamaLlm, transfer, LlmProvider};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A cancellable download progress tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadProgress {
    pub done: u64,
    pub total: u64,
    pub status: String,
}

pub type ProgressCallback = Box<dyn Fn(DownloadProgress) + Send + Sync>;

/// Optional model/runtime operations. They are separate from checks/apply
/// because `lf-ui` does not itself link a speech runtime or model downloader.
pub trait SetupModelBackend: Send + Sync {
    fn speech_format(&self, _model: &SttModel) -> Option<&'static str> {
        None
    }
    fn speech_detail(&self, _model: &SttModel) -> Option<String> {
        None
    }
    fn try_dictation_cancellable(
        &self,
        model: &SttModel,
        device: Option<&str>,
        language: Option<&str>,
        cancel: Arc<AtomicBool>,
    ) -> Result<String, String> {
        lf_providers::transfer::check_cancel(&cancel)?;
        let result = self.try_dictation(model, device, language);
        lf_providers::transfer::check_cancel(&cancel)?;
        result
    }

    fn speech_downloads(
        &self,
        models: &[SttModel],
    ) -> std::result::Result<HashMap<String, bool>, String>;

    fn download_speech(
        &self,
        model: &SttModel,
        progress: ProgressCallback,
        cancel: Arc<AtomicBool>,
    ) -> std::result::Result<(), String>;

    fn ollama(&self, base_url: &str) -> std::result::Result<(bool, Vec<String>), String>;

    fn pull_cleanup(
        &self,
        base_url: &str,
        model_id: &str,
        progress: ProgressCallback,
        cancel: Arc<AtomicBool>,
    ) -> std::result::Result<(), String>;

    fn try_dictation(
        &self,
        model: &SttModel,
        device: Option<&str>,
        language: Option<&str>,
    ) -> std::result::Result<String, String>;
}

#[derive(Default)]
pub struct FlowModelBackend {
    details: Mutex<HashMap<String, String>>,
}
impl FlowModelBackend {
    #[cfg(feature = "runtime")]
    fn inspect_speech_at(
        &self,
        models: &[SttModel],
        root: &std::path::Path,
    ) -> HashMap<String, bool> {
        let mut details = HashMap::new();
        let installed = models
            .iter()
            .map(|model| {
                match lf_stt::setup_downloads::native_model(model.backend, model.model_size) {
                    Ok(native) => {
                        let info = native.inspect(root);
                        if info.installed {
                            details.insert(model.id.clone(), byte_size(info.bytes));
                        }
                        (model.id.clone(), info.installed)
                    }
                    Err(_) => {
                        details.insert(model.id.clone(), "Unavailable".into());
                        (model.id.clone(), false)
                    }
                }
            })
            .collect();
        *self.details.lock().unwrap_or_else(|e| e.into_inner()) = details;
        installed
    }
}
impl SetupModelBackend for FlowModelBackend {
    fn speech_format(&self, model: &SttModel) -> Option<&'static str> {
        Some(if cfg!(feature = "runtime") {
            if model.backend == "parakeet" {
                "ONNX"
            } else {
                "GGML"
            }
        } else {
            "Not built"
        })
    }
    fn speech_detail(&self, model: &SttModel) -> Option<String> {
        let detail = self
            .details
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&model.id)
            .cloned();
        detail.or_else(|| {
            if cfg!(feature = "runtime") {
                None
            } else {
                Some("Not built".into())
            }
        })
    }
    fn speech_downloads(&self, models: &[SttModel]) -> Result<HashMap<String, bool>, String> {
        #[cfg(not(feature = "runtime"))]
        {
            let _ = models;
            Err("Speech inspection requires the runtime feature.".into())
        }
        #[cfg(feature = "runtime")]
        {
            let root = lf_stt::model_storage::hf_hub_cache(None);
            Ok(self.inspect_speech_at(models, &root))
        }
    }
    fn download_speech(
        &self,
        model: &SttModel,
        progress: ProgressCallback,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), String> {
        #[cfg(not(feature = "runtime"))]
        {
            let _ = (model, progress, cancel);
            Err("Speech downloads require the runtime feature.".into())
        }
        #[cfg(feature = "runtime")]
        {
            let native = lf_stt::setup_downloads::native_model(model.backend, model.model_size)?;
            let root = lf_stt::model_storage::hf_hub_cache(None);
            let endpoint =
                std::env::var("HF_ENDPOINT").unwrap_or_else(|_| "https://huggingface.co".into());
            native.download(
                &root,
                &endpoint,
                &|done, total| {
                    progress(DownloadProgress {
                        done,
                        total,
                        status: format!("Downloading {} weights", native.format),
                    })
                },
                &cancel,
            )?;
            let info = native.inspect(&root);
            if !info.installed {
                return Err("Native model download is incomplete.".into());
            }
            self.details
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(model.id.clone(), byte_size(info.bytes));
            Ok(())
        }
    }
    fn ollama(&self, base_url: &str) -> Result<(bool, Vec<String>), String> {
        let mut models: Vec<_> = OllamaLlm::new(base_url, "", 3, 8192)
            .list_models()
            .map_err(|e| format!("Ollama not running or unavailable: {e}"))?
            .into_iter()
            .map(|m| m.id)
            .filter(|m| !m.is_empty())
            .collect();
        models.sort();
        Ok((true, models))
    }
    fn pull_cleanup(
        &self,
        base_url: &str,
        model_id: &str,
        progress: ProgressCallback,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), String> {
        OllamaLlm::new(base_url, model_id, 120, 8192)
            .pull(
                &|done, total, status| {
                    progress(DownloadProgress {
                        done,
                        total,
                        status: status.into(),
                    })
                },
                &cancel,
            )
            .map_err(|e| e.to_string())
    }
    fn try_dictation(
        &self,
        model: &SttModel,
        device: Option<&str>,
        language: Option<&str>,
    ) -> Result<String, String> {
        self.try_dictation_cancellable(model, device, language, Arc::new(AtomicBool::new(false)))
    }
    fn try_dictation_cancellable(
        &self,
        model: &SttModel,
        device: Option<&str>,
        language: Option<&str>,
        cancel: Arc<AtomicBool>,
    ) -> Result<String, String> {
        #[cfg(not(feature = "runtime"))]
        {
            let _ = (model, device, language, cancel);
            Err("Dictation trial requires the runtime feature.".into())
        }
        #[cfg(feature = "runtime")]
        {
            let native = lf_stt::setup_downloads::native_model(model.backend, model.model_size)?;
            if !native
                .inspect(&lf_stt::model_storage::hf_hub_cache(None))
                .installed
            {
                return Err(format!(
                    "Download the native {} weights before recording.",
                    native.format
                ));
            }
            let mut config = lf_core::config::Config::default();
            config.set_field("stt_backend", serde_json::json!(model.backend));
            config.set_field("model_size", serde_json::json!(model.model_size));
            config.set_field("language", serde_json::json!(language));
            let engine = lf_stt::runtime::hub_speech(&config);
            let mut audio = lf_audio::Recorder::new(device.map(str::to_owned));
            run_trial(&mut audio, engine.as_ref(), &cancel, Duration::from_secs(5))
        }
    }
}
#[cfg(feature = "runtime")]
fn byte_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
    } else {
        format!("{} MB", bytes / 1_000_000)
    }
}

pub trait TrialAudio {
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self) -> Result<Vec<f32>, String>;
}
impl TrialAudio for lf_audio::Recorder {
    fn start(&mut self) -> Result<(), String> {
        lf_audio::Recorder::start(self).map_err(|e| e.to_string())
    }
    fn stop(&mut self) -> Result<Vec<f32>, String> {
        lf_audio::Recorder::stop(self).map_err(|e| e.to_string())
    }
}
struct Capture<'a> {
    audio: &'a mut dyn TrialAudio,
    stopped: bool,
}
impl Drop for Capture<'_> {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.audio.stop();
        }
    }
}
pub fn run_trial(
    audio: &mut dyn TrialAudio,
    engine: &dyn lf_runtime::engine::SpeechEngine,
    cancel: &AtomicBool,
    duration: Duration,
) -> Result<String, String> {
    transfer::check_cancel(cancel)?;
    engine.load()?;
    transfer::check_cancel(cancel)?;
    let mut capture = Capture {
        audio,
        stopped: false,
    };
    capture.audio.start()?;
    transfer::check_cancel(cancel)?;
    let started = Instant::now();
    while started.elapsed() < duration {
        transfer::check_cancel(cancel)?;
        std::thread::park_timeout(
            duration
                .saturating_sub(started.elapsed())
                .min(Duration::from_millis(20)),
        );
    }
    capture.stopped = true;
    let pcm = capture.audio.stop()?;
    transfer::check_cancel(cancel)?;
    if pcm.is_empty() {
        return Err("No audio was captured.".into());
    }
    let result = engine.transcribe(&pcm, None)?;
    transfer::check_cancel(cancel)?;
    if result.trim().is_empty() {
        return Err("No speech was recognized.".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::Ordering, mpsc};
    #[derive(Default)]
    struct Counts {
        starts: usize,
        stops: usize,
        active: bool,
    }
    struct Audio {
        counts: Arc<Mutex<Counts>>,
        pcm: Vec<f32>,
        error: Option<&'static str>,
        cancel: Option<Arc<AtomicBool>>,
        entered: Option<mpsc::Sender<()>>,
    }
    impl Audio {
        fn new() -> Self {
            Self {
                counts: Arc::default(),
                pcm: vec![0.25; 160],
                error: None,
                cancel: None,
                entered: None,
            }
        }
    }
    impl TrialAudio for Audio {
        fn start(&mut self) -> Result<(), String> {
            {
                let mut counts = self.counts.lock().unwrap();
                counts.starts += 1;
                counts.active = true;
            }
            if let Some(cancel) = &self.cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
            }
            if self.error == Some("open failed") {
                return Err("open failed".into());
            }
            Ok(())
        }
        fn stop(&mut self) -> Result<Vec<f32>, String> {
            let mut counts = self.counts.lock().unwrap();
            counts.stops += 1;
            counts.active = false;
            if self.error == Some("stop failed") {
                return Err("stop failed".into());
            }
            Ok(std::mem::take(&mut self.pcm))
        }
    }
    struct Engine {
        counts: Arc<Mutex<Counts>>,
        seen: Mutex<Vec<f32>>,
        error: Option<&'static str>,
        text: &'static str,
        cancel_on_load: Option<Arc<AtomicBool>>,
    }
    impl Engine {
        fn new(audio: &Audio) -> Self {
            Self {
                counts: audio.counts.clone(),
                seen: Mutex::default(),
                error: None,
                text: "real trial text",
                cancel_on_load: None,
            }
        }
    }
    impl lf_runtime::engine::SpeechEngine for Engine {
        fn load(&self) -> Result<(), String> {
            if let Some(cancel) = &self.cancel_on_load {
                cancel.store(true, Ordering::Release);
            }
            if self.error == Some("load failed") {
                return Err("load failed".into());
            }
            Ok(())
        }
        fn transcribe(&self, pcm: &[f32], _: Option<&str>) -> Result<String, String> {
            assert!(
                !self.counts.lock().unwrap().active,
                "mic must close before inference"
            );
            *self.seen.lock().unwrap() = pcm.to_vec();
            if let Some(error) = self.error {
                return Err(error.into());
            }
            Ok(self.text.into())
        }
        fn unload(&self) {}
    }

    #[test]
    fn trial_records_16khz_pcm_closes_mic_then_displays_engine_text() {
        let mut audio = Audio::new();
        let engine = Engine::new(&audio);
        assert_eq!(
            run_trial(&mut audio, &engine, &AtomicBool::new(false), Duration::ZERO).unwrap(),
            "real trial text"
        );
        assert_eq!(*engine.seen.lock().unwrap(), vec![0.25; 160]);
        let counts = audio.counts.lock().unwrap();
        assert_eq!((counts.starts, counts.stops, counts.active), (1, 1, false));
    }
    #[test]
    fn trial_never_reports_success_for_empty_or_failed_capture_or_engine() {
        for error in [
            "open failed",
            "stop failed",
            "load failed",
            "decode failed",
            "empty audio",
            "empty speech",
        ] {
            let mut audio = Audio::new();
            let mut engine = Engine::new(&audio);
            match error {
                "open failed" | "stop failed" => audio.error = Some(error),
                "load failed" | "decode failed" => engine.error = Some(error),
                "empty audio" => audio.pcm.clear(),
                "empty speech" => engine.text = "",
                _ => unreachable!(),
            }
            let result = run_trial(&mut audio, &engine, &AtomicBool::new(false), Duration::ZERO)
                .unwrap_err();
            assert!(!result.is_empty());
            let counts = audio.counts.lock().unwrap();
            assert!(!counts.active);
            assert_eq!(counts.stops, usize::from(error != "load failed"));
        }
    }
    #[test]
    fn trial_cancel_is_sticky_before_start_after_loading_and_during_open() {
        for stage in ["before", "load", "open"] {
            let mut audio = Audio::new();
            let mut engine = Engine::new(&audio);
            let cancel = Arc::new(AtomicBool::new(stage == "before"));
            if stage == "load" {
                engine.cancel_on_load = Some(cancel.clone());
            }
            if stage == "open" {
                audio.cancel = Some(cancel.clone());
            }
            assert!(
                run_trial(&mut audio, &engine, &cancel, Duration::from_secs(60))
                    .unwrap_err()
                    .contains("cancelled")
            );
            let counts = audio.counts.lock().unwrap();
            assert!(!counts.active);
            assert_eq!(counts.starts, usize::from(stage == "open"));
            assert_eq!(counts.stops, counts.starts);
            assert!(engine.seen.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn trial_cancel_during_capture_releases_mic_without_waiting_five_seconds() {
        let mut audio = Audio::new();
        let engine = Engine::new(&audio);
        let counts = audio.counts.clone();
        let (entered, ready) = mpsc::channel();
        audio.entered = Some(entered);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let (reply, done) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            reply
                .send(run_trial(
                    &mut audio,
                    &engine,
                    &stop,
                    Duration::from_secs(60),
                ))
                .unwrap()
        });
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        cancel.store(true, Ordering::Release);
        assert!(done
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err()
            .contains("cancelled"));
        worker.join().unwrap();
        let counts = counts.lock().unwrap();
        assert_eq!((counts.starts, counts.stops, counts.active), (1, 1, false));
    }
    #[test]
    fn ollama_list_is_real_sorted_empty_or_a_visible_error() {
        use std::io::{BufRead, Write};
        for body in [
            r#"{"models":[{"name":"z"},{"name":"a"}]}"#,
            r#"{"models":[]}"#,
            "bad JSON",
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(stream);
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });
            let result = FlowModelBackend::default().ollama(&url);
            server.join().unwrap();
            match body {
                "bad JSON" => assert!(result.is_err()),
                s if s.contains("\"z\"") => {
                    assert_eq!(result.unwrap(), (true, vec!["a".into(), "z".into()]))
                }
                _ => assert_eq!(result.unwrap(), (true, vec![])),
            }
        }
        assert!(FlowModelBackend::default()
            .ollama("http://127.0.0.1:0")
            .unwrap_err()
            .contains("Ollama"));
    }
    #[test]
    fn native_format_is_labeled_without_io() {
        let backend = FlowModelBackend::default();
        let model = crate::setup_flow::stt_model_by_id("fw-base").unwrap();
        assert_eq!(
            backend.speech_format(&model),
            Some(if cfg!(feature = "runtime") {
                "GGML"
            } else {
                "Not built"
            })
        );
        assert_eq!(
            backend.speech_detail(&model),
            if cfg!(feature = "runtime") {
                None
            } else {
                Some("Not built".into())
            }
        );
    }
    #[cfg(feature = "runtime")]
    #[test]
    fn missing_speech_uses_catalog_size_and_inspection_clears_stale_installed_size() {
        let backend = FlowModelBackend::default();
        let model = crate::setup_flow::stt_model_by_id("fw-base").unwrap();
        let models = std::slice::from_ref(&model);
        let root = tempfile::tempdir().unwrap();
        assert!(model.size_mb > 0);
        assert_eq!(backend.speech_detail(&model), None);
        assert!(!backend.inspect_speech_at(models, root.path())[&model.id]);
        assert_eq!(backend.speech_detail(&model), None);

        let native =
            lf_stt::setup_downloads::native_model(model.backend, model.model_size).unwrap();
        let repo = root
            .path()
            .join(format!("models--{}", native.repo.replace('/', "--")));
        let snapshot = repo.join("snapshots").join("a".repeat(40));
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::create_dir_all(repo.join("refs")).unwrap();
        std::fs::write(repo.join("refs/main"), "a".repeat(40)).unwrap();
        let weights = snapshot.join(&native.files[0]);
        std::fs::File::create(&weights)
            .unwrap()
            .set_len(2_000_000)
            .unwrap();
        assert!(backend.inspect_speech_at(models, root.path())[&model.id]);
        assert_eq!(backend.speech_detail(&model), Some("2 MB".into()));

        std::fs::remove_file(weights).unwrap();
        assert!(!backend.inspect_speech_at(models, root.path())[&model.id]);
        assert_eq!(backend.speech_detail(&model), None);
    }
    #[cfg(not(feature = "runtime"))]
    #[test]
    fn runtime_missing_is_explicit_for_inspection_download_and_trial() {
        let backend = FlowModelBackend::default();
        let model = crate::setup_flow::stt_model_by_id("fw-base").unwrap();
        assert!(backend
            .speech_downloads(std::slice::from_ref(&model))
            .unwrap_err()
            .contains("runtime feature"));
        assert!(backend
            .download_speech(
                &model,
                Box::new(|_| panic!("unsupported")),
                Arc::new(AtomicBool::new(false))
            )
            .unwrap_err()
            .contains("runtime feature"));
        assert!(backend
            .try_dictation(&model, None, None)
            .unwrap_err()
            .contains("runtime feature"));
    }
}
