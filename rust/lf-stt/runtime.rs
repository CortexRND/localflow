//! Native adapters for lf-runtime's capture, speech and worker-factory seams.

use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use lf_audio::{Recorder, SAMPLE_RATE};
use lf_core::config::Config;
use lf_core::perf_ledger::PerfLedger;
use lf_runtime::engine::{AudioSource, Pcm, SpeechEngine};
use lf_runtime::run::RunOptions;
use lf_runtime::worker::SttFactory;

use crate::{factory, EngineSpec, SttSlot};

pub struct RecorderSource {
    recorder: Recorder,
    stop_error: Option<String>,
}

impl RecorderSource {
    pub fn new(device: Option<String>) -> Self {
        Self {
            recorder: Recorder::new(device),
            stop_error: None,
        }
    }

    pub fn from_config(config: &Config) -> Result<Self> {
        validate_sample_rate(config)?;
        Ok(Self::new(
            config.field_str("input_device").map(str::to_owned),
        ))
    }

    /// AudioSource cannot return stop errors. A failed stop drops the clip;
    /// callers may retrieve the error instead of mistaking it for silence.
    pub fn take_stop_error(&mut self) -> Option<String> {
        self.stop_error.take()
    }
}

impl AudioSource for RecorderSource {
    fn start(&mut self) -> Result<(), String> {
        self.stop_error = None;
        self.recorder.start().map_err(|e| e.to_string())
    }

    fn stop(&mut self) -> Pcm {
        match self.recorder.stop() {
            Ok(pcm) => pcm,
            Err(error) => {
                log::error!("microphone stop failed: {error}");
                self.stop_error = Some(error.to_string());
                Vec::new()
            }
        }
    }

    fn is_recording(&self) -> bool {
        self.recorder.recording()
    }
}

/// One frozen model snapshot, shared safely by runtime jobs and HTTP callers.
pub struct SpeechAdapter {
    slot: Mutex<SttSlot>,
}

/// Defers engine construction and weight loading until the first speech request.
pub struct LazySpeechAdapter {
    make: Box<dyn Fn() -> Result<SpeechAdapter, String> + Send + Sync>,
    engine: Mutex<Option<SpeechAdapter>>,
    loaded: std::sync::atomic::AtomicBool,
}

impl LazySpeechAdapter {
    /// For the dictation recorder: rejects a configured `sample_rate` other than 16 kHz.
    pub fn new(config: Config) -> Self {
        Self::with_factory(move || SpeechAdapter::from_config(&config))
    }

    /// For callers that resample to 16 kHz PCM themselves (e.g. meeting capture).
    /// The dictation `sample_rate` setting does not describe that audio, so it is
    /// not consulted.
    pub fn for_16khz_pcm(config: Config) -> Self {
        Self::with_factory(move || {
            factory::from_config(&config, None)
                .map(|spec| SpeechAdapter::from_config_spec(&config, spec))
        })
    }

    fn with_factory(make: impl Fn() -> Result<SpeechAdapter> + Send + Sync + 'static) -> Self {
        Self {
            make: Box::new(move || make().map_err(|e| e.to_string())),
            engine: Mutex::new(None),
            loaded: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn with_engine<T>(
        &self,
        use_engine: impl FnOnce(&SpeechAdapter) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut slot = self
            .engine
            .lock()
            .map_err(|_| "speech engine poisoned".to_string())?;
        if slot.is_none() {
            *slot = Some((self.make)()?);
        }
        let engine = slot.as_ref().unwrap();
        if !engine.loaded() {
            engine.load()?;
        }
        self.loaded
            .store(true, std::sync::atomic::Ordering::Release);
        use_engine(engine)
    }
}

impl SpeechEngine for LazySpeechAdapter {
    fn loaded(&self) -> bool {
        self.loaded.load(std::sync::atomic::Ordering::Acquire)
    }
    fn load(&self) -> Result<(), String> {
        self.with_engine(|_| Ok(()))
    }
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> Result<String, String> {
        self.with_engine(|engine| engine.transcribe(pcm, prompt))
    }
    fn unload(&self) {
        let mut engine = self.engine.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(engine) = engine.take() {
            engine.unload();
        }
        self.loaded
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

pub fn lazy_stt_factory() -> SttFactory {
    Arc::new(|config| Ok(Arc::new(LazySpeechAdapter::new(config.clone())) as Arc<dyn SpeechEngine>))
}

/// The opt-in native Hub owns the same-executable helper; other runtimes do not.
pub fn hub_speech(config: &Config) -> Arc<dyn SpeechEngine> {
    if cfg!(target_os = "macos")
        && std::env::var("LOCALFLOW_SPEECH_IN_PROCESS").as_deref() != Ok("1")
    {
        match std::env::current_exe() {
            Ok(exe) => Arc::new(crate::helper::HelperSpeechEngine::new(exe, config)),
            Err(error) => Arc::new(UnavailableSpeech(error.to_string())),
        }
    } else {
        Arc::new(LazySpeechAdapter::new(config.clone()))
    }
}

/// [`hub_speech`] for an already installed model only, for one-off work
/// such as retrying a saved clip. The configured model's files are checked
/// here first (no weights are loaded). On macOS the model still runs in the
/// same-executable helper, which is asked for a cache-only load: a helper
/// that doesn't know that request fails before loading anything instead of
/// falling back to an auto-downloading load. Elsewhere it runs in process
/// with [`SpeechAdapter::from_config_cached`]. Neither path downloads.
pub fn hub_speech_cached(config: &Config) -> Result<Arc<dyn SpeechEngine>, String> {
    let local = SpeechAdapter::from_config_cached(config).map_err(|e| format!("{e:#}"))?;
    if cfg!(target_os = "macos")
        && std::env::var("LOCALFLOW_SPEECH_IN_PROCESS").as_deref() != Ok("1")
    {
        drop(local);
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        Ok(Arc::new(crate::helper::HelperSpeechEngine::cached(
            exe, config,
        )))
    } else {
        Ok(Arc::new(local))
    }
}

struct UnavailableSpeech(String);
impl SpeechEngine for UnavailableSpeech {
    fn loaded(&self) -> bool {
        false
    }
    fn load(&self) -> Result<(), String> {
        Err(self.0.clone())
    }
    fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
        Err(self.0.clone())
    }
    fn unload(&self) {}
}

impl SpeechAdapter {
    pub fn from_config(config: &Config) -> Result<Self> {
        validate_sample_rate(config)?;
        Ok(Self::from_config_spec(
            config,
            factory::from_config(config, None)?,
        ))
    }

    /// A configured model: its loads go to the perf ledger (`stt_load`)
    /// unless `perf_metrics` is off.
    fn from_config_spec(config: &Config, spec: EngineSpec) -> Self {
        let slot = SttSlot::new().with_perf_ledger(PerfLedger::for_config(config));
        Self::with_slot(slot, spec)
    }

    /// The configured model from the user's HF cache only (see
    /// [`factory::from_config_cached`]); scoped leases as usual.
    pub fn from_config_cached(config: &Config) -> Result<Self> {
        Self::from_config_cached_at(config, &crate::model_storage::hf_hub_cache(None))
    }

    fn from_config_cached_at(config: &Config, cache_root: &std::path::Path) -> Result<Self> {
        validate_sample_rate(config)?;
        Ok(Self::from_config_spec(
            config,
            factory::from_config_cached(config, None, cache_root)?,
        ))
    }

    /// Injection seam for custom engines, including offline fakes.
    pub fn from_spec(spec: EngineSpec) -> Self {
        Self::with_slot(SttSlot::new(), spec)
    }

    fn with_slot(slot: SttSlot, spec: EngineSpec) -> Self {
        let mut slot = slot.with_scoped_leases();
        slot.select(spec);
        Self {
            slot: Mutex::new(slot),
        }
    }

    pub fn loaded(&self) -> bool {
        self.slot.lock().map(|slot| slot.loaded()).unwrap_or(false)
    }
}

impl SpeechEngine for SpeechAdapter {
    /// The slot's real state, so the runtime can preload a cold model. A busy
    /// slot (mid-load or mid-inference) is reported loaded instead of blocking.
    fn loaded(&self) -> bool {
        match self.slot.try_lock() {
            Ok(slot) => slot.loaded(),
            Err(std::sync::TryLockError::WouldBlock) => true,
            Err(std::sync::TryLockError::Poisoned(slot)) => slot.into_inner().loaded(),
        }
    }

    fn load(&self) -> Result<(), String> {
        self.slot
            .lock()
            .map_err(|_| "speech slot poisoned".to_string())?
            .load()
            .map_err(|e| e.to_string())
    }

    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> Result<String, String> {
        self.slot
            .lock()
            .map_err(|_| "speech slot poisoned".to_string())?
            .transcribe(pcm, prompt)
            .map(|result| result.text)
            .map_err(|e| e.to_string())
    }

    fn unload(&self) {
        // Even after an inference panic, unload native resources and their lease.
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).unload();
    }
}

pub fn stt_factory() -> SttFactory {
    Arc::new(|config| {
        SpeechAdapter::from_config(config)
            .map(|engine| Arc::new(engine) as Arc<dyn SpeechEngine>)
            .map_err(|e| e.to_string())
    })
}

/// Pass to lf_runtime::run::run. Construction neither opens the mic nor loads weights.
pub fn run_options(
    config: &Config,
    play_sound: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<RunOptions> {
    Ok(RunOptions {
        audio: Box::new(RecorderSource::from_config(config)?),
        speech: Arc::new(SpeechAdapter::from_config(config)?),
        build_stt: stt_factory(),
        play_sound,
    })
}

fn validate_sample_rate(config: &Config) -> Result<()> {
    let rate = config
        .fields
        .get("sample_rate")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(i64::from(SAMPLE_RATE));
    if rate != i64::from(SAMPLE_RATE) {
        bail!("native dictation requires sample_rate = {SAMPLE_RATE} (configured {rate})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{LoadInfo, Transcript};

    #[derive(Default)]
    struct Counts {
        loads: AtomicUsize,
        calls: AtomicUsize,
        unloads: AtomicUsize,
    }

    struct Fake {
        counts: Arc<Counts>,
        loaded: Cell<bool>,
        fail_load: bool,
    }

    impl crate::SpeechEngine for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn load(&mut self) -> Result<LoadInfo> {
            self.counts.loads.fetch_add(1, Ordering::SeqCst);
            self.loaded.set(true);
            if self.fail_load {
                bail!("load failed");
            }
            Ok(LoadInfo::default())
        }
        fn transcribe(&mut self, pcm: &[f32], prompt: Option<&str>) -> Result<Transcript> {
            assert!(self.loaded.get());
            self.counts.calls.fetch_add(1, Ordering::SeqCst);
            if prompt == Some("fail") {
                bail!("inference failed");
            }
            Ok(Transcript {
                text: if pcm.is_empty() {
                    String::new()
                } else {
                    prompt.unwrap_or("native").into()
                },
                ..Transcript::default()
            })
        }
        fn unload(&mut self) {
            self.counts.unloads.fetch_add(1, Ordering::SeqCst);
            self.loaded.set(false);
        }
        fn loaded(&self) -> bool {
            self.loaded.get()
        }
    }

    fn adapter(root: &std::path::Path, fail_load: bool) -> (Arc<SpeechAdapter>, Arc<Counts>) {
        let counts = Arc::new(Counts::default());
        let spec = EngineSpec {
            key: "fake".into(),
            backend: "whisper-cpp".into(),
            model_size: "base".into(),
            engine: Box::new(Fake {
                counts: counts.clone(),
                loaded: Cell::new(false),
                fail_load,
            }),
        };
        (
            Arc::new(SpeechAdapter::with_slot(
                SttSlot::with_lease_root(root.into()),
                spec,
            )),
            counts,
        )
    }

    fn lazy(root: &std::path::Path, fail_load: bool) -> (LazySpeechAdapter, Arc<Counts>) {
        let root = root.to_path_buf();
        let counts = Arc::new(Counts::default());
        let engine_counts = counts.clone();
        (
            LazySpeechAdapter {
                make: Box::new(move || {
                    Ok(SpeechAdapter::with_slot(
                        SttSlot::with_lease_root(root.clone()),
                        EngineSpec {
                            key: "fake".into(),
                            backend: "whisper-cpp".into(),
                            model_size: "base".into(),
                            engine: Box::new(Fake {
                                counts: engine_counts.clone(),
                                loaded: Cell::new(false),
                                fail_load,
                            }),
                        },
                    ))
                }),
                engine: Mutex::new(None),
                loaded: std::sync::atomic::AtomicBool::new(false),
            },
            counts,
        )
    }

    #[test]
    fn lazy_speech_defers_load_handles_empty_and_reuses_then_unloads_engine() {
        let root = tempfile::tempdir().unwrap();
        let (speech, counts) = lazy(root.path(), false);
        assert!(!speech.loaded());
        assert_eq!(counts.loads.load(Ordering::SeqCst), 0);
        assert_eq!(speech.transcribe(&[], None).unwrap(), "");
        assert!(speech.loaded());
        assert_eq!(speech.transcribe(&[0.5], None).unwrap(), "native");
        assert_eq!(counts.loads.load(Ordering::SeqCst), 1);
        speech.unload();
        assert!(!speech.loaded());
    }

    #[test]
    fn lazy_speech_load_failure_is_a_recoverable_request_error() {
        let root = tempfile::tempdir().unwrap();
        let (speech, counts) = lazy(root.path(), true);
        assert_eq!(counts.loads.load(Ordering::SeqCst), 0);
        assert!(speech
            .transcribe(&[0.5], None)
            .unwrap_err()
            .contains("load failed"));
        assert!(!speech.loaded());
        let mut invalid = Config::default();
        invalid.set_field("stt_backend", serde_json::json!("missing-provider"));
        let missing = LazySpeechAdapter::new(invalid);
        assert!(!missing.loaded());
        assert!(missing.load().is_err());
    }

    #[test]
    fn only_the_dictation_constructor_checks_the_dictation_sample_rate() {
        // An unknown provider fails in the factory before any model download,
        // so the error shows which check each constructor reached.
        let mut config = Config::default();
        config.set_field("sample_rate", serde_json::json!(22050));
        config.set_field("stt_backend", serde_json::json!("test-unknown"));
        let dictation = LazySpeechAdapter::new(config.clone()).load().unwrap_err();
        assert!(dictation.contains("sample_rate = 16000"), "{dictation}");
        assert_eq!(
            LazySpeechAdapter::for_16khz_pcm(config).load().unwrap_err(),
            "unsupported speech provider: test-unknown"
        );
    }

    #[test]
    fn load_prompt_empty_error_unload_reload_and_shared_calls() {
        let root = tempfile::tempdir().unwrap();
        let (speech, counts) = adapter(root.path(), false);
        assert!(!speech.loaded());
        speech.load().unwrap();
        speech.load().unwrap();
        assert!(speech.loaded());
        assert_eq!(counts.loads.load(Ordering::SeqCst), 1);
        assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
        assert_eq!(speech.transcribe(&[], None).unwrap(), "");
        assert_eq!(
            speech
                .transcribe(&[0.25], Some("dictionary prompt"))
                .unwrap(),
            "dictionary prompt"
        );
        assert_eq!(
            speech.transcribe(&[0.25], Some("fail")).unwrap_err(),
            "inference failed"
        );
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let speech = speech.clone();
                std::thread::spawn(move || speech.transcribe(&[0.25], None).unwrap())
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), "native");
        }
        speech.unload();
        assert!(!speech.loaded());
        assert!(crate::stt_lease::live_leases(Some(root.path()), |_| true).is_empty());
        assert_eq!(speech.transcribe(&[0.25], None).unwrap(), "native");
        assert_eq!(counts.loads.load(Ordering::SeqCst), 2);
        drop(speech);
        assert!(crate::stt_lease::live_leases(Some(root.path()), |_| true).is_empty());
        assert_eq!(Arc::strong_count(&counts), 1);
    }

    #[test]
    fn cold_adapter_reports_unloaded_through_the_trait_and_prepare_speech_loads_it_once() {
        use lf_runtime::worker::{WorkItem, Worker};
        let root = tempfile::tempdir().unwrap();
        let (adapter, counts) = adapter(root.path(), false);
        let speech: Arc<dyn SpeechEngine> = adapter.clone();
        assert!(
            !speech.loaded(),
            "a cold adapter must not claim to be loaded"
        );
        let worker = Worker {
            config: Arc::new(|| Arc::new(Config::default())),
            vocabulary: Arc::new(|_| Default::default()),
            history: None,
            speech: speech.clone(),
            build_llm: Arc::new(|_| None),
            build_stt: Arc::new(|_| unreachable!()),
            deliver: Arc::new(|_, _, _| {}),
            last_text_path: None,
            perf: None,
        };
        let (work, queue) = std::sync::mpsc::channel();
        work.send(WorkItem::PrepareSpeech).unwrap();
        work.send(WorkItem::PrepareSpeech).unwrap();
        drop(work);
        worker.run(queue);
        assert!(speech.loaded());
        assert_eq!(
            counts.loads.load(Ordering::SeqCst),
            1,
            "preload is idempotent"
        );
        assert_eq!(
            crate::stt_lease::live_leases(Some(root.path()), |_| true).len(),
            1
        );
        // Unload (idle or dictation-off) still releases the model and lease.
        speech.unload();
        assert!(!speech.loaded());
        assert!(crate::stt_lease::live_leases(Some(root.path()), |_| true).is_empty());
    }

    #[test]
    fn replacement_loads_before_old_unload_without_losing_either_lease() {
        let root = tempfile::tempdir().unwrap();
        let (old, _) = adapter(root.path(), false);
        old.load().unwrap();
        let (failed, counts) = adapter(root.path(), true);
        assert_eq!(failed.load().unwrap_err(), "load failed");
        assert!(!failed.loaded());
        assert_eq!(counts.unloads.load(Ordering::SeqCst), 1);
        assert!(old.loaded());
        assert_eq!(
            crate::stt_lease::live_leases(Some(root.path()), |_| true).len(),
            1
        );
        let (new, _) = adapter(root.path(), false);
        new.load().unwrap();
        assert_eq!(
            crate::stt_lease::live_leases(Some(root.path()), |_| true).len(),
            2
        );
        old.unload();
        drop(old);
        assert_eq!(
            crate::stt_lease::live_leases(Some(root.path()), |_| true).len(),
            1
        );
        assert_eq!(new.transcribe(&[0.25], None).unwrap(), "native");
        drop(new);
        drop(failed);
        assert!(crate::stt_lease::live_leases(Some(root.path()), |_| true).is_empty());
    }

    #[test]
    fn construction_is_idle_and_rejects_incompatible_rates_and_providers() {
        let mut config = Config::default();
        config
            .fields
            .insert("stt_backend".into(), serde_json::json!("whisper-cpp"));
        let mut audio = RecorderSource::from_config(&config).unwrap();
        assert!(!audio.is_recording());
        assert!(audio.stop().is_empty());
        assert!(audio.take_stop_error().is_none());
        if cfg!(feature = "whisper") {
            let options = run_options(&config, Arc::new(|_| {})).unwrap();
            assert!(!options.audio.is_recording());
            let speech = (options.build_stt)(&config).unwrap();
            speech.unload();
        }
        config
            .fields
            .insert("sample_rate".into(), serde_json::json!(48000));
        assert!(RecorderSource::from_config(&config).is_err());
        assert!(SpeechAdapter::from_config(&config).is_err());
        config
            .fields
            .insert("sample_rate".into(), serde_json::json!(16000));
        config
            .fields
            .insert("stt_backend".into(), serde_json::json!("plugin"));
        assert!(stt_factory()(&config).is_err());
    }

    #[test]
    fn cached_adapter_checks_rate_then_installed_files_without_loading() {
        let cache = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config
            .fields
            .insert("model_size".into(), serde_json::json!("tiny"));
        config
            .fields
            .insert("sample_rate".into(), serde_json::json!(44100));
        let error = SpeechAdapter::from_config_cached_at(&config, cache.path())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("sample_rate"), "{error}");
        config
            .fields
            .insert("sample_rate".into(), serde_json::json!(16000));
        assert!(SpeechAdapter::from_config_cached_at(&config, cache.path()).is_err());
        if cfg!(feature = "whisper") {
            crate::downloads::install_for_test(
                cache.path(),
                crate::model_storage::WHISPER_CPP_REPO,
                &[("ggml-tiny.bin", b"weights")],
            );
            let adapter = SpeechAdapter::from_config_cached_at(&config, cache.path()).unwrap();
            assert!(!adapter.loaded(), "construction loads no weights");
        }
    }

    #[cfg(feature = "server")]
    #[test]
    fn stacked_http_backends_use_outer_transcribe_and_delegate_other_routes() {
        use lf_runtime::server::DictationBackend;
        use lf_server::backend::{Backend, BackendRequest, BackendResponse};
        use serde_json::json;

        struct Fallback;
        impl Backend for Fallback {
            fn handle(&self, request: BackendRequest) -> BackendResponse {
                BackendResponse {
                    status: 200,
                    body: json!({"route":request.path}),
                }
            }
            fn stt_available(&self, _: &str) -> bool {
                false
            }
            fn transcriber_loaded(&self) -> bool {
                false
            }
            fn autostart_status(&self) -> String {
                "fallback".into()
            }
        }
        fn request(path: &str) -> BackendRequest {
            let mut config = Config::default();
            config.fields.insert("styles_enabled".into(), json!(false));
            let body = b"--test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"clip.wav\"\r\nContent-Type: audio/wav\r\n\r\ninvalid audio\r\n--test--\r\n".to_vec();
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
                "multipart/form-data; boundary=test".parse().unwrap(),
            );
            request
        }
        let root = tempfile::tempdir().unwrap();
        let (speech, counts) = adapter(root.path(), false);
        speech.load().unwrap();
        let make = || DictationBackend {
            speech: speech.clone(),
            vocabulary: Arc::new(|_| Default::default()),
            build_llm: Arc::new(|_| None),
            history: Arc::new(|_| None),
            decode: Arc::new(|_, _| Ok(vec![0.25; 1600])),
            fallback: Arc::new(Fallback),
        };
        let stt_outer = crate::server::SttBackend::new(make());
        assert_eq!(stt_outer.handle(request("/transcribe")).status, 400);
        assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            stt_outer.handle(request("/prompts")).body["route"],
            "/prompts"
        );
        assert_eq!(stt_outer.autostart_status(), "fallback");

        let mut runtime_outer = make();
        runtime_outer.fallback = Arc::new(crate::server::SttBackend::new(Fallback));
        let result = runtime_outer.handle(request("/transcribe"));
        assert_eq!(result.status, 200);
        assert_eq!(result.body["text"], "native");
        assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime_outer.handle(request("/prompts")).body["route"],
            "/prompts"
        );
        assert_eq!(runtime_outer.autostart_status(), "fallback");
        assert_eq!(
            runtime_outer.stt_available("faster-whisper"),
            cfg!(feature = "whisper")
        );
    }
}
