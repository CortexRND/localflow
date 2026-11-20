//! Dictation lifecycle for standalone and application-owned event loops.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
    Arc, Mutex, Weak,
};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Result};
use lf_core::{
    config::Config, dictionary::DictionaryStore, history::HistoryStore, hotphrases::HotPhraseStore,
    symbols::discover_commands,
};
use lf_input::{hub::KeyboardHub, paste::Injector, InputError, InputWarning};
use lf_providers::{
    factory::{build_llm, BuildOptions},
    CoreSecrets,
};

use crate::app::{hub_actions, job_sink, keyboard_bindings, work_queue, LiveSettings};
use crate::controller::{Controller, ControllerDeps, ControllerHandle, MIN_CLIP_SECONDS};
use crate::engine::{AudioSource, SpeechEngine};
use crate::live::{ConfigWatcher, WatcherGuard};
use crate::menubar::{MenuActions, MenuState};
use crate::pipeline::Vocabulary;
use crate::settings::{bool_field, dictation_enabled, load_runtime_config, toggle_dictation};
use crate::singleinstance::{acquire_at, lock_file_path, SingleInstance};
use crate::status::{last_text_path, read_last_text, status_path, ListenerFact, StatusReporter};
use crate::warmup::LlmWarmer;
use crate::worker::{LlmFactory, SttFactory, VocabularyFn, WorkItem, Worker};

pub struct RunOptions {
    pub audio: Box<dyn AudioSource>,
    pub speech: Arc<dyn SpeechEngine>,
    pub build_stt: SttFactory,
    pub play_sound: Arc<dyn Fn(&str) + Send + Sync>,
}

pub type ToggleStore = Arc<dyn Fn() -> Result<bool, String> + Send + Sync>;
type KeyboardSource = Arc<
    dyn Fn(
            &KeyboardHub,
            lf_input::listener::WarningSink,
            Arc<AtomicBool>,
            lf_input::listener::ReadySink,
        ) -> Result<(), InputError>
        + Send
        + Sync,
>;

struct RuntimePaths {
    config: PathBuf,
    config_source: Option<PathBuf>,
    lock: PathBuf,
    status: PathBuf,
    last_text: PathBuf,
    history: Option<PathBuf>,
}

impl RuntimePaths {
    fn user() -> Result<Self> {
        Ok(Self {
            config: lf_core::config::try_config_path()?,
            config_source: None,
            lock: lock_file_path("localflow"),
            status: status_path(),
            last_text: last_text_path(),
            history: None,
        })
    }
}

pub struct RuntimeHandle {
    pub menu: MenuState,
    pub controller: ControllerHandle,
    live: Arc<LiveSettings>,
    stop: Arc<AtomicBool>,
    work: Sender<WorkItem>,
    watch: Option<WatcherGuard>,
    consumer: Option<JoinHandle<()>>,
    worker: Option<JoinHandle<()>>,
    worker_done: Option<std::sync::mpsc::Receiver<()>>,
    listener: Option<JoinHandle<Result<(), String>>>,
    lock: Option<SingleInstance>,
    status: Option<Arc<StatusReporter>>,
}

impl RuntimeHandle {
    /// The status-file `instance` of this running runtime: proof for the
    /// process owning this handle that a status file is its current
    /// runtime's (not an earlier start's). `None` once shut down.
    pub fn status_instance(&self) -> Option<String> {
        self.status.as_ref().map(|s| s.instance().to_string())
    }

    pub fn apply_config(&self, config: &Config) {
        self.live.apply(config);
    }

    pub fn shutdown(&mut self) {
        self.shutdown_with_timeout(Duration::from_millis(250));
    }

    /// Close capture/listener and release ownership before returning. Queued work is
    /// discarded; uninterruptible inference may finish after the timeout, but its
    /// closed controller cannot paste results. Returns whether the worker finished.
    pub fn shutdown_with_timeout(&mut self, timeout: Duration) -> bool {
        if self.stop.swap(true, Ordering::AcqRel) {
            return self.finish_worker(Duration::ZERO);
        }
        self.controller.shutdown();
        self.watch.take();
        if let Some(thread) = self.consumer.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.listener.take() {
            let _ = thread.join();
        }
        let _ = self.work.send(WorkItem::Shutdown);
        let finished = self.finish_worker(timeout);
        // Readers see an explicit stop, not a stale "ok" listener.
        if let Some(status) = self.status.take() {
            status.set_listener(ListenerFact::Dead, "LocalFlow stopped");
            status.set_state("stopped");
        }
        self.lock.take();
        finished
    }

    fn finish_worker(&mut self, timeout: Duration) -> bool {
        if self.worker.is_none() {
            return true;
        }
        let finished = self.worker_done.as_ref().is_some_and(|done| {
            !matches!(
                done.recv_timeout(timeout),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            )
        });
        if finished {
            if let Some(worker) = self.worker.take() {
                if worker.join().is_err() {
                    log::error!("dictation worker panicked during shutdown");
                }
            }
            self.worker_done.take();
        }
        finished
    }

    pub fn menu_actions(&self, quit: Arc<dyn Fn() + Send + Sync>) -> MenuActions {
        let toggle = self.controller.clone();
        let paste = self.controller.clone();
        MenuActions {
            toggle_dictation: Arc::new(move || toggle.note_dictation_toggle()),
            paste_last: Arc::new(move || paste.note_paste_last()),
            quit,
        }
    }
}

impl Drop for RuntimeHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start without opening audio, loading weights or owning the application's UI loop.
pub fn start(options: RunOptions) -> Result<Option<RuntimeHandle>> {
    let config = load_runtime_config(None)?;
    let menu = MenuState::new(crate::platform_menu_bar(), dictation_enabled(&config));
    start_with_menu(options, menu, None)
}

pub fn start_with_menu(
    options: RunOptions,
    menu: MenuState,
    toggle: Option<ToggleStore>,
) -> Result<Option<RuntimeHandle>> {
    start_at(
        options,
        menu,
        toggle,
        RuntimePaths::user()?,
        keyboard_source(false),
    )
}

fn keyboard_source(prompt: bool) -> KeyboardSource {
    Arc::new(move |hub, warn, stop, ready| {
        lf_input::listener::run_cancellable_with_ready(hub, warn, stop, prompt, ready)
    })
}

/// The listener fact an exit reason stands for.
fn listener_failure(error: &InputError) -> ListenerFact {
    match error {
        InputError::TapCreateFailed => ListenerFact::Denied,
        InputError::Unsupported(_) => ListenerFact::Unsupported,
        InputError::Backend(_) => ListenerFact::Dead,
    }
}

pub fn default_vocabulary(config: &Config) -> VocabularyFn {
    let dictionary = DictionaryStore::new(None);
    let phrases = HotPhraseStore::default_store();
    let commands = discover_commands(
        &config.field_str_list("command_names").unwrap_or_default(),
        &config.field_str_list("skills_dirs").unwrap_or_default(),
    );
    Arc::new(move |config| Vocabulary {
        words: dictionary.words(),
        replacements: dictionary.replacements(),
        hot_phrases: if bool_field(config, "hot_phrases", true) {
            phrases.enabled().unwrap_or_default()
        } else {
            Vec::new()
        },
        commands: commands.clone(),
    })
}

pub fn default_llm_factory() -> LlmFactory {
    Arc::new(|config| build_llm(config, &BuildOptions::default(), &CoreSecrets).ok())
}

fn start_at(
    options: RunOptions,
    menu: MenuState,
    toggle: Option<ToggleStore>,
    paths: RuntimePaths,
    listen: KeyboardSource,
) -> Result<Option<RuntimeHandle>> {
    let Some(mut lock) = acquire_at(&paths.lock)? else {
        return Ok(None);
    };
    let config = Arc::new(load_runtime_config(paths.config_source.as_deref())?);
    let history_enabled = bool_field(&config, "history_enabled", true);
    let lock_identity = lock.identity().unwrap_or_default();
    let status = Arc::new(
        StatusReporter::new(
            &paths.status,
            config.field_str("input_device").unwrap_or(""),
        )
        .with_lock_identity(&lock_identity),
    );
    status.publish();
    let (work_tx, work_rx) = work_queue();
    let state_menu = menu.clone();
    let toggle_path = paths.config.clone();
    let history_path = paths.history.clone();
    let state_status = status.clone();
    let live_slot: Arc<Mutex<Weak<LiveSettings>>> = Arc::default();
    let toggle_live = live_slot.clone();
    let toggle = toggle.unwrap_or_else(|| {
        Arc::new(move || toggle_dictation(&toggle_path).map_err(|e| e.to_string()))
    });
    let prepare = Mutex::new(work_tx.clone());
    let warmer = LlmWarmer::ollama();
    let deps = ControllerDeps {
        audio: options.audio,
        jobs: job_sink(work_tx.clone()),
        // Both are asynchronous: the worker loads speech while the user speaks
        // and the warm-up runs on its own thread.
        on_recording_start: Some(Arc::new(move |kind, config: &Config| {
            let _ = prepare
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .send(WorkItem::PrepareSpeech);
            warmer.on_recording_start(kind, config);
        })),
        output: Arc::new(Injector::native()),
        play_sound: options.play_sound,
        frontmost: Arc::new(crate::context::frontmost_app),
        on_state: Some(Arc::new(move |state: &str| {
            state_status.set_state(state);
            state_menu.change(|m| m.app_state = state.to_string());
        })),
        latest_text: Arc::new(move |fallback: &str| {
            HistoryStore::open(history_path.clone(), None, history_enabled)
                .and_then(|h| h.latest_text(fallback))
                .unwrap_or_else(|_| fallback.to_string())
        }),
        toggle_store: Arc::new(move || {
            let enabled = toggle()?;
            if let Some(live) = toggle_live
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .upgrade()
            {
                let mut updated = (*live.config()).clone();
                updated.set_field("dictation_enabled", serde_json::json!(enabled));
                live.apply(&updated);
            }
            Ok(enabled)
        }),
        clock: {
            let start = std::time::Instant::now();
            Arc::new(move || start.elapsed().as_secs_f64())
        },
        min_clip_seconds: MIN_CLIP_SECONDS,
        initial_last_text: read_last_text(&paths.last_text),
    };
    let (controller, consumer) = Controller::new(config.clone(), deps);
    let consumer_thread = consumer.spawn();
    let live = Arc::new(LiveSettings::new(
        config.clone(),
        controller.clone(),
        Some(work_tx.clone()),
        Some(menu.clone()),
    ));
    *live_slot.lock().unwrap_or_else(|p| p.into_inner()) = Arc::downgrade(&live);
    menu.change(|model| model.dictation_enabled = dictation_enabled(&config));
    let mut handle = RuntimeHandle {
        menu: menu.clone(),
        controller: controller.clone(),
        live: live.clone(),
        stop: Arc::new(AtomicBool::new(false)),
        work: work_tx,
        watch: None,
        consumer: Some(consumer_thread),
        worker: None,
        worker_done: None,
        listener: None,
        lock: Some(lock),
        status: Some(status.clone()),
    };
    let worker_live = live.clone();
    let deliver_controller = controller.clone();
    let worker = Worker {
        config: Arc::new(move || worker_live.config()),
        vocabulary: default_vocabulary(&config),
        history: HistoryStore::open(paths.history, None, history_enabled).ok(),
        speech: options.speech,
        build_llm: default_llm_factory(),
        build_stt: options.build_stt,
        deliver: Arc::new(move |job, text, error| {
            deliver_controller.note_job_result(
                text,
                job.clipboard_before.clone(),
                error,
                Some(job.selection.clone()),
            )
        }),
        last_text_path: Some(paths.last_text),
    };
    let worker_stop = handle.stop.clone();
    let (finished, done) = std::sync::mpsc::channel();
    handle.worker_done = Some(done);
    handle.worker = Some(
        std::thread::Builder::new()
            .name("localflow-worker".into())
            .spawn(move || {
                worker.run_until(work_rx, worker_stop);
                let _ = finished.send(());
            })?,
    );
    let (hub, warnings) = KeyboardHub::new(
        &keyboard_bindings(&config, &controller),
        hub_actions(&controller),
    );
    for warning in warnings {
        log::warn!("{warning}");
    }
    live.set_hub(hub.clone());
    let apply_live = live.clone();
    let tick_live = live.clone();
    let beat = status.clone();
    let mic_status = status.clone();
    let loader_path = paths.config_source.clone();
    handle.watch = Some(
        ConfigWatcher::new(
            paths.config.clone(),
            Arc::new(move || load_runtime_config(loader_path.as_deref())),
            Arc::new(move |fresh| {
                mic_status.set_mic_configured(fresh.field_str("input_device").unwrap_or(""));
                apply_live.apply(&fresh);
            }),
        )
        .with_tick(
            // Always wake on the interval so the status heartbeat stays fresh
            // while this runtime lives; deferred rebinding retries meanwhile.
            Arc::new(move || {
                tick_live.retry();
                beat.beat();
                true
            }),
            crate::live::DEFAULT_INTERVAL,
        )
        .start(),
    );
    if let Ok(fresh) = load_runtime_config(paths.config_source.as_deref()) {
        live.apply(&fresh);
    }
    let listener_stop = handle.stop.clone();
    let warning_menu = menu.clone();
    let listener_status = status;
    let listener_controller = controller.clone();
    handle.listener = Some(
        std::thread::Builder::new()
            .name("localflow-keyboard".into())
            .spawn(move || {
                let warn_status = listener_status.clone();
                let warn = Arc::new(move |warning: InputWarning| {
                    log::warn!("{warning}");
                    match warning {
                        InputWarning::InputMonitoringDenied => {
                            warn_status.add_warning("input-monitoring")
                        }
                        InputWarning::DeadTap => {
                            warn_status.listener_dead("dead-tap", &warning.to_string())
                        }
                    }
                    warning_menu
                        .change(|m| m.input_warning = Some("⚠︎ Input Monitoring needed".into()));
                });
                // Only the platform's own confirmation marks the listener ok.
                let ready_status = listener_status.clone();
                let ready: lf_input::listener::ReadySink =
                    Arc::new(move || ready_status.set_listener(ListenerFact::Ok, ""));
                let result = listen(&hub, warn, listener_stop.clone(), ready);
                if !listener_stop.load(Ordering::Acquire) {
                    let reason = result
                        .as_ref()
                        .err()
                        .map(InputError::to_string)
                        .unwrap_or_else(|| "unexpected exit".into());
                    log::warn!("keyboard listener stopped: {reason}");
                    let fact = result
                        .as_ref()
                        .err()
                        .map_or(ListenerFact::Dead, listener_failure);
                    listener_status.set_listener(fact, &reason);
                    // Shortcuts are gone, so nothing could end a hands-free
                    // (toggle/locked) or command recording: discard it. The
                    // source already released held keys, so a hold gesture
                    // has ended normally; cancel is a no-op when idle.
                    listener_controller.note_cancel();
                    let note = match fact {
                        ListenerFact::Unsupported => "⚠︎ Global shortcuts unavailable",
                        _ => "⚠︎ Input Monitoring needed",
                    };
                    menu.change(|m| m.input_warning = Some(note.into()));
                }
                result.map_err(|e| e.to_string())
            })?,
    );
    Ok(Some(handle))
}

/// Standalone owns its menu loop; embedded applications retain their own loop.
pub fn run(options: RunOptions) -> Result<()> {
    let config = load_runtime_config(None)?;
    let menu = MenuState::new(crate::platform_menu_bar(), dictation_enabled(&config));
    let mut runtime = start_at(
        options,
        menu,
        None,
        RuntimePaths::user()?,
        keyboard_source(true),
    )?
    .ok_or_else(|| anyhow!("another localflow instance is already running"))?;
    #[cfg(target_os = "macos")]
    let result = crate::macos::run_menu_bar(
        runtime.menu.snapshot(),
        runtime.menu_actions(Arc::new(|| std::process::exit(0))),
    )
    .map_err(|e| anyhow!(e));
    #[cfg(not(target_os = "macos"))]
    let result = runtime
        .listener
        .take()
        .unwrap()
        .join()
        .map_err(|_| anyhow!("keyboard thread panicked"))?
        .map_err(|e| anyhow!(e));
    runtime.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn deferred_legacy_settings_survive_embedded_start() {
        const CHILD: &str = "LF_TEST_RUNTIME_LEGACY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "run::tests::deferred_legacy_settings_survive_embedded_start",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("HOME", root.path())
                .env("USERPROFILE", root.path())
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("WIN_PD_OVERRIDE_LOCAL_APPDATA", root.path().join("data"))
                .env_remove("FIREWORKS_API_KEY")
                .env_remove("LOCALFLOW_FIREWORKS_API_KEY")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        struct UnavailableKeyring;
        impl lf_core::secrets::KeyringBackend for UnavailableKeyring {
            fn get_password(&self, _: &str, _: &str) -> anyhow::Result<Option<String>> {
                Ok(None)
            }
            fn set_password(&self, _: &str, _: &str, _: &str) -> anyhow::Result<()> {
                anyhow::bail!("keychain unavailable")
            }
            fn delete_password(&self, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }
        lf_core::secrets::set_keyring_backend(Some(Arc::new(UnavailableKeyring)));
        let legacy = lf_core::config::try_legacy_config_path().unwrap();
        std::fs::write(&legacy, "model_size = 'tiny.en'\nsounds_enabled = false\nfireworks_api_key = 'test-only-migration-key'\n").unwrap();
        let target = lf_core::config::try_config_path().unwrap();
        let expected = load_runtime_config(None).unwrap();
        assert!(!target.exists(), "migration should defer its write");
        assert_eq!(expected.field_str("model_size"), Some("tiny.en"));
        let (started, _received) = mpsc::channel();
        let mut runtime = start_at(
            options(Box::new(Audio {
                recording: Arc::new(AtomicBool::new(false)),
                started,
            })),
            MenuState::new(None, true),
            None,
            RuntimePaths::user().unwrap(),
            Arc::new(|_, _, _, _| Err(InputError::Backend("test listener".into()))),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            runtime.live.config().field_str("model_size"),
            Some("tiny.en")
        );
        assert_eq!(
            runtime.live.config().field_bool("sounds_enabled"),
            Some(false)
        );
        runtime.shutdown();
        assert!(legacy.is_file());
    }

    struct Audio {
        recording: Arc<AtomicBool>,
        started: mpsc::Sender<()>,
    }
    impl AudioSource for Audio {
        fn start(&mut self) -> Result<(), String> {
            self.recording.store(true, Ordering::Release);
            let _ = self.started.send(());
            Ok(())
        }
        fn stop(&mut self) -> Vec<f32> {
            self.recording.store(false, Ordering::Release);
            Vec::new()
        }
        fn is_recording(&self) -> bool {
            self.recording.load(Ordering::Acquire)
        }
    }
    struct MissingSpeech;
    impl SpeechEngine for MissingSpeech {
        fn loaded(&self) -> bool {
            false
        }
        fn load(&self) -> Result<(), String> {
            Err("model missing".into())
        }
        fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
            Err("model missing".into())
        }
        fn unload(&self) {}
    }
    fn paths(root: &std::path::Path) -> RuntimePaths {
        RuntimePaths {
            config: root.join("config.toml"),
            config_source: Some(root.join("config.toml")),
            lock: root.join("localflow.lock"),
            status: root.join("status"),
            last_text: root.join("last-text"),
            history: Some(root.join("history.sqlite")),
        }
    }
    fn options(audio: Box<dyn AudioSource>) -> RunOptions {
        RunOptions {
            audio,
            speech: Arc::new(MissingSpeech),
            build_stt: Arc::new(|_| Ok(Arc::new(MissingSpeech))),
            play_sound: Arc::new(|_| {}),
        }
    }

    #[test]
    fn bounded_shutdown_releases_ownership_and_discards_queued_reloads() {
        use std::sync::atomic::AtomicUsize;
        struct BlockingLoad {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl SpeechEngine for BlockingLoad {
            fn load(&self) -> Result<(), String> {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(|error| error.to_string())
            }
            fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
                unreachable!()
            }
            fn unload(&self) {}
        }
        let root = tempfile::tempdir().unwrap();
        let (entered, loading) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let blocked = Arc::new(BlockingLoad {
            entered,
            release: Mutex::new(released),
        });
        let builds = Arc::new(AtomicUsize::new(0));
        let count = builds.clone();
        let (started, _received) = mpsc::channel();
        let mut opts = options(Box::new(Audio {
            recording: Arc::new(AtomicBool::new(false)),
            started,
        }));
        opts.build_stt = Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(blocked.clone())
        });
        let listener: KeyboardSource =
            Arc::new(|_, _, _, _| Err(InputError::Backend("test listener".into())));
        let mut runtime = start_at(
            opts,
            MenuState::new(None, true),
            None,
            paths(root.path()),
            listener.clone(),
        )
        .unwrap()
        .unwrap();
        runtime
            .work
            .send(WorkItem::ReloadStt(Arc::new(Config::default())))
            .unwrap();
        loading.recv_timeout(Duration::from_secs(5)).unwrap();
        runtime
            .work
            .send(WorkItem::ReloadStt(Arc::new(Config::default())))
            .unwrap();
        let before = std::time::Instant::now();
        assert!(!runtime.shutdown_with_timeout(Duration::ZERO));
        assert!(before.elapsed() < Duration::from_secs(3));
        let (started, _received) = mpsc::channel();
        let mut restarted = start_at(
            options(Box::new(Audio {
                recording: Arc::new(AtomicBool::new(false)),
                started,
            })),
            MenuState::new(None, true),
            None,
            paths(root.path()),
            listener,
        )
        .unwrap()
        .expect("ownership released even while old native work is blocked");
        restarted.shutdown();
        release.send(()).unwrap();
        assert!(runtime.finish_worker(Duration::from_secs(5)));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(runtime.shutdown_with_timeout(Duration::ZERO));
    }
    #[test]
    fn embedded_start_is_lazy_listener_failure_is_nonfatal_and_shutdown_closes_capture() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.set_field("command_names", serde_json::json!([]));
        config.set_field("skills_dirs", serde_json::json!([]));
        // Keep the recording start offline: no Ollama warm-up request.
        config.set_field("cleanup_enabled", serde_json::json!(false));
        lf_core::config::save_config(&config, Some(&paths(root.path()).config)).unwrap();
        let recording = Arc::new(AtomicBool::new(false));
        let (started, received) = mpsc::channel();
        let mut runtime = start_at(
            options(Box::new(Audio {
                recording: recording.clone(),
                started,
            })),
            MenuState::new(None, true),
            None,
            paths(root.path()),
            Arc::new(|_, _, _, _| Err(InputError::TapCreateFailed)),
        )
        .unwrap()
        .unwrap();
        assert!(runtime.listener.take().unwrap().join().unwrap().is_err());
        assert!(runtime
            .menu
            .snapshot()
            .input_warning
            .unwrap()
            .contains("Input Monitoring needed"));
        let status = crate::status::StatusFile::read(&paths(root.path()).status)
            .unwrap()
            .unwrap();
        assert_eq!(status.listener, Some(ListenerFact::Denied));
        assert!(status.warnings.contains(&"listener".to_string()));
        assert!(!recording.load(Ordering::Acquire));
        runtime.controller.note_start();
        received.recv_timeout(Duration::from_secs(3)).unwrap();
        runtime.shutdown();
        runtime.shutdown();
        assert!(!recording.load(Ordering::Acquire));
    }
    #[test]
    fn a_failing_listener_ends_toggle_mode_capture() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.set_field("command_names", serde_json::json!([]));
        config.set_field("skills_dirs", serde_json::json!([]));
        config.set_field("cleanup_enabled", serde_json::json!(false));
        lf_core::config::save_config(&config, Some(&paths(root.path()).config)).unwrap();
        let recording = Arc::new(AtomicBool::new(false));
        let (started, capture_started) = mpsc::channel();
        let (fail, failure) = mpsc::channel::<()>();
        let failure = Mutex::new(failure);
        let mut runtime = start_at(
            options(Box::new(Audio {
                recording: recording.clone(),
                started,
            })),
            MenuState::new(None, true),
            None,
            paths(root.path()),
            Arc::new(move |_, _, _, ready| {
                ready();
                // The source dies (X connection lost) while hands-free
                // recording runs.
                failure.lock().unwrap().recv().unwrap();
                Err(InputError::Backend("X connection closed".into()))
            }),
        )
        .unwrap()
        .unwrap();
        runtime.controller.note_toggle();
        capture_started
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(recording.load(Ordering::Acquire));
        fail.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while recording.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "capture kept running after the shortcuts disappeared"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = crate::status::StatusFile::read(&paths(root.path()).status)
            .unwrap()
            .unwrap();
        assert_eq!(status.listener, Some(ListenerFact::Dead));
        runtime.shutdown();
    }

    #[test]
    fn listener_is_ok_only_after_platform_registration_and_stop_is_explicit() {
        use crate::status::StatusFile;
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.set_field("command_names", serde_json::json!([]));
        config.set_field("skills_dirs", serde_json::json!([]));
        config.set_field("cleanup_enabled", serde_json::json!(false));
        lf_core::config::save_config(&config, Some(&paths(root.path()).config)).unwrap();
        let (entered, registering) = mpsc::channel();
        let (register, registered) = mpsc::channel::<()>();
        let registered = Mutex::new(registered);
        let (started, _) = mpsc::channel();
        let mut runtime = start_at(
            options(Box::new(Audio {
                recording: Arc::new(AtomicBool::new(false)),
                started,
            })),
            MenuState::new(None, true),
            None,
            paths(root.path()),
            Arc::new(move |_, _, stop, ready| {
                entered.send(()).unwrap();
                registered.lock().unwrap().recv().unwrap();
                ready();
                while !stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            }),
        )
        .unwrap()
        .unwrap();
        let status_path = paths(root.path()).status;
        registering.recv_timeout(Duration::from_secs(3)).unwrap();
        let before = StatusFile::read(&status_path).unwrap().unwrap();
        assert_eq!(
            before.instance,
            runtime.status_instance(),
            "the handle proves the file"
        );
        let (holder, _) = crate::singleinstance::read_holder_at(&paths(root.path()).lock);
        assert_eq!(holder, Some(std::process::id()));
        assert_eq!(
            before.lock_identity.as_deref(),
            std::fs::read_to_string(paths(root.path()).lock)
                .ok()
                .as_deref()
                .map(str::trim),
            "bound to this lock acquisition"
        );
        assert_eq!(
            before.listener,
            Some(ListenerFact::Starting),
            "a spawned listener thread is not a listening one"
        );
        register.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let now = StatusFile::read(&status_path).unwrap().unwrap();
            if now.listener == Some(ListenerFact::Ok) {
                assert_eq!(now.instance, before.instance);
                break;
            }
            assert!(std::time::Instant::now() < deadline, "listener never ok");
            std::thread::sleep(Duration::from_millis(10));
        }
        runtime.shutdown();
        assert_eq!(runtime.status_instance(), None, "no proof after shutdown");
        let after = StatusFile::read(&status_path).unwrap().unwrap();
        assert_eq!(after.state.as_deref(), Some("stopped"));
        assert_eq!(after.listener, Some(ListenerFact::Dead));
    }

    #[test]
    fn speech_loads_at_recording_start_not_at_startup() {
        struct SignalLoad(mpsc::Sender<()>);
        impl SpeechEngine for SignalLoad {
            fn loaded(&self) -> bool {
                false
            }
            fn load(&self) -> Result<(), String> {
                let _ = self.0.send(());
                Err("not provisioned".into())
            }
            fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
                Err("not provisioned".into())
            }
            fn unload(&self) {}
        }
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.set_field("command_names", serde_json::json!([]));
        config.set_field("skills_dirs", serde_json::json!([]));
        config.set_field("cleanup_enabled", serde_json::json!(false));
        lf_core::config::save_config(&config, Some(&paths(root.path()).config)).unwrap();
        let (started, received) = mpsc::channel();
        let (loads, loaded) = mpsc::channel();
        let mut opts = options(Box::new(Audio {
            recording: Arc::new(AtomicBool::new(false)),
            started,
        }));
        opts.speech = Arc::new(SignalLoad(loads));
        let mut runtime = start_at(
            opts,
            MenuState::new(None, true),
            None,
            paths(root.path()),
            Arc::new(|_, _, _, _| Err(InputError::Backend("test listener".into()))),
        )
        .unwrap()
        .unwrap();
        assert!(loaded.recv_timeout(Duration::from_millis(200)).is_err());
        runtime.controller.note_start();
        received.recv_timeout(Duration::from_secs(3)).unwrap();
        loaded.recv_timeout(Duration::from_secs(3)).unwrap();
        runtime.shutdown();
    }
    #[test]
    fn existing_owner_skips_audio_speech_and_keyboard_startup() {
        let root = tempfile::tempdir().unwrap();
        let _owner = acquire_at(&paths(root.path()).lock).unwrap().unwrap();
        let (started, _) = mpsc::channel();
        let recording = Arc::new(AtomicBool::new(false));
        assert!(start_at(
            options(Box::new(Audio {
                recording: recording.clone(),
                started
            })),
            MenuState::new(None, true),
            None,
            paths(root.path()),
            Arc::new(|_, _, _, _| panic!("must not start a second listener"))
        )
        .unwrap()
        .is_none());
        assert!(!recording.load(Ordering::Acquire));
        assert!(!paths(root.path()).status.exists());
    }
    #[test]
    fn missing_speech_is_reported_without_loading_or_panicking_at_startup() {
        let menu = MenuState::new(None, true);
        let errors = menu.clone();
        let speech = crate::engine::ReportingSpeech::new(
            Arc::new(MissingSpeech),
            Arc::new(move |error| errors.change(|model| model.input_warning = Some(error.into()))),
        );
        assert!(!speech.loaded());
        assert_eq!(
            speech.transcribe(&[0.5], None).unwrap_err(),
            "model missing"
        );
        assert_eq!(
            menu.snapshot().input_warning.as_deref(),
            Some("model missing")
        );
    }
}
