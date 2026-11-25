//! In-process services; initialization and all native/model/network work run off AppKit.

use anyhow::Result;
use lf_core::config::Config;
use lf_runtime::{
    engine::SpeechEngine,
    menubar::MenuState,
    run::{RunOptions, RuntimeHandle, ToggleStore},
};
use lf_server::{Server, ServerState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

mod meeting_watch;

struct NativeCapabilities;
impl lf_server::backend::Backend for NativeCapabilities {
    fn handle(
        &self,
        request: lf_server::backend::BackendRequest,
    ) -> lf_server::backend::BackendResponse {
        lf_server::backend::Backend::handle(&lf_server::backend::UnavailableBackend, request)
    }
    fn stt_available(&self, provider: &str) -> bool {
        lf_stt::factory::backend_available(provider)
    }
}

fn dictation_backend(
    speech: Arc<dyn SpeechEngine>,
    config: &Config,
) -> lf_runtime::server::DictationBackend {
    let mut backend = lf_runtime::server::DictationBackend::new(speech, config);
    backend.fallback = Arc::new(NativeCapabilities);
    backend
}

type Engine = Arc<dyn SpeechEngine>;

/// The server's speech selection. A newly selected engine waits as `pending`
/// while any request still borrows `current`, and is promoted on the first
/// access after that borrow ends, so two engines are never selected at once
/// and nothing has to load either engine for the handoff to happen.
///
/// Engines leave the slot only by being moved into a `retired` list; they are
/// never dropped while the slot lock is held.
struct SpeechSlot {
    current: Engine,
    pending: Option<Engine>,
}

impl SpeechSlot {
    /// Promote `pending` once nothing but the slot references `current`.
    fn settle(&mut self, retired: &mut Vec<Engine>) {
        if Arc::strong_count(&self.current) == 1 {
            if let Some(pending) = self.pending.take() {
                retired.push(std::mem::replace(&mut self.current, pending));
            }
        }
    }

    fn holds(&self, engine: &Engine) -> bool {
        Arc::ptr_eq(&self.current, engine)
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, engine))
    }
}

/// Drops replaced engines on one cleanup thread per holder, started on the
/// first retirement: an engine's destructor may wait for a helper process to
/// exit, which must never stall a request (such as status) or a lock holder.
#[derive(Default)]
struct Reaper {
    sender: Option<std::sync::mpsc::Sender<Engine>>,
    /// Engines the cleanup thread could not take yet. They are kept, never
    /// dropped here, and handed over with the next retirement.
    parked: Vec<Engine>,
}

impl Reaper {
    fn retire(&mut self, retired: Vec<Engine>) {
        self.parked.extend(retired);
        // A stopped cleanup thread is restarted at most once per retirement.
        for _ in 0..2 {
            if self.parked.is_empty() {
                return;
            }
            let sender = match self.sender.take() {
                Some(sender) => sender,
                None => match spawn_reaper() {
                    Ok(sender) => sender,
                    Err(error) => {
                        log::error!("could not start speech cleanup: {error}");
                        return;
                    }
                },
            };
            while let Some(engine) = self.parked.pop() {
                if let Err(std::sync::mpsc::SendError(engine)) = sender.send(engine) {
                    self.parked.push(engine);
                    break;
                }
            }
            if self.parked.is_empty() {
                self.sender = Some(sender);
                return;
            }
            log::error!("speech cleanup stopped; restarting it");
        }
    }
}

fn spawn_reaper() -> std::io::Result<std::sync::mpsc::Sender<Engine>> {
    let (sender, retired) = std::sync::mpsc::channel::<Engine>();
    std::thread::Builder::new()
        .name("localflow-speech-cleanup".into())
        .spawn(move || {
            for engine in retired {
                let release = std::panic::AssertUnwindSafe(|| drop(engine));
                if std::panic::catch_unwind(release).is_err() {
                    log::error!("releasing a replaced speech engine panicked");
                }
            }
        })?;
    Ok(sender)
}

struct SpeechHolder {
    slot: Mutex<SpeechSlot>,
    reaper: Mutex<Reaper>,
}

#[derive(Clone)]
struct SharedSpeech(Arc<SpeechHolder>);

impl SharedSpeech {
    fn new(engine: Engine) -> Self {
        Self(Arc::new(SpeechHolder {
            slot: Mutex::new(SpeechSlot {
                current: engine,
                pending: None,
            }),
            reaper: Mutex::default(),
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SpeechSlot> {
        // Also taken from `Held::drop`, which must not panic on poison.
        self.0
            .slot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Apply `change` to the settled slot. Everything it (or a promotion)
    /// retires goes to the reaper after the slot lock is released.
    fn update<T>(&self, change: impl FnOnce(&mut SpeechSlot, &mut Vec<Engine>) -> T) -> T {
        let mut retired = Vec::new();
        let value = {
            let mut slot = self.lock();
            slot.settle(&mut retired);
            let value = change(&mut slot, &mut retired);
            slot.settle(&mut retired);
            value
        };
        self.retire(retired);
        value
    }

    fn retire(&self, retired: Vec<Engine>) {
        if !retired.is_empty() {
            self.0
                .reaper
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .retire(retired);
        }
    }

    fn hold(&self, engine: Engine) -> Held {
        Held {
            engine: Some(engine),
            owner: self.clone(),
        }
    }

    /// The engine requests use now (promoting a released pending selection).
    fn current(&self) -> Held {
        self.hold(self.update(|slot, _| slot.current.clone()))
    }

    /// The newest selection: pending if one waits, otherwise current.
    fn latest(&self) -> Held {
        self.hold(self.update(|slot, _| slot.pending.as_ref().unwrap_or(&slot.current).clone()))
    }

    /// Select `engine`, immediately if `current` is unborrowed; otherwise it
    /// replaces (retires) any older pending selection and waits for the
    /// borrow to end.
    fn publish(&self, engine: Engine) {
        self.update(|slot, retired| {
            retired.extend(slot.pending.take());
            if Arc::ptr_eq(&slot.current, &engine) {
                retired.push(engine);
            } else {
                slot.pending = Some(engine);
            }
        });
    }

    /// Use `engine` now (a dictation reload that already loaded it); the old
    /// engine and an older pending selection are retired, so the latter can
    /// never be promoted over it.
    fn set(&self, engine: Engine) {
        self.update(|slot, retired| {
            retired.extend(slot.pending.take());
            retired.push(std::mem::replace(&mut slot.current, engine));
        });
    }

    /// Promote a pending selection whose predecessor has been released.
    fn settle(&self) {
        self.update(|_, _| ());
    }

    /// Take back a reference handed out by [`Held`]. One the slot still holds
    /// cannot be the last and is dropped under the lock (so the slot cannot
    /// retire it meanwhile); any other may be the last and goes to the reaper.
    fn release(&self, engine: Engine) {
        let mut retired = Vec::new();
        {
            let mut slot = self.lock();
            if slot.holds(&engine) {
                drop(engine);
            } else {
                retired.push(engine);
            }
            slot.settle(&mut retired);
        }
        self.retire(retired);
    }
}

/// A reference to one of `owner`'s engines. Dropping it hands the reference
/// back, so the last reference to a replaced engine is always dropped by the
/// reaper, never by the request, meeting or runtime that used it.
struct Held {
    engine: Option<Engine>,
    owner: SharedSpeech,
}

impl Held {
    fn engine(&self) -> &Engine {
        self.engine.as_ref().expect("an engine is held until drop")
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.take() {
            self.owner.release(engine);
        }
    }
}

impl SpeechEngine for Held {
    fn loaded(&self) -> bool {
        self.engine().loaded()
    }
    fn load(&self) -> std::result::Result<(), String> {
        self.engine().load()
    }
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> std::result::Result<String, String> {
        self.engine().transcribe(pcm, prompt)
    }
    fn unload(&self) {
        self.engine().unload();
    }
}

impl SpeechEngine for SharedSpeech {
    fn loaded(&self) -> bool {
        self.current().loaded()
    }
    fn load(&self) -> std::result::Result<(), String> {
        self.current().load()
    }
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> std::result::Result<String, String> {
        self.current().transcribe(pcm, prompt)
    }
    fn unload(&self) {
        self.current().unload();
    }
}

/// A dictation reload's engine; publishes itself to the server once loaded.
struct PublishSpeech(Held);

#[cfg(test)]
fn refresh_server_speech(
    previous: &Mutex<Config>,
    shared: &SharedSpeech,
    fresh: Config,
    factory: &lf_runtime::worker::SttFactory,
) -> Result<(), String> {
    replace_server_speech(&mut previous.lock().unwrap(), shared, fresh, factory)
}

/// Select speech for `fresh` when its STT settings differ from `previous`.
/// The engine is published, so an in-flight request keeps its engine until it
/// finishes. On a factory error nothing changes, including `previous`.
fn replace_server_speech(
    previous: &mut Config,
    shared: &SharedSpeech,
    fresh: Config,
    factory: &lf_runtime::worker::SttFactory,
) -> Result<(), String> {
    if lf_runtime::live::STT_FIELDS
        .iter()
        .any(|field| previous.field(field) != fresh.field(field))
    {
        shared.publish(factory(&fresh)?);
    }
    *previous = fresh;
    Ok(())
}

/// Hand STT reloads from the server to a started in-process dictation
/// runtime. Taken under the selection lock, so a concurrent server settings
/// change either completes before the handoff or is skipped after it.
fn hand_off_speech(selection: &Mutex<Config>, server_owns_speech: &AtomicBool, started: bool) {
    if started {
        let _selection = selection.lock().unwrap();
        server_owns_speech.store(false, Ordering::Release);
    }
}

/// `previous` is the config the server's speech was selected from; while
/// `update_speech` is set the server follows STT settings itself, once a
/// dictation runtime owns speech reloads (it publishes into `shared`) it stops.
fn watch_server_services(
    previous: Arc<Mutex<Config>>,
    shared: SharedSpeech,
    factory: lf_runtime::worker::SttFactory,
    update_speech: Arc<AtomicBool>,
    on_config: Arc<dyn Fn(Config) + Send + Sync>,
    maintenance: lf_runtime::live::OnTick,
) -> Result<lf_runtime::live::WatcherGuard> {
    let (apply, tick) = server_speech_updates(
        previous,
        shared,
        factory,
        update_speech,
        on_config,
        maintenance,
    );
    let watch = lf_runtime::live::ConfigWatcher::new(
        lf_core::config::try_config_path()?,
        Arc::new(|| lf_runtime::settings::load_runtime_config(None)),
        apply.clone(),
    )
    .with_tick(tick, lf_runtime::live::DEFAULT_INTERVAL)
    .start();
    if let Ok(fresh) = lf_runtime::settings::load_runtime_config(None) {
        apply(fresh);
    }
    Ok(watch)
}

/// The settings watcher's change callback and periodic tick, without the
/// file watcher itself (so tests can drive them without touching user config).
#[allow(clippy::type_complexity)]
fn server_speech_updates(
    previous: Arc<Mutex<Config>>,
    shared: SharedSpeech,
    factory: lf_runtime::worker::SttFactory,
    update_speech: Arc<AtomicBool>,
    on_config: Arc<dyn Fn(Config) + Send + Sync>,
    maintenance: lf_runtime::live::OnTick,
) -> (Arc<dyn Fn(Config) + Send + Sync>, lf_runtime::live::OnTick) {
    let tick_shared = shared.clone();
    let tick: lf_runtime::live::OnTick = Arc::new(move || {
        maintenance();
        // Release a superseded engine as soon as its last request finishes.
        tick_shared.settle();
        true
    });
    let apply: Arc<dyn Fn(Config) + Send + Sync> = Arc::new(move |fresh| {
        on_config(fresh.clone());
        let mut previous = previous.lock().unwrap();
        if update_speech.load(Ordering::Acquire) {
            if let Err(error) = replace_server_speech(&mut previous, &shared, fresh, &factory) {
                log::error!("server speech selection failed: {error}");
            }
        }
    });
    (apply, tick)
}

impl SpeechEngine for PublishSpeech {
    fn loaded(&self) -> bool {
        self.0.loaded()
    }
    fn load(&self) -> std::result::Result<(), String> {
        self.0.load()?;
        self.0.owner.set(self.0.engine().clone());
        Ok(())
    }
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> std::result::Result<String, String> {
        self.0.transcribe(pcm, prompt)
    }
    fn unload(&self) {
        self.0.unload();
    }
}

/// What a server-only runtime keeps to activate dictation later on the same
/// server and shared speech; consumed by the first successful activation.
struct Dictation {
    menu: MenuState,
    shared: SharedSpeech,
    make_speech: lf_runtime::worker::SttFactory,
    /// The config the server's speech was last selected from.
    selection: Arc<Mutex<Config>>,
    /// Whether the server, not a dictation runtime, follows STT settings.
    server_owns_speech: Arc<AtomicBool>,
    /// Meeting capture and meeting watching may open audio (setup finished).
    meetings: Arc<AtomicBool>,
    meeting_config: Option<Arc<dyn Fn(Config) + Send + Sync>>,
}

pub struct EmbeddedRuntime {
    shutting_down: bool,
    pub runtime: Option<RuntimeHandle>,
    server: Option<ServerHandle>,
    pub speech: Arc<dyn SpeechEngine>,
    memory: crate::speech_memory::SpeechMemory,
    server_watch: Option<lf_runtime::live::WatcherGuard>,
    meeting_watch: Option<meeting_watch::MeetingWatch>,
    dictation: Option<Dictation>,
}

impl EmbeddedRuntime {
    /// Start only the local server (health, setup and Hub APIs) with lazy
    /// shared speech. Nothing here opens audio or listens to the keyboard:
    /// meeting capture/watching and dictation wait for [`Self::activate`].
    pub fn start_server(config: Config, menu: MenuState) -> Self {
        let memory = crate::speech_memory::SpeechMemory::new();
        let model_memory = memory.clone();
        let error_menu = menu.clone();
        let report: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |error| {
            error_menu.change(|model| {
                model.input_warning = Some(format!("Speech model unavailable: {error}"))
            })
        });
        let make_speech: lf_runtime::worker::SttFactory = Arc::new(move |config| {
            Ok(
                model_memory.speech(Arc::new(lf_runtime::engine::ReportingSpeech::new(
                    lf_stt::runtime::hub_speech(config),
                    report.clone(),
                ))),
            )
        });
        let initial = make_speech(&config).expect("lazy speech construction is infallible");
        let shared = SharedSpeech::new(initial);
        let speech: Arc<dyn SpeechEngine> = Arc::new(shared.clone());
        let server_shared = shared.clone();
        let meetings = Arc::new(AtomicBool::new(false));
        let mut meeting_watch = None;
        let server = (|| -> Result<Option<ServerHandle>> {
            let host = config.field_str("server_host").unwrap_or("127.0.0.1");
            let port = u16::try_from(config.field_i64("server_port").unwrap_or(8756))?;
            ensure_server(host, port, || {
                ensure_server_token(&lf_core::apitoken::try_token_path()?)?;
                let dictation = dictation_backend(speech.clone(), &config);
                let backend = lf_meetings_backend::MeetingsBackend::new()?
                    .with_inner(Arc::new(dictation))
                    .with_capture(Arc::new(Capture {
                        speech: server_shared.clone(),
                        allowed: meetings.clone(),
                    }))
                    .with_perf_ledger(lf_core::perf_ledger::PerfLedger::default());
                let watcher = meeting_watch::MeetingWatch::new(config.clone())?;
                let backend: Arc<dyn lf_server::backend::Backend> =
                    Arc::new(backend.with_watcher(watcher.watcher.clone()));
                watcher.attach(&backend);
                meeting_watch = Some(watcher);
                Ok(Arc::new(
                    ServerState::load_with_backend(backend)?
                        .with_config_reader(Arc::new(lf_core::config::load_config))
                        .with_config_updater(Arc::new(|update| {
                            crate::config_write::update(None, |fresh| {
                                let updated = lf_core::config::apply_config_update(fresh, update)?;
                                lf_server::api::validate_runtime(&updated)
                                    .map_err(anyhow::Error::msg)?;
                                Ok(updated)
                            })
                        })),
                ))
            })
        })()
        .unwrap_or_else(|e| {
            log::error!("local server startup failed: {e}");
            None
        });
        eprintln!(
            "in-process server: ready; speech loaded: {}",
            speech.loaded()
        );
        let live = Arc::new(Mutex::new(config.clone()));
        let apply_live = live.clone();
        let meeting_config = meeting_watch
            .as_ref()
            .map(|watcher| watcher.apply_callback());
        let watcher_callback = meeting_config.clone();
        let watcher_allowed = meetings.clone();
        let on_config = Arc::new(move |fresh: Config| {
            *apply_live.lock().unwrap() = fresh.clone();
            // Watching probes the microphone; it waits for activation.
            if watcher_allowed.load(Ordering::Acquire) {
                if let Some(callback) = &watcher_callback {
                    callback(fresh);
                }
            }
        });
        let selection = Arc::new(Mutex::new(config));
        let server_owns_speech = Arc::new(AtomicBool::new(true));
        let server_state = server.as_ref().map(|server| server.state.clone());
        let server_watch = lf_core::config::try_config_path()
            .and_then(|path| {
                let settings = Mutex::new(crate::speech_memory::RuntimeSettings::new(&path));
                let maintenance_memory = memory.clone();
                let maintenance: lf_runtime::live::OnTick = Arc::new(move || {
                    let mut inspected = crate::speech_memory::Inspected::default();
                    {
                        let mut settings = settings.lock().unwrap();
                        settings.reload();
                        let enabled = live
                            .lock()
                            .unwrap()
                            .field_bool("dictation_enabled")
                            .unwrap_or(true);
                        let mut unload = || {
                            maintenance_memory.poll_holding(
                                enabled,
                                settings.timeout,
                                std::time::Instant::now(),
                                &mut inspected,
                            );
                        };
                        // `while_idle` holds the meetings state lock around
                        // the poll (and the poll holds the memory lock).
                        if let Some(state) = &server_state {
                            state.while_idle(&mut unload);
                        } else {
                            unload();
                        }
                    }
                    // An engine retired meanwhile may be destroyed here, after
                    // every lock above is released.
                    drop(inspected);
                    true
                });
                watch_server_services(
                    selection.clone(),
                    server_shared.clone(),
                    make_speech.clone(),
                    server_owns_speech.clone(),
                    on_config,
                    maintenance,
                )
            })
            .map_err(|error| {
                log::error!("could not watch server settings: {error}");
            })
            .ok();
        crate::speech_profile::start(speech.clone(), memory.clone());
        Self {
            shutting_down: false,
            runtime: None,
            server,
            speech,
            memory,
            server_watch,
            meeting_watch,
            dictation: Some(Dictation {
                menu,
                shared: server_shared,
                make_speech,
                selection,
                server_owns_speech,
                meetings,
                meeting_config,
            }),
        }
    }

    /// Whether [`Self::activate`] already succeeded (dictation running, or
    /// owned by another process).
    pub fn activated(&self) -> bool {
        self.dictation.is_none()
    }

    /// Start dictation (recorder, keyboard listener, menu runtime) on this
    /// already running server and shared speech, and allow meeting capture,
    /// using `config` loaded after setup saved it. Idempotent; a failure keeps
    /// the server serving and can be retried. Returns whether it succeeded.
    pub fn activate(&mut self, config: Config, toggle: ToggleStore) -> bool {
        self.activate_with(
            config,
            toggle,
            |config| {
                Ok(Box::new(lf_stt::runtime::RecorderSource::from_config(
                    config,
                )?))
            },
            lf_runtime::run::start_with_menu,
        )
    }

    fn activate_with(
        &mut self,
        config: Config,
        toggle: ToggleStore,
        open_audio: impl FnOnce(&Config) -> Result<Box<dyn lf_runtime::engine::AudioSource>>,
        start: impl FnOnce(RunOptions, MenuState, Option<ToggleStore>) -> Result<Option<RuntimeHandle>>,
    ) -> bool {
        if self.shutting_down {
            return false;
        }
        let Some(dictation) = self.dictation.take() else {
            return true;
        };
        dictation.menu.change(|model| {
            model.dictation_enabled = config.field_bool("dictation_enabled").unwrap_or(true)
        });
        if !dictation.meetings.swap(true, Ordering::AcqRel) {
            if let Some(apply) = &dictation.meeting_config {
                apply(config.clone());
            }
        }
        let memory = &self.memory;
        let result = (|| -> Result<Option<RuntimeHandle>> {
            // Dictation shares the server's newest selection; if a request
            // still borrows the old engine, the server switches once it ends.
            let speech: Arc<dyn SpeechEngine> = {
                let mut selected = dictation.selection.lock().unwrap();
                replace_server_speech(
                    &mut selected,
                    &dictation.shared,
                    config.clone(),
                    &dictation.make_speech,
                )
                .map_err(anyhow::Error::msg)?;
                Arc::new(dictation.shared.latest())
            };
            let reload_speech = dictation.make_speech.clone();
            let destination = dictation.shared.clone();
            let build_stt: lf_runtime::worker::SttFactory = Arc::new(move |config| {
                Ok(
                    Arc::new(PublishSpeech(destination.hold(reload_speech(config)?)))
                        as Arc<dyn SpeechEngine>,
                )
            });
            let options = RunOptions {
                audio: memory.audio(open_audio(&config)?),
                speech,
                build_stt,
                play_sound: Arc::new(lf_runtime::sounds::play),
            };
            start(options, dictation.menu.clone(), Some(toggle))
        })();
        match result {
            Ok(runtime) => {
                hand_off_speech(
                    &dictation.selection,
                    &dictation.server_owns_speech,
                    runtime.is_some(),
                );
                eprintln!(
                    "in-process runtime: dictation {}",
                    if runtime.is_some() {
                        "ready"
                    } else {
                        "owned by another process"
                    }
                );
                self.runtime = runtime;
                true
            }
            Err(error) => {
                log::error!("dictation startup failed: {error}");
                dictation.menu.change(|model| {
                    model.input_warning = Some(format!("Dictation unavailable: {error}"))
                });
                self.dictation = Some(dictation);
                false
            }
        }
    }

    pub fn shutdown(&mut self) {
        if std::mem::replace(&mut self.shutting_down, true) {
            return;
        }
        self.server_watch.take();
        if let Some(watcher) = self.meeting_watch.as_mut() {
            watcher.shutdown();
        }
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.shutdown();
        }
        let memory = self.memory.clone();
        if let Some(server) = self.server.as_mut() {
            server.shutdown_then(move || memory.shutdown());
        } else {
            finish_services(None, move || memory.shutdown());
        }
    }
}

impl Drop for EmbeddedRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn ensure_server_token(path: &std::path::Path) -> Result<()> {
    if lf_core::apitoken::load_at(path)?.is_empty() {
        lf_core::apitoken::load_or_create_at(path)?;
        anyhow::ensure!(
            !lf_core::apitoken::load_at(path)?.is_empty(),
            "could not persist local server API token"
        );
    }
    Ok(())
}

pub fn healthy(base: &str) -> bool {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .ok()
        .and_then(|client| {
            client
                .get(format!("{}/healthz", base.trim_end_matches('/')))
                .send()
                .ok()
        })
        .filter(|response| response.status().is_success())
        .and_then(|response| response.json::<serde_json::Value>().ok())
        .is_some_and(|body| body["ok"] == true)
}

fn ensure_server(
    host: &str,
    port: u16,
    state: impl FnOnce() -> Result<Arc<ServerState>>,
) -> Result<Option<ServerHandle>> {
    if healthy(&lf_server::server_url::base_url(host, port)) {
        eprintln!("local server: already healthy");
        return Ok(None);
    }
    Ok(Some(ServerHandle::start(host.to_owned(), port, state()?)?))
}

struct ServerHandle {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    state: Arc<ServerState>,
    drained: Option<std::sync::mpsc::Receiver<()>>,
}

const HTTP_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(4);

impl ServerHandle {
    fn start(host: String, port: u16, state: Arc<ServerState>) -> Result<Self> {
        Self::bind(host, port, state).map(|(handle, _)| handle)
    }
    fn bind(
        host: String,
        port: u16,
        state: Arc<ServerState>,
    ) -> Result<(Self, std::net::SocketAddr)> {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let shutdown_state = state.clone();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let (finished, drained) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("localflow-server".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .max_blocking_threads(16)
                        .enable_all()
                        .build()?;
                    let bound = ready.clone();
                    let result = runtime.block_on(async move {
                        let server = Server::bind_with_state(&host, port, state).await?;
                        let _ = bound.try_send(Ok(server.local_addr()?));
                        eprintln!("in-process server: listening {}", server.local_addr()?);
                        let (closing, closed) = tokio::sync::oneshot::channel();
                        let serving = server.run_until(async move {
                                let _ = stopped.await;
                                let _ = closing.send(());
                            });
                        tokio::pin!(serving);
                        tokio::select! {
                            result = &mut serving => result,
                            _ = closed => match tokio::time::timeout(HTTP_DRAIN_GRACE, &mut serving).await {
                                Ok(result) => result,
                                Err(_) => {
                                    log::warn!("aborting HTTP requests after shutdown grace period");
                                    Ok(())
                                }
                            }
                        }
                    });
                    runtime.shutdown_timeout(std::time::Duration::ZERO);
                    result
                })();
                if let Err(error) = result {
                    let _ = ready.try_send(Err(error.to_string()));
                    log::error!("local server stopped: {error}");
                    eprintln!("local server unavailable: {error}");
                }
                let _ = finished.send(());
            })?;
        let address = started
            .recv_timeout(std::time::Duration::from_secs(5))?
            .map_err(anyhow::Error::msg)?;
        Ok((
            Self {
                stop: Some(stop),
                state: shutdown_state,
                drained: Some(drained),
            },
            address,
        ))
    }
    #[cfg(test)]
    fn shutdown(&mut self) {
        self.shutdown_then(|| {});
    }
    fn shutdown_then(&mut self, after: impl FnOnce() + Send + 'static) {
        if let Some(stop) = self.stop.take() {
            finish_services(
                Some(ServerShutdown {
                    stop,
                    state: self.state.clone(),
                    drained: self.drained.take(),
                }),
                after,
            );
        }
    }
}

struct ServerShutdown {
    stop: tokio::sync::oneshot::Sender<()>,
    state: Arc<ServerState>,
    drained: Option<std::sync::mpsc::Receiver<()>>,
}

fn finish_services(server: Option<ServerShutdown>, after: impl FnOnce() + Send + 'static) {
    let pending = crate::shutdown::register_pending_meeting_write();
    if let Err(error) = std::thread::Builder::new()
        .name("localflow-meeting-save".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let remaining = || std::time::Duration::from_secs(60).saturating_sub(started.elapsed());
            if !pending.wait_for_ui_writes(remaining()) {
                log::warn!("timed out waiting for meeting notes before shutdown");
            }
            if let Some(server) = server {
                let http_started = std::time::Instant::now();
                let _ = server.stop.send(());
                if let Err(error) = server.state.shutdown() {
                    log::error!("meeting shutdown failed: {error}");
                }
                if let Some(drained) = server.drained {
                    let grace = HTTP_DRAIN_GRACE
                        .saturating_sub(http_started.elapsed())
                        .min(remaining());
                    if drained.recv_timeout(grace).is_err() {
                        log::warn!("timed out draining server requests during quit");
                    }
                }
            }
            after();
            drop(pending);
        })
    {
        log::error!("could not start meeting shutdown: {error}");
    }
}

/// Meeting capture; refuses to open audio until dictation activation (setup
/// finished), so a server-only setup run can never initialize the recorder.
/// The speech model is loaded before a meeting may start capturing, so a
/// model that cannot load refuses the start instead of dropping its audio.
struct Capture {
    speech: SharedSpeech,
    allowed: Arc<AtomicBool>,
}

impl Capture {
    fn ensure_allowed(&self) -> Result<()> {
        anyhow::ensure!(
            self.allowed.load(Ordering::Acquire),
            "Finish LocalFlow setup before recording meetings."
        );
        Ok(())
    }
}

#[cfg(test)]
#[test]
fn stalled_chat_cannot_starve_final_meeting_audio_or_quit() {
    let _writes = crate::shutdown::TestWrites::new();
    use crate::backends::{
        http::{HttpMeetingClient, HttpTransport},
        meeting::MeetingClient,
    };
    use lf_server::backend::{Backend, BackendRequest, BackendResponse};
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };
    struct Speech(AtomicBool);
    impl SpeechEngine for Speech {
        fn loaded(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
        fn load(&self) -> Result<(), String> {
            self.0.store(true, Ordering::Release);
            Ok(())
        }
        fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
            if self.loaded() {
                Ok("final buffered audio".into())
            } else {
                Err("speech closed early".into())
            }
        }
        fn unload(&self) {
            self.0.store(false, Ordering::Release);
        }
    }
    struct Meeting {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        speech: Arc<dyn SpeechEngine>,
        saved: mpsc::Sender<String>,
    }
    impl Backend for Meeting {
        fn handle(&self, _: BackendRequest) -> BackendResponse {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            BackendResponse {
                status: 200,
                body: serde_json::json!({}),
            }
        }
        fn shutdown(&self, _: &Config) -> Result<()> {
            let text = self
                .speech
                .transcribe(&[0.5], None)
                .map_err(anyhow::Error::msg)?;
            self.saved.send(text).unwrap();
            Ok(())
        }
    }
    let (entered, entering) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let memory = crate::speech_memory::SpeechMemory::new();
    let engine = Arc::new(Speech(AtomicBool::new(true)));
    let speech = memory.speech(engine.clone());
    let (saved, saving) = mpsc::channel();
    let state = Arc::new(ServerState::new(
        Config::default(),
        "test-token".into(),
        Arc::new(Meeting {
            entered,
            release: Mutex::new(blocked),
            saved,
            speech,
        }),
    ));
    let (mut handle, address) = ServerHandle::bind("127.0.0.1".into(), 0, state).unwrap();
    let client = HttpMeetingClient(
        HttpTransport::new(&format!("http://{address}"), || Ok("test-token".into())).unwrap(),
    );
    let (reply, response) = mpsc::channel();
    let chat = std::thread::spawn(move || {
        reply
            .send(client.chat("never returns during quit"))
            .unwrap();
    });
    entering.recv_timeout(Duration::from_secs(5)).unwrap();
    let (finish, done) = mpsc::channel();
    let started = Instant::now();
    handle.shutdown_then(move || {
        memory.shutdown();
        finish.send(()).unwrap();
    });
    let finalized = saving.recv_timeout(Duration::from_secs(5));
    let bounded = done.recv_timeout(Duration::from_secs(6)).is_ok();
    let aborted = response
        .recv_timeout(Duration::from_secs(2))
        .map(|result| result.is_err())
        .unwrap_or(false);
    release.send(()).unwrap();
    chat.join().unwrap();
    if !bounded {
        done.recv_timeout(Duration::from_secs(6)).unwrap();
    }
    assert!(bounded, "ordinary chat exhausted the meeting/quit budget");
    assert_eq!(finalized.unwrap(), "final buffered audio");
    assert!(!engine.loaded());
    assert!(aborted, "stalled HTTP request was not aborted");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[cfg(test)]
#[test]
fn closing_notes_queued_behind_autosave_are_acknowledged_before_http_stops() {
    let _writes = crate::shutdown::TestWrites::new();
    use crate::backends::{
        http::{HttpMeetingClient, HttpTransport},
        meeting::MeetingNotesWriter,
    };
    use lf_server::backend::{Backend, BackendRequest, BackendResponse};
    use std::{sync::mpsc, time::Duration};
    struct Notes {
        events: Arc<Mutex<Vec<String>>>,
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl Backend for Notes {
        fn handle(&self, request: BackendRequest) -> BackendResponse {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let notes = body["notes"].as_str().unwrap();
            if notes == "autosave" {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
            self.events.lock().unwrap().push(notes.into());
            BackendResponse {
                status: 200,
                body: serde_json::json!({}),
            }
        }
        fn shutdown(&self, _: &Config) -> Result<()> {
            self.events.lock().unwrap().push("backend drained".into());
            Ok(())
        }
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    let (entered, entering) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let state = Arc::new(ServerState::new(
        Config::default(),
        "test-token".into(),
        Arc::new(Notes {
            events: events.clone(),
            entered,
            release: Mutex::new(blocked),
        }),
    ));
    let serving = state.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let (finished, drained) = mpsc::channel();
    let (bound, address) = mpsc::channel();
    let (observed, stopping) = mpsc::channel();
    let log = events.clone();
    let server = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let server = Server::bind_with_state("127.0.0.1", 0, serving)
                    .await
                    .unwrap();
                bound.send(server.local_addr().unwrap()).unwrap();
                server
                    .run_until(async move {
                        let _ = stopped.await;
                        log.lock().unwrap().push("HTTP stopped".into());
                        observed.send(()).unwrap();
                    })
                    .await
                    .unwrap();
            });
        finished.send(()).unwrap();
    });
    let base = format!("http://{}", address.recv().unwrap());
    let client = Arc::new(HttpMeetingClient(
        HttpTransport::new(&base, || Ok("test-token".into())).unwrap(),
    ));
    let writer = Arc::new(MeetingNotesWriter::new());
    let first = writer.ticket();
    let autosave_guard = crate::shutdown::register_pending_write();
    let save_writer = writer.clone();
    let save_client = client.clone();
    let autosave = std::thread::spawn(move || {
        let _guard = autosave_guard;
        save_writer.save(&*save_client, first, "autosave")
    });
    entering.recv_timeout(Duration::from_secs(5)).unwrap();
    let last = writer.ticket();
    let closing_guard = crate::shutdown::register_pending_write();
    let closing = std::thread::spawn(move || {
        let _guard = closing_guard;
        writer.save(&*client, last, "closing flush")
    });
    let mut handle = ServerHandle {
        stop: Some(stop),
        state,
        drained: Some(drained),
    };
    let (finish, done) = mpsc::channel();
    let log = events.clone();
    handle.shutdown_then(move || {
        log.lock().unwrap().push("speech closed".into());
        finish.send(()).unwrap();
    });
    let premature = stopping.recv_timeout(Duration::from_millis(20)).is_ok();
    release.send(()).unwrap();
    let autosaved = autosave.join().unwrap();
    let closed = closing.join().unwrap();
    done.recv_timeout(Duration::from_secs(6)).unwrap();
    server.join().unwrap();
    assert!(
        !premature,
        "HTTP stopped while closing notes were queued behind autosave"
    );
    autosaved.unwrap();
    closed.unwrap();
    let events = events.lock().unwrap();
    let position = |event: &str| events.iter().position(|value| value == event).unwrap();
    assert!(position("autosave") < position("closing flush"));
    assert!(position("closing flush") < position("HTTP stopped"));
    assert!(position("closing flush") < position("backend drained"));
    assert!(position("backend drained") < position("speech closed"));
    assert!(position("HTTP stopped") < position("speech closed"));
}

#[test]
#[cfg(test)]
fn shutdown_keeps_speech_available_for_final_meeting_audio() {
    let _writes = crate::shutdown::TestWrites::new();
    use lf_server::backend::{Backend, BackendRequest, BackendResponse};
    use std::sync::mpsc;
    use std::time::Duration;
    struct Speech(mpsc::Sender<()>);
    impl SpeechEngine for Speech {
        fn load(&self) -> Result<(), String> {
            Ok(())
        }
        fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
            Ok("final chunk".into())
        }
        fn unload(&self) {
            let _ = self.0.send(());
        }
    }
    struct Meeting {
        speech: Arc<dyn SpeechEngine>,
        release: Mutex<mpsc::Receiver<()>>,
        saved: mpsc::Sender<Result<String, String>>,
    }
    impl Backend for Meeting {
        fn handle(&self, _: BackendRequest) -> BackendResponse {
            unreachable!()
        }
        fn shutdown(&self, _: &Config) -> Result<()> {
            self.release.lock().unwrap().recv().unwrap();
            self.saved
                .send(self.speech.transcribe(&[0.5], None))
                .unwrap();
            Ok(())
        }
    }
    let memory = crate::speech_memory::SpeechMemory::new();
    let (unload, unloaded) = mpsc::channel();
    let speech = memory.speech(Arc::new(Speech(unload)));
    let (release, wait) = mpsc::channel();
    let (save, saved) = mpsc::channel();
    let state = Arc::new(ServerState::new(
        Config::default(),
        String::new(),
        Arc::new(Meeting {
            speech: speech.clone(),
            release: Mutex::new(wait),
            saved: save,
        }),
    ));
    let (stop, _stopped) = tokio::sync::oneshot::channel();
    let (finish, drained) = mpsc::channel();
    let mut runtime = EmbeddedRuntime {
        shutting_down: false,
        runtime: None,
        server: Some(ServerHandle {
            stop: Some(stop),
            state,
            drained: Some(drained),
        }),
        speech,
        memory,
        server_watch: None,
        meeting_watch: None,
        dictation: None,
    };
    runtime.shutdown();
    runtime.shutdown();
    let premature = unloaded.try_recv().is_ok();
    assert!(
        unloaded.recv_timeout(Duration::from_millis(20)).is_err(),
        "speech shut down before HTTP drain"
    );
    finish.send(()).unwrap();
    assert!(
        unloaded.recv_timeout(Duration::from_millis(20)).is_err(),
        "speech shut down before meeting drain"
    );
    release.send(()).unwrap();
    let result = saved.recv_timeout(Duration::from_secs(6)).unwrap();
    assert!(!premature, "speech was unloaded before meeting drain");
    assert_eq!(result.unwrap(), "final chunk");
    unloaded.recv_timeout(Duration::from_secs(6)).unwrap();
}

#[cfg(test)]
#[test]
fn shutdown_flushes_notes_before_backend_and_signals_even_on_save_error() {
    let _writes = crate::shutdown::TestWrites::new();
    use lf_server::backend::{Backend, BackendRequest, BackendResponse};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Save(Arc<AtomicBool>);
    impl Backend for Save {
        fn handle(&self, _: BackendRequest) -> BackendResponse {
            unreachable!()
        }
        fn shutdown(&self, _: &Config) -> Result<()> {
            self.0.store(true, Ordering::SeqCst);
            anyhow::bail!("save failed")
        }
    }
    let notes = crate::shutdown::register_pending_write();
    let saved = Arc::new(AtomicBool::new(false));
    let state = Arc::new(ServerState::new(
        Config::default(),
        String::new(),
        Arc::new(Save(saved.clone())),
    ));
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let (finished, done) = std::sync::mpsc::channel();
    let mut handle = ServerHandle {
        stop: Some(stop),
        state,
        drained: None,
    };
    handle.shutdown_then(move || {
        let _ = finished.send(());
    });
    handle.shutdown();
    assert!(!saved.load(Ordering::SeqCst));
    assert!(stopped.try_recv().is_err());
    drop(notes);
    done.recv_timeout(std::time::Duration::from_secs(6))
        .unwrap();
    stopped.blocking_recv().unwrap();
    assert!(saved.load(Ordering::SeqCst));
}

struct MeetingSpeech(Held);
impl lf_meetings::Transcriber for MeetingSpeech {
    fn transcribe(&self, audio: &[f32], prompt: Option<&str>) -> Result<String> {
        self.0.transcribe(audio, prompt).map_err(anyhow::Error::msg)
    }
}
impl lf_meetings_backend::CaptureFactory for Capture {
    fn transcriber(&self, _: &Config) -> Result<Arc<dyn lf_meetings::Transcriber>> {
        self.ensure_allowed()?;
        let speech = self.speech.current();
        speech.load().map_err(anyhow::Error::msg)?;
        Ok(Arc::new(MeetingSpeech(speech)))
    }
    fn microphone(&self, config: &Config) -> Result<Box<dyn lf_meetings::AudioSource>> {
        self.ensure_allowed()?;
        Ok(Box::new(
            lf_meetings::system_audio::CpalSource::microphone_named(
                config.field_str("input_device"),
            )?,
        ))
    }
    fn system_audio(&self, config: &Config) -> Result<Option<Box<dyn lf_meetings::AudioSource>>> {
        self.ensure_allowed()?;
        Ok(Some(lf_meetings::system_audio::open_system_audio(
            16000,
            config.field_str("system_audio_device").unwrap_or_default(),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn composed_backend_reports_compiled_capabilities_before_loading() {
        use lf_server::backend::Backend;
        let config = Config::default();
        let speech = Arc::new(lf_stt::runtime::LazySpeechAdapter::new(config.clone()));
        let backend = lf_meetings_backend::MeetingsBackend::new()
            .unwrap()
            .with_inner(Arc::new(dictation_backend(speech.clone(), &config)));
        for provider in [
            "auto",
            "faster-whisper",
            "whisper-cpp",
            "parakeet",
            "parakeet-onnx",
            "",
            "unsupported",
        ] {
            assert_eq!(
                backend.stt_available(provider),
                lf_stt::factory::backend_available(provider),
                "{provider}"
            );
        }
        assert!(!speech.loaded());
        assert!(!backend.transcriber_loaded());
    }

    #[test]
    fn server_only_speech_tracks_settings_lazily_after_meeting_borrow_ends() {
        // Records loaded models: only the meeting's own engine may load.
        struct Engine(String, Arc<Mutex<Vec<String>>>);
        impl SpeechEngine for Engine {
            fn loaded(&self) -> bool {
                self.1.lock().unwrap().contains(&self.0)
            }
            fn load(&self) -> Result<(), String> {
                self.1.lock().unwrap().push(self.0.clone());
                Ok(())
            }
            fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
                Ok(self.0.clone())
            }
            fn unload(&self) {}
        }
        let loads = Arc::new(Mutex::new(Vec::new()));
        let record = loads.clone();
        let factory: lf_runtime::worker::SttFactory = Arc::new(move |config| {
            let model = config.field_str("model_size").unwrap_or_default();
            if model == "invalid" {
                return Err("selection failed".into());
            }
            Ok(Arc::new(Engine(model.into(), record.clone())))
        });
        let original = Config::default();
        let shared = SharedSpeech::new(factory(&original).unwrap());
        let previous = Mutex::new(original.clone());
        let capture = Capture {
            speech: shared.clone(),
            allowed: Arc::new(AtomicBool::new(true)),
        };
        let original_model = original.field_str("model_size").unwrap().to_owned();
        let meeting =
            lf_meetings_backend::CaptureFactory::transcriber(&capture, &original).unwrap();
        assert_eq!(*loads.lock().unwrap(), [original_model.as_str()]);
        let mut fresh = original.clone();
        fresh.set_field("model_size", serde_json::json!("new-model"));
        refresh_server_speech(&previous, &shared, fresh.clone(), &factory).unwrap();
        // The meeting keeps its engine and new requests stay on it until it ends.
        assert_eq!(meeting.transcribe(&[], None).unwrap(), original_model);
        assert_eq!(shared.transcribe(&[], None).unwrap(), original_model);
        assert_eq!(shared.latest().transcribe(&[], None).unwrap(), "new-model");
        drop(meeting);
        assert_eq!(shared.transcribe(&[], None).unwrap(), "new-model");
        assert!(!shared.loaded());
        let same = shared.current();
        fresh.set_field("sounds_enabled", serde_json::json!(false));
        refresh_server_speech(&previous, &shared, fresh.clone(), &factory).unwrap();
        assert!(Arc::ptr_eq(same.engine(), shared.current().engine()));
        drop(same);
        fresh.set_field("model_size", serde_json::json!("invalid"));
        assert!(refresh_server_speech(&previous, &shared, fresh, &factory).is_err());
        assert_eq!(shared.transcribe(&[], None).unwrap(), "new-model");
        assert_eq!(
            previous.lock().unwrap().field_str("model_size"),
            Some("new-model")
        );
        // The replacement stayed lazy; only the meeting's model ever loaded.
        assert_eq!(*loads.lock().unwrap(), [original_model]);
    }

    #[test]
    fn meeting_transcriber_loads_the_model_or_refuses_before_capture() {
        use lf_meetings_backend::CaptureFactory;
        use std::sync::atomic::AtomicUsize;
        #[derive(Default)]
        struct Engine {
            fail: AtomicBool,
            loads: AtomicUsize,
            loaded: AtomicBool,
        }
        impl SpeechEngine for Engine {
            fn loaded(&self) -> bool {
                self.loaded.load(Ordering::SeqCst)
            }
            fn load(&self) -> Result<(), String> {
                self.loads.fetch_add(1, Ordering::SeqCst);
                if self.fail.load(Ordering::SeqCst) {
                    return Err("model not downloaded".into());
                }
                self.loaded.store(true, Ordering::SeqCst);
                Ok(())
            }
            fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
                if !self.loaded() {
                    return Err("transcribed before load".into());
                }
                Ok("heard".into())
            }
            fn unload(&self) {}
        }
        let capture = |engine: &Arc<Engine>, allowed: bool| Capture {
            speech: SharedSpeech::new(engine.clone()),
            allowed: Arc::new(AtomicBool::new(allowed)),
        };
        let config = Config::default();

        // Setup unfinished: refused without loading.
        let engine = Arc::new(Engine::default());
        assert!(capture(&engine, false).transcriber(&config).is_err());
        assert_eq!(engine.loads.load(Ordering::SeqCst), 0);

        // A model that cannot load refuses the start with its error.
        let engine = Arc::new(Engine::default());
        engine.fail.store(true, Ordering::SeqCst);
        let failing = capture(&engine, true);
        let error = failing.transcriber(&config).err().expect("load failed");
        assert_eq!(error.to_string(), "model not downloaded");
        assert_eq!(engine.loads.load(Ordering::SeqCst), 1);
        // The failed borrow went back: only this test and the slot hold it.
        assert_eq!(Arc::strong_count(&engine), 2);
        // The shared slot stays usable once the model can load.
        engine.fail.store(false, Ordering::SeqCst);
        let meeting = failing.transcriber(&config).expect("model loads");
        assert_eq!(engine.loads.load(Ordering::SeqCst), 2);
        assert_eq!(meeting.transcribe(&[], None).unwrap(), "heard");

        // A loadable model is loaded exactly once, before any transcription.
        let engine = Arc::new(Engine::default());
        let meeting = capture(&engine, true)
            .transcriber(&config)
            .expect("model loads");
        assert_eq!(engine.loads.load(Ordering::SeqCst), 1);
        assert!(engine.loaded());
        assert_eq!(meeting.transcribe(&[0.1], None).unwrap(), "heard");
        assert_eq!(engine.loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn new_server_initializes_only_missing_or_empty_tokens_and_reports_file_errors() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("api-token");
        ensure_server_token(&path).unwrap();
        let token = lf_core::apitoken::load_at(&path).unwrap();
        assert!(!token.is_empty());
        ensure_server_token(&path).unwrap();
        assert_eq!(lf_core::apitoken::load_at(&path).unwrap(), token);
        std::fs::write(&path, " \n").unwrap();
        ensure_server_token(&path).unwrap();
        assert!(!lf_core::apitoken::load_at(&path).unwrap().is_empty());
        std::fs::write(&path, [255]).unwrap();
        assert!(ensure_server_token(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), [255]);
        assert!(ensure_server_token(&path.join("blocked")).is_err());
    }

    #[test]
    fn server_tracks_only_successful_speech_replacements_and_old_unload_is_isolated() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Engine(&'static str, bool, AtomicBool);
        impl SpeechEngine for Engine {
            fn loaded(&self) -> bool {
                self.2.load(Ordering::Acquire)
            }
            fn load(&self) -> std::result::Result<(), String> {
                if self.1 {
                    return Err("model unavailable".into());
                }
                self.2.store(true, Ordering::Release);
                Ok(())
            }
            fn transcribe(
                &self,
                _: &[f32],
                _: Option<&str>,
            ) -> std::result::Result<String, String> {
                Ok(self.0.into())
            }
            fn unload(&self) {
                self.2.store(false, Ordering::Release);
            }
        }
        let old = Arc::new(Engine("old", false, AtomicBool::new(false)));
        let shared = SharedSpeech::new(old.clone());
        assert!(!shared.loaded());
        assert_eq!(shared.transcribe(&[], None).unwrap(), "old");
        let replacement =
            PublishSpeech(shared.hold(Arc::new(Engine("new", false, AtomicBool::new(false)))));
        replacement.load().unwrap();
        old.unload();
        assert!(shared.loaded());
        assert_eq!(shared.transcribe(&[0.1], None).unwrap(), "new");
        let failed =
            PublishSpeech(shared.hold(Arc::new(Engine("bad", true, AtomicBool::new(false)))));
        assert!(failed.load().is_err());
        assert!(shared.loaded());
        assert_eq!(shared.transcribe(&[], None).unwrap(), "new");
        replacement.unload();
        assert!(!shared.loaded());
    }

    #[test]
    fn composed_server_has_health_and_status_and_healthy_owner_is_never_replaced() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.toml");
        lf_core::config::save_config(&Config::default(), Some(&config_path)).unwrap();
        let writer_path = config_path.clone();
        let reader_path = config_path.clone();
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fail_write = fail.clone();
        let (ready, address) = mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let state = Arc::new(
                        ServerState::new(
                            Config::default(),
                            "test-token".into(),
                            Arc::new(lf_server::backend::UnavailableBackend),
                        )
                        .with_config_reader(Arc::new(move || {
                            lf_core::config::load_from(&reader_path)
                        }))
                        .with_config_updater(Arc::new(move |update| {
                            anyhow::ensure!(
                                !fail_write.load(std::sync::atomic::Ordering::Acquire),
                                "save failed"
                            );
                            crate::config_write::update(Some(&writer_path), |fresh| {
                                lf_core::config::apply_config_update(fresh, update)
                            })
                        })),
                    );
                    let server = Server::bind_with_state("127.0.0.1", 0, state.clone())
                        .await
                        .unwrap();
                    let addr = server.local_addr().unwrap();
                    assert!(Server::bind_with_state("127.0.0.1", addr.port(), state)
                        .await
                        .is_err());
                    ready.send(addr).unwrap();
                    server
                        .run_until(async move {
                            let _ = stopped.await;
                        })
                        .await
                        .unwrap();
                });
        });
        let address = address.recv_timeout(Duration::from_secs(5)).unwrap();
        let base = format!("http://{address}");
        assert!(healthy(&base));
        assert!(ensure_server("127.0.0.1", address.port(), || panic!(
            "healthy owner must not be replaced"
        ))
        .unwrap()
        .is_none());
        let response = reqwest::blocking::Client::new()
            .get(format!("{base}/api/status"))
            .header("X-Localflow-Token", "test-token")
            .send()
            .unwrap();
        assert!(response.status().is_success());
        assert_eq!(
            response.json::<serde_json::Value>().unwrap()["stt"]["loaded"],
            false
        );
        crate::config_write::update(Some(&config_path), |fresh| {
            let mut updated = fresh.clone();
            updated.set_field("sounds_enabled", serde_json::json!(false));
            Ok(updated)
        })
        .unwrap();
        let client = reqwest::blocking::Client::new();
        let update = |body: serde_json::Value| {
            client
                .put(format!("{base}/api/config"))
                .header("X-Localflow-Token", "test-token")
                .json(&body)
                .send()
                .unwrap()
        };
        assert!(update(serde_json::json!({"dictation":{"enabled":false}}))
            .status()
            .is_success());
        let saved = lf_core::config::load_from(&config_path).unwrap();
        assert_eq!(saved.field_bool("sounds_enabled"), Some(false));
        assert_eq!(saved.field_bool("dictation_enabled"), Some(false));
        assert!(update(serde_json::json!({})).status().is_success());
        fail.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            update(serde_json::json!({"dictation":{"enabled":true}}))
                .status()
                .as_u16(),
            500
        );
        assert_eq!(
            lf_core::config::load_from(&config_path)
                .unwrap()
                .field_bool("dictation_enabled"),
            Some(false)
        );
        crate::config_write::update(Some(&config_path), |fresh| {
            let mut updated = fresh.clone();
            updated.set_field("dictation_enabled", serde_json::json!(true));
            Ok(updated)
        })
        .unwrap();
        let current = || {
            client
                .get(format!("{base}/api/config"))
                .header("X-Localflow-Token", "test-token")
                .send()
                .unwrap()
        };
        assert_eq!(
            current().json::<serde_json::Value>().unwrap()["dictation"]["enabled"],
            true
        );
        std::fs::write(&config_path, "invalid = [").unwrap();
        assert_eq!(current().status().as_u16(), 503);
        assert!(healthy(&base));
        let _ = stop.send(());
        thread.join().unwrap();
    }

    /// Panics on load unless it counts loads (for a test that starts a meeting,
    /// which legitimately loads); either way activation can be checked lazy.
    struct ModelEngine(String, Option<Arc<std::sync::atomic::AtomicUsize>>);
    impl SpeechEngine for ModelEngine {
        fn loaded(&self) -> bool {
            false
        }
        fn load(&self) -> Result<(), String> {
            let loads = self.1.as_ref().expect("activation must keep speech lazy");
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
            Ok(self.0.clone())
        }
        fn unload(&self) {}
    }

    struct SilentAudio;
    impl lf_runtime::engine::AudioSource for SilentAudio {
        fn start(&mut self) -> Result<(), String> {
            panic!("activation must not open the microphone")
        }
        fn stop(&mut self) -> Vec<f32> {
            Vec::new()
        }
        fn is_recording(&self) -> bool {
            false
        }
    }

    /// A server-only runtime on an ephemeral port with fake lazy speech: no
    /// user config, token file, real port or microphone is touched.
    type ServerOnly = (EmbeddedRuntime, String, Capture, Arc<Mutex<Vec<Config>>>);

    fn server_only(startup: Config) -> ServerOnly {
        server_only_with(startup, by_model(None))
    }

    fn by_model(
        loads: Option<Arc<std::sync::atomic::AtomicUsize>>,
    ) -> lf_runtime::worker::SttFactory {
        Arc::new(move |config| {
            Ok(Arc::new(ModelEngine(
                config.field_str("model_size").unwrap_or_default().into(),
                loads.clone(),
            )))
        })
    }

    fn server_only_with(startup: Config, factory: lf_runtime::worker::SttFactory) -> ServerOnly {
        let shared = SharedSpeech::new(factory(&startup).unwrap());
        let state = Arc::new(ServerState::new(
            Config::default(),
            "test-token".into(),
            Arc::new(lf_server::backend::UnavailableBackend),
        ));
        let (server, address) = ServerHandle::bind("127.0.0.1".into(), 0, state).unwrap();
        let meetings = Arc::new(AtomicBool::new(false));
        let watched = Arc::new(Mutex::new(Vec::new()));
        let record = watched.clone();
        let runtime = EmbeddedRuntime {
            shutting_down: false,
            runtime: None,
            server: Some(server),
            speech: Arc::new(shared.clone()),
            memory: crate::speech_memory::SpeechMemory::new(),
            server_watch: None,
            meeting_watch: None,
            dictation: Some(Dictation {
                menu: MenuState::new(None, true),
                shared: shared.clone(),
                make_speech: factory,
                selection: Arc::new(Mutex::new(startup)),
                server_owns_speech: Arc::new(AtomicBool::new(true)),
                meetings: meetings.clone(),
                meeting_config: Some(Arc::new(move |config| record.lock().unwrap().push(config))),
            }),
        };
        let capture = Capture {
            speech: shared,
            allowed: meetings,
        };
        (runtime, format!("http://{address}"), capture, watched)
    }

    fn toggle() -> ToggleStore {
        Arc::new(|| -> Result<bool, String> { panic!("activation must not toggle dictation") })
    }

    #[test]
    fn server_serves_health_before_activation_which_uses_fresh_config_once() {
        use lf_meetings_backend::CaptureFactory;
        let _writes = crate::shutdown::TestWrites::new();
        let mut startup = Config::default();
        startup.set_field("model_size", serde_json::json!("setup-default"));
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (mut runtime, base, capture, watched) =
            server_only_with(startup.clone(), by_model(Some(loads.clone())));
        // Setup phase: HTTP is up, nothing that records or listens is.
        assert!(healthy(&base));
        assert!(!runtime.activated());
        assert!(runtime.runtime.is_none());
        assert!(capture.transcriber(&startup).is_err());
        assert!(capture.microphone(&startup).is_err());
        assert!(capture.system_audio(&startup).is_err());
        assert!(watched.lock().unwrap().is_empty());
        // Finish saved a different microphone and model.
        let mut fresh = startup.clone();
        fresh.set_field("model_size", serde_json::json!("chosen-model"));
        fresh.set_field("input_device", serde_json::json!("Chosen Mic"));
        fresh.set_field("dictation_enabled", serde_json::json!(false));
        let opened = Arc::new(Mutex::new(None));
        let opened_by = opened.clone();
        let started = Arc::new(Mutex::new(None));
        let started_by = started.clone();
        let health_at_start = base.clone();
        assert!(runtime.activate_with(
            fresh.clone(),
            toggle(),
            move |config| {
                *opened_by.lock().unwrap() = config.field_str("input_device").map(str::to_owned);
                Ok(Box::new(SilentAudio))
            },
            move |options, menu, toggle| {
                *started_by.lock().unwrap() = Some((
                    options.speech.transcribe(&[], None).unwrap(),
                    menu.snapshot().dictation_enabled,
                    toggle.is_some(),
                    healthy(&health_at_start),
                ));
                Ok(None)
            },
        ));
        assert_eq!(opened.lock().unwrap().as_deref(), Some("Chosen Mic"));
        assert_eq!(
            started.lock().unwrap().take(),
            Some(("chosen-model".into(), false, true, true)),
            "dictation must start with the saved config on the running server"
        );
        assert!(runtime.activated());
        // The server kept serving and now shares the activated selection.
        assert!(healthy(&base));
        assert_eq!(
            runtime.speech.transcribe(&[], None).unwrap(),
            "chosen-model"
        );
        // Activation kept speech lazy; starting a meeting is what loads it.
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        assert!(capture.transcriber(&fresh).is_ok());
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert_eq!(watched.lock().unwrap().len(), 1);
        assert_eq!(
            watched.lock().unwrap()[0].field_str("input_device"),
            Some("Chosen Mic")
        );
        // Repeated Finish/Hub requests never start a second dictation runtime.
        assert!(runtime.activate_with(
            fresh,
            toggle(),
            |_| panic!("second activation opened audio"),
            |_, _, _| panic!("second activation started dictation"),
        ));
        assert_eq!(watched.lock().unwrap().len(), 1);
        runtime.shutdown();
        assert!(!runtime.activate_with(
            startup,
            toggle(),
            |_| panic!("activation after shutdown"),
            |_, _, _| panic!("activation after shutdown"),
        ));
    }

    #[test]
    fn failed_activation_keeps_the_server_and_a_retry_uses_the_newer_config() {
        let _writes = crate::shutdown::TestWrites::new();
        let (mut runtime, base, _capture, watched) = server_only(Config::default());
        let mut first = Config::default();
        first.set_field("model_size", serde_json::json!("first"));
        assert!(!runtime.activate_with(
            first,
            toggle(),
            |_| anyhow::bail!("microphone permission denied"),
            |_, _, _| panic!("dictation started without audio"),
        ));
        assert!(!runtime.activated());
        assert!(healthy(&base));
        let warning = runtime
            .dictation
            .as_ref()
            .unwrap()
            .menu
            .snapshot()
            .input_warning
            .unwrap();
        assert!(
            warning.contains("microphone permission denied"),
            "{warning}"
        );
        let mut retry = Config::default();
        retry.set_field("model_size", serde_json::json!("retry"));
        let started = Arc::new(Mutex::new(None));
        let started_by = started.clone();
        assert!(runtime.activate_with(
            retry,
            toggle(),
            |_| Ok(Box::new(SilentAudio)),
            move |options, _, _| {
                *started_by.lock().unwrap() = Some(options.speech.transcribe(&[], None).unwrap());
                Ok(None)
            },
        ));
        assert_eq!(started.lock().unwrap().as_deref(), Some("retry"));
        assert!(runtime.activated());
        assert!(healthy(&base));
        // Meeting watching was enabled once, by the first attempt.
        assert_eq!(watched.lock().unwrap().len(), 1);
    }

    /// Every engine is distinct ("model#n", counted in `built`) and panics on
    /// load, so assertions see engine identity and no desktop load can happen.
    /// The model "invalid" fails selection.
    fn numbered(built: Arc<std::sync::atomic::AtomicUsize>) -> lf_runtime::worker::SttFactory {
        Arc::new(move |config| {
            let model = config.field_str("model_size").unwrap_or_default();
            if model == "invalid" {
                return Err("selection failed".into());
            }
            let n = built.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Arc::new(ModelEngine(format!("{model}#{n}"), None)))
        })
    }

    fn model(name: &str) -> Config {
        let mut config = Config::default();
        config.set_field("model_size", serde_json::json!(name));
        config
    }

    type Recorded = Arc<Mutex<Option<Arc<dyn SpeechEngine>>>>;

    /// Activates with fake audio; the dictation speech is recorded and
    /// `Ok(None)` returned, so activation itself never hands ownership off.
    fn activate_recording(runtime: &mut EmbeddedRuntime, config: Config) -> (bool, Recorded) {
        let recorded: Recorded = Arc::new(Mutex::new(None));
        let started_with = recorded.clone();
        let activated = runtime.activate_with(
            config,
            toggle(),
            |_| Ok(Box::new(SilentAudio)),
            move |options, _, _| {
                *started_with.lock().unwrap() = Some(options.speech);
                Ok(None)
            },
        );
        (activated, recorded)
    }

    #[test]
    fn activation_without_in_flight_request_shares_one_engine_immediately() {
        use std::sync::atomic::AtomicUsize;
        let _writes = crate::shutdown::TestWrites::new();
        let built = Arc::new(AtomicUsize::new(0));
        let (mut runtime, _base, _capture, _watched) =
            server_only_with(model("old"), numbered(built.clone()));
        let owner = runtime
            .dictation
            .as_ref()
            .unwrap()
            .server_owns_speech
            .clone();
        let (activated, recorded) = activate_recording(&mut runtime, model("new"));
        assert!(activated);
        let dictation = recorded.lock().unwrap().take().unwrap();
        assert_eq!(dictation.transcribe(&[], None).unwrap(), "new#2");
        assert_eq!(runtime.speech.transcribe(&[], None).unwrap(), "new#2");
        assert_eq!(built.load(Ordering::SeqCst), 2, "one engine per selection");
        assert!(!dictation.loaded());
        // No runtime started in-process: the server keeps following settings.
        assert!(owner.load(Ordering::Acquire));
    }

    #[test]
    fn activation_during_in_flight_request_switches_server_once_it_ends() {
        use std::sync::atomic::AtomicUsize;
        let _writes = crate::shutdown::TestWrites::new();
        let built = Arc::new(AtomicUsize::new(0));
        let (mut runtime, _base, capture, _watched) =
            server_only_with(model("old"), numbered(built.clone()));
        let in_flight = capture.speech.current();
        let (activated, recorded) = activate_recording(&mut runtime, model("new"));
        assert!(activated);
        let dictation = recorded.lock().unwrap().take().unwrap();
        assert_eq!(dictation.transcribe(&[], None).unwrap(), "new#2");
        assert_eq!(runtime.speech.transcribe(&[], None).unwrap(), "old#1");
        drop(in_flight);
        // The next request promotes the pending engine, without any tick.
        assert_eq!(runtime.speech.transcribe(&[], None).unwrap(), "new#2");
        assert_eq!(built.load(Ordering::SeqCst), 2);
        assert!(!dictation.loaded());
    }

    #[test]
    fn failed_selection_during_in_flight_request_publishes_nothing_and_retry_works() {
        use std::sync::atomic::AtomicUsize;
        let _writes = crate::shutdown::TestWrites::new();
        let built = Arc::new(AtomicUsize::new(0));
        let (mut runtime, base, capture, _watched) =
            server_only_with(model("old"), numbered(built.clone()));
        let selection = runtime.dictation.as_ref().unwrap().selection.clone();
        let in_flight = capture.speech.current();
        assert!(!runtime.activate_with(
            model("invalid"),
            toggle(),
            |_| panic!("audio opened after failed selection"),
            |_, _, _| panic!("dictation started after failed selection"),
        ));
        assert!(!runtime.activated());
        assert!(healthy(&base));
        assert_eq!(
            selection.lock().unwrap().field_str("model_size"),
            Some("old")
        );
        drop(in_flight);
        assert_eq!(
            capture.speech.latest().transcribe(&[], None).unwrap(),
            "old#1"
        );
        assert_eq!(runtime.speech.transcribe(&[], None).unwrap(), "old#1");
        let (activated, recorded) = activate_recording(&mut runtime, model("retry"));
        assert!(activated);
        let dictation = recorded.lock().unwrap().take().unwrap();
        assert_eq!(dictation.transcribe(&[], None).unwrap(), "retry#2");
        assert_eq!(runtime.speech.transcribe(&[], None).unwrap(), "retry#2");
        assert_eq!(built.load(Ordering::SeqCst), 2);
    }

    /// (engine, finished): `false` when its destructor starts, `true` when done.
    type DropEvents = mpsc::Receiver<(&'static str, bool)>;

    /// A lazy fake engine that reports its destruction; with a gate, the
    /// destructor blocks until the gate is released, like a helper process
    /// that takes a while to exit.
    struct Tracked {
        name: &'static str,
        events: mpsc::Sender<(&'static str, bool)>,
        gate: Option<Mutex<mpsc::Receiver<()>>>,
    }

    impl SpeechEngine for Tracked {
        fn loaded(&self) -> bool {
            false
        }
        fn load(&self) -> Result<(), String> {
            panic!("retirement must not load speech")
        }
        fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
            Ok(self.name.into())
        }
        fn unload(&self) {}
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            let _ = self.events.send((self.name, false));
            if let Some(gate) = &self.gate {
                let _ = gate.lock().unwrap().recv();
            }
            let _ = self.events.send((self.name, true));
        }
    }

    fn tracked(
        events: &mpsc::Sender<(&'static str, bool)>,
        name: &'static str,
    ) -> Arc<dyn SpeechEngine> {
        Arc::new(Tracked {
            name,
            events: events.clone(),
            gate: None,
        })
    }

    fn next_event(events: &DropEvents) -> (&'static str, bool) {
        events
            .recv_timeout(Duration::from_secs(5))
            .expect("a retired engine was never released")
    }

    /// Waits until `names` have all been fully destroyed (in any order).
    fn expect_destroyed(events: &DropEvents, names: &[&'static str]) {
        let mut remaining: Vec<_> = names.to_vec();
        while !remaining.is_empty() {
            if let (name, true) = next_event(events) {
                let index = remaining
                    .iter()
                    .position(|expected| *expected == name)
                    .unwrap_or_else(|| panic!("unexpected destruction of {name}"));
                remaining.remove(index);
            }
        }
    }

    /// Runs `access` on another thread; fails instead of hanging if it blocks.
    fn returns<T: Send + 'static>(access: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, result) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(access());
        });
        result
            .recv_timeout(Duration::from_secs(5))
            .expect("speech access blocked behind a retiring engine")
    }

    #[test]
    fn newer_selections_supersede_pending_ones_and_reloads_drop_stale_pending() {
        let (sender, events) = mpsc::channel();
        let shared = SharedSpeech::new(tracked(&sender, "old"));
        let in_flight = shared.current();
        let first = tracked(&sender, "first");
        let first_weak = Arc::downgrade(&first);
        shared.publish(first);
        shared.publish(tracked(&sender, "second"));
        // The superseded pending engine is destroyed by the cleanup thread.
        expect_destroyed(&events, &["first"]);
        assert!(first_weak.upgrade().is_none());
        assert_eq!(shared.latest().transcribe(&[], None).unwrap(), "second");
        assert_eq!(shared.current().transcribe(&[], None).unwrap(), "old");
        // A dictation reload loads its engine and sets it, even while borrowed;
        // the older pending server selection must never be promoted over it.
        shared.set(tracked(&sender, "reloaded"));
        expect_destroyed(&events, &["second"]);
        // The request's reference to the replaced engine is the last one; it
        // goes to the cleanup thread too.
        drop(in_flight);
        expect_destroyed(&events, &["old"]);
        shared.settle();
        assert_eq!(shared.current().transcribe(&[], None).unwrap(), "reloaded");
        assert_eq!(shared.latest().transcribe(&[], None).unwrap(), "reloaded");
        assert!(events.try_recv().is_err(), "the selection was retired");
    }

    #[test]
    fn promotion_on_status_returns_while_old_engine_destructor_blocks() {
        let (sender, events) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let shared = SharedSpeech::new(Arc::new(Tracked {
            name: "old",
            events: sender.clone(),
            gate: Some(Mutex::new(gate)),
        }));
        // A borrow outside the holder (as a long upload would) defers the
        // new selection; ending it does not promote by itself.
        let borrowed = shared.current().engine().clone();
        shared.publish(tracked(&sender, "new"));
        drop(borrowed);
        assert!(events.try_recv().is_err());
        // Status (`loaded`) promotes; it must not wait for the old destructor.
        let status = shared.clone();
        assert!(!returns(move || status.loaded()));
        assert_eq!(next_event(&events), ("old", false), "destructor started");
        // Other speech access proceeds while the destructor is still blocked.
        let request = shared.clone();
        assert_eq!(
            returns(move || request.transcribe(&[], None).unwrap()),
            "new"
        );
        let selection = shared.clone();
        let replacement = tracked(&sender, "newer");
        returns(move || selection.publish(replacement));
        let request = shared.clone();
        assert_eq!(
            returns(move || request.transcribe(&[], None).unwrap()),
            "newer"
        );
        assert!(events.try_recv().is_err(), "old destructor still blocked");
        release.send(()).unwrap();
        assert_eq!(next_event(&events), ("old", true));
        // The cleanup thread then continues with later retirements.
        expect_destroyed(&events, &["new"]);
    }

    #[test]
    fn speech_access_without_replacement_retires_nothing() {
        let (sender, events) = mpsc::channel();
        let shared = SharedSpeech::new(tracked(&sender, "only"));
        let meeting = shared.current();
        let dictation = shared.latest();
        assert!(!shared.loaded());
        assert_eq!(shared.transcribe(&[], None).unwrap(), "only");
        shared.settle();
        drop(meeting);
        drop(dictation);
        shared.settle();
        assert_eq!(shared.current().transcribe(&[], None).unwrap(), "only");
        assert!(events.try_recv().is_err(), "nothing was destroyed");
        assert!(
            shared.0.reaper.lock().unwrap().sender.is_none(),
            "no cleanup thread without a retirement"
        );
    }

    #[test]
    fn stopped_cleanup_thread_is_restarted_without_dropping_inline() {
        let (sender, events) = mpsc::channel();
        let shared = SharedSpeech::new(tracked(&sender, "old"));
        let (dead, receiver) = mpsc::channel();
        drop(receiver);
        shared.0.reaper.lock().unwrap().sender = Some(dead);
        shared.publish(tracked(&sender, "new"));
        expect_destroyed(&events, &["old"]);
        let reaper = shared.0.reaper.lock().unwrap();
        assert!(reaper.parked.is_empty());
        assert!(reaper.sender.is_some());
    }

    #[test]
    fn handoff_stops_server_reloads_only_when_dictation_started() {
        use std::sync::atomic::AtomicUsize;
        let built = Arc::new(AtomicUsize::new(0));
        let factory = numbered(built.clone());
        let shared = SharedSpeech::new(factory(&model("old")).unwrap());
        let selection = Arc::new(Mutex::new(model("old")));
        let owner = Arc::new(AtomicBool::new(true));
        let (apply, _tick) = server_speech_updates(
            selection.clone(),
            shared.clone(),
            factory,
            owner.clone(),
            Arc::new(|_| {}),
            Arc::new(|| true),
        );
        hand_off_speech(&selection, &owner, false);
        assert!(owner.load(Ordering::Acquire));
        apply(model("server"));
        assert_eq!(shared.current().transcribe(&[], None).unwrap(), "server#2");
        hand_off_speech(&selection, &owner, true);
        assert!(!owner.load(Ordering::Acquire));
        apply(model("ignored"));
        assert_eq!(shared.current().transcribe(&[], None).unwrap(), "server#2");
        assert_eq!(
            built.load(Ordering::SeqCst),
            2,
            "no engine built after handoff"
        );
    }

    #[test]
    fn deferred_server_reload_follows_dictation_engine_after_in_flight_request_ends() {
        use std::sync::atomic::AtomicUsize;
        let _writes = crate::shutdown::TestWrites::new();
        let built = Arc::new(AtomicUsize::new(0));
        let factory = numbered(built.clone());
        let (mut runtime, _base, capture, _watched) =
            server_only_with(model("old"), factory.clone());
        let inactive = runtime.dictation.as_ref().unwrap();
        let (selection, server_owns_speech) = (
            inactive.selection.clone(),
            inactive.server_owns_speech.clone(),
        );
        // The server's settings watcher, minus the file watch on user config.
        let (apply, tick) = server_speech_updates(
            selection.clone(),
            capture.speech.clone(),
            factory,
            server_owns_speech.clone(),
            Arc::new(|_| {}),
            Arc::new(|| true),
        );
        assert_eq!(runtime.speech.transcribe(&[], None).unwrap(), "old#1");
        // An in-flight server upload: `SharedSpeech::transcribe` (behind the
        // server's `DictationBackend`) holds this clone for the whole request.
        let in_flight = capture.speech.current();
        // Setup saved a new model; the watcher sees it while the request runs.
        apply(model("new"));
        assert_eq!(
            runtime.speech.transcribe(&[], None).unwrap(),
            "old#1",
            "the borrowed engine cannot be replaced yet"
        );
        let (activated, recorded) = activate_recording(&mut runtime, model("new"));
        assert!(activated);
        // What activation does for `Ok(Some(RuntimeHandle))`: dictation started.
        hand_off_speech(&selection, &server_owns_speech, true);
        let dictation_speech = recorded.lock().unwrap().take().unwrap();
        let dictation_model = dictation_speech.transcribe(&[], None).unwrap();
        assert_eq!(dictation_model, "new#2");
        // The request finishes; the watcher's next tick runs.
        assert_eq!(Arc::strong_count(in_flight.engine()), 2);
        drop(in_flight);
        assert!(tick());
        assert_eq!(
            runtime.speech.transcribe(&[], None).unwrap(),
            dictation_model,
            "after the in-flight request ends the server must use the dictation \
             runtime's new engine, not keep the old model until a desktop load"
        );
        // Settings changes after the handoff are the dictation runtime's to
        // load; the server watcher no longer selects (or overwrites) speech.
        apply(model("later"));
        assert!(tick());
        assert_eq!(runtime.speech.transcribe(&[], None).unwrap(), "new#2");
        assert_eq!(built.load(Ordering::SeqCst), 2, "no duplicate engines");
        assert!(!dictation_speech.loaded(), "nothing may load speech");
    }

    #[test]
    fn malformed_empty_and_error_health_responses_are_not_healthy() {
        use std::io::{Read, Write};
        for (status, body) in [(200, "{}"), (200, "not json"), (500, r#"{"ok":true}"#)] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let thread = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = [0; 4096];
                let _ = stream.read(&mut request);
                let _ = write!(stream, "HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            });
            assert!(!healthy(&format!("http://{address}")));
            thread.join().unwrap();
        }
        assert!(!healthy("http://127.0.0.1:0"));
    }
}
