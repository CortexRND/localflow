//! In-process services; initialization and all native/model/network work run off AppKit.

use anyhow::Result;
use lf_core::config::Config;
use lf_runtime::{
    engine::SpeechEngine,
    menubar::MenuState,
    run::{RunOptions, RuntimeHandle, ToggleStore},
};
use lf_server::{Server, ServerState};
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

#[derive(Clone)]
struct SharedSpeech(Arc<Mutex<Arc<dyn SpeechEngine>>>);

impl SharedSpeech {
    fn current(&self) -> Arc<dyn SpeechEngine> {
        self.0.lock().unwrap().clone()
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

struct PublishSpeech {
    inner: Arc<dyn SpeechEngine>,
    destination: SharedSpeech,
}

fn refresh_server_speech(
    previous: &Mutex<Config>,
    shared: &SharedSpeech,
    fresh: Config,
    factory: &lf_runtime::worker::SttFactory,
) -> Result<bool, String> {
    let mut previous = previous.lock().unwrap();
    if lf_runtime::live::STT_FIELDS
        .iter()
        .any(|field| previous.field(field) != fresh.field(field))
    {
        let mut current = shared.0.lock().unwrap();
        if Arc::strong_count(&current) > 1 {
            return Ok(false);
        }
        *current = factory(&fresh)?;
    }
    *previous = fresh;
    Ok(true)
}

fn watch_server_services(
    config: Config,
    shared: SharedSpeech,
    factory: lf_runtime::worker::SttFactory,
    update_speech: bool,
    on_config: Arc<dyn Fn(Config) + Send + Sync>,
    maintenance: lf_runtime::live::OnTick,
) -> Result<lf_runtime::live::WatcherGuard> {
    let previous = Mutex::new(config);
    let pending = Arc::new(Mutex::new(None::<Config>));
    let retry_pending = pending.clone();
    let retry: lf_runtime::live::OnTick = Arc::new(move || {
        maintenance();
        let mut pending = retry_pending.lock().unwrap();
        if let Some(fresh) = pending.as_ref() {
            match refresh_server_speech(&previous, &shared, fresh.clone(), &factory) {
                Ok(true) => {
                    pending.take();
                }
                Ok(false) => {}
                Err(error) => log::error!("server speech selection failed: {error}"),
            }
        }
        true
    });
    let apply_retry = retry.clone();
    let apply: Arc<dyn Fn(Config) + Send + Sync> = Arc::new(move |fresh| {
        on_config(fresh.clone());
        if update_speech {
            *pending.lock().unwrap() = Some(fresh);
            apply_retry();
        }
    });
    let watch = lf_runtime::live::ConfigWatcher::new(
        lf_core::config::try_config_path()?,
        Arc::new(|| lf_runtime::settings::load_runtime_config(None)),
        apply.clone(),
    )
    .with_tick(retry, lf_runtime::live::DEFAULT_INTERVAL)
    .start();
    if let Ok(fresh) = lf_runtime::settings::load_runtime_config(None) {
        apply(fresh);
    }
    Ok(watch)
}

impl SpeechEngine for PublishSpeech {
    fn loaded(&self) -> bool {
        self.inner.loaded()
    }
    fn load(&self) -> std::result::Result<(), String> {
        self.inner.load()?;
        *self.destination.0.lock().unwrap() = self.inner.clone();
        Ok(())
    }
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> std::result::Result<String, String> {
        self.inner.transcribe(pcm, prompt)
    }
    fn unload(&self) {
        self.inner.unload();
    }
}

pub struct EmbeddedRuntime {
    shutting_down: bool,
    pub runtime: Option<RuntimeHandle>,
    server: Option<ServerHandle>,
    pub speech: Arc<dyn SpeechEngine>,
    memory: crate::speech_memory::SpeechMemory,
    server_watch: Option<lf_runtime::live::WatcherGuard>,
    meeting_watch: Option<meeting_watch::MeetingWatch>,
}

impl EmbeddedRuntime {
    pub fn start(config: Config, menu: MenuState, toggle: ToggleStore) -> Self {
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
        let shared = SharedSpeech(Arc::new(Mutex::new(initial.clone())));
        let speech: Arc<dyn SpeechEngine> = Arc::new(shared.clone());
        let server_shared = shared.clone();
        let reload_speech = make_speech.clone();
        let build_stt: lf_runtime::worker::SttFactory = Arc::new(move |config| {
            Ok(Arc::new(PublishSpeech {
                inner: reload_speech(config)?,
                destination: shared.clone(),
            }) as Arc<dyn SpeechEngine>)
        });
        let runtime = (|| -> Result<Option<RuntimeHandle>> {
            let options = RunOptions {
                audio: memory.audio(Box::new(lf_stt::runtime::RecorderSource::from_config(
                    &config,
                )?)),
                speech: initial,
                build_stt,
                play_sound: Arc::new(lf_runtime::sounds::play),
            };
            lf_runtime::run::start_with_menu(options, menu.clone(), Some(toggle))
        })()
        .unwrap_or_else(|e| {
            log::error!("dictation startup failed: {e}");
            menu.change(|m| m.input_warning = Some(format!("Dictation unavailable: {e}")));
            None
        });
        let mut meeting_watch = None;
        let server = (|| -> Result<Option<ServerHandle>> {
            let host = config.field_str("server_host").unwrap_or("127.0.0.1");
            let port = u16::try_from(config.field_i64("server_port").unwrap_or(8756))?;
            ensure_server(host, port, || {
                ensure_server_token(&lf_core::apitoken::try_token_path()?)?;
                let dictation = dictation_backend(speech.clone(), &config);
                let backend = lf_meetings_backend::MeetingsBackend::new()?
                    .with_inner(Arc::new(dictation))
                    .with_capture(Arc::new(Capture(server_shared.clone())));
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
            "in-process runtime: ready; speech loaded: {}",
            speech.loaded()
        );
        let live = Arc::new(Mutex::new(config.clone()));
        let apply_live = live.clone();
        let watcher_callback = meeting_watch
            .as_ref()
            .map(|watcher| watcher.apply_callback());
        let on_config = Arc::new(move |fresh: Config| {
            *apply_live.lock().unwrap() = fresh.clone();
            if let Some(callback) = &watcher_callback {
                callback(fresh);
            }
        });
        let server_state = server.as_ref().map(|server| server.state.clone());
        let server_watch = lf_core::config::try_config_path()
            .and_then(|path| {
                let settings = Mutex::new(crate::speech_memory::RuntimeSettings::new(&path));
                let maintenance_memory = memory.clone();
                let maintenance: lf_runtime::live::OnTick = Arc::new(move || {
                    let mut settings = settings.lock().unwrap();
                    settings.reload();
                    let enabled = live
                        .lock()
                        .unwrap()
                        .field_bool("dictation_enabled")
                        .unwrap_or(true);
                    let mut unload = || {
                        maintenance_memory.poll(
                            enabled,
                            settings.timeout,
                            std::time::Instant::now(),
                        );
                    };
                    if let Some(state) = &server_state {
                        state.while_idle(&mut unload);
                    } else {
                        unload();
                    }
                    true
                });
                watch_server_services(
                    config,
                    server_shared,
                    make_speech,
                    runtime.is_none(),
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
            runtime,
            server,
            speech,
            memory,
            server_watch,
            meeting_watch,
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

struct Capture(SharedSpeech);

#[cfg(test)]
#[test]
fn stalled_chat_cannot_starve_final_meeting_audio_or_quit() {
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

struct MeetingSpeech(Arc<dyn SpeechEngine>);
impl lf_meetings::Transcriber for MeetingSpeech {
    fn transcribe(&self, audio: &[f32], prompt: Option<&str>) -> Result<String> {
        self.0.transcribe(audio, prompt).map_err(anyhow::Error::msg)
    }
}
impl lf_meetings_backend::CaptureFactory for Capture {
    fn transcriber(&self, _: &Config) -> Result<Arc<dyn lf_meetings::Transcriber>> {
        Ok(Arc::new(MeetingSpeech(self.0.current())))
    }
    fn microphone(&self, config: &Config) -> Result<Box<dyn lf_meetings::AudioSource>> {
        Ok(Box::new(
            lf_meetings::system_audio::CpalSource::microphone_named(
                config.field_str("input_device"),
            )?,
        ))
    }
    fn system_audio(&self, config: &Config) -> Result<Option<Box<dyn lf_meetings::AudioSource>>> {
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
        struct Engine(String);
        impl SpeechEngine for Engine {
            fn loaded(&self) -> bool {
                false
            }
            fn load(&self) -> Result<(), String> {
                panic!("must remain lazy")
            }
            fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
                Ok(self.0.clone())
            }
            fn unload(&self) {}
        }
        let factory: lf_runtime::worker::SttFactory = Arc::new(|config| {
            let model = config.field_str("model_size").unwrap_or_default();
            if model == "invalid" {
                return Err("selection failed".into());
            }
            Ok(Arc::new(Engine(model.into())))
        });
        let original = Config::default();
        let shared = SharedSpeech(Arc::new(Mutex::new(factory(&original).unwrap())));
        let previous = Mutex::new(original.clone());
        let meeting =
            lf_meetings_backend::CaptureFactory::transcriber(&Capture(shared.clone()), &original)
                .unwrap();
        let mut fresh = original.clone();
        fresh.set_field("model_size", serde_json::json!("new-model"));
        assert!(!refresh_server_speech(&previous, &shared, fresh.clone(), &factory).unwrap());
        assert_eq!(
            meeting.transcribe(&[], None).unwrap(),
            original.field_str("model_size").unwrap()
        );
        drop(meeting);
        assert!(refresh_server_speech(&previous, &shared, fresh.clone(), &factory).unwrap());
        assert_eq!(shared.transcribe(&[], None).unwrap(), "new-model");
        assert!(!shared.loaded());
        let same = shared.current();
        fresh.set_field("sounds_enabled", serde_json::json!(false));
        assert!(refresh_server_speech(&previous, &shared, fresh.clone(), &factory).unwrap());
        assert!(Arc::ptr_eq(&same, &shared.current()));
        drop(same);
        fresh.set_field("model_size", serde_json::json!("invalid"));
        assert!(refresh_server_speech(&previous, &shared, fresh, &factory).is_err());
        assert_eq!(shared.transcribe(&[], None).unwrap(), "new-model");
        assert_eq!(
            previous.lock().unwrap().field_str("model_size"),
            Some("new-model")
        );
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
        let shared = SharedSpeech(Arc::new(Mutex::new(old.clone())));
        assert!(!shared.loaded());
        assert_eq!(shared.transcribe(&[], None).unwrap(), "old");
        let replacement = PublishSpeech {
            inner: Arc::new(Engine("new", false, AtomicBool::new(false))),
            destination: shared.clone(),
        };
        replacement.load().unwrap();
        old.unload();
        assert!(shared.loaded());
        assert_eq!(shared.transcribe(&[0.1], None).unwrap(), "new");
        let failed = PublishSpeech {
            inner: Arc::new(Engine("bad", true, AtomicBool::new(false))),
            destination: shared.clone(),
        };
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
