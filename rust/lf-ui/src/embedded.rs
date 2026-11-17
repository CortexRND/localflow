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

fn watch_server_speech(
    config: Config,
    shared: SharedSpeech,
    factory: lf_runtime::worker::SttFactory,
) -> Result<lf_runtime::live::WatcherGuard> {
    let previous = Mutex::new(config);
    let pending = Arc::new(Mutex::new(None::<Config>));
    let retry_pending = pending.clone();
    let retry: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
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
    });
    let apply_retry = retry.clone();
    let apply: Arc<dyn Fn(Config) + Send + Sync> = Arc::new(move |fresh| {
        *pending.lock().unwrap() = Some(fresh);
        apply_retry();
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
    pub runtime: Option<RuntimeHandle>,
    server: Option<ServerHandle>,
    pub speech: Arc<dyn SpeechEngine>,
    server_speech_watch: Option<lf_runtime::live::WatcherGuard>,
}

impl EmbeddedRuntime {
    pub fn start(config: Config, menu: MenuState, toggle: ToggleStore) -> Self {
        let error_menu = menu.clone();
        let report: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |error| {
            error_menu.change(|model| {
                model.input_warning = Some(format!("Speech model unavailable: {error}"))
            })
        });
        let make_speech: lf_runtime::worker::SttFactory = Arc::new(move |config| {
            Ok(Arc::new(lf_runtime::engine::ReportingSpeech::new(
                Arc::new(lf_stt::runtime::LazySpeechAdapter::new(config.clone())),
                report.clone(),
            )))
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
                audio: Box::new(lf_stt::runtime::RecorderSource::from_config(&config)?),
                speech: initial,
                build_stt,
                play_sound: Arc::new(|_| {}),
            };
            lf_runtime::run::start_with_menu(options, menu.clone(), Some(toggle))
        })()
        .unwrap_or_else(|e| {
            log::error!("dictation startup failed: {e}");
            menu.change(|m| m.input_warning = Some(format!("Dictation unavailable: {e}")));
            None
        });
        let server = (|| -> Result<Option<ServerHandle>> {
            let host = config.field_str("server_host").unwrap_or("127.0.0.1");
            let port = u16::try_from(config.field_i64("server_port").unwrap_or(8756))?;
            ensure_server(host, port, || {
                ensure_server_token(&lf_core::apitoken::try_token_path()?)?;
                let dictation = lf_runtime::server::DictationBackend::new(speech.clone(), &config);
                let backend = lf_meetings_backend::MeetingsBackend::new()?
                    .with_inner(Arc::new(dictation))
                    .with_capture(Arc::new(Capture(server_shared.clone())));
                Ok(Arc::new(
                    ServerState::load_with_backend(Arc::new(backend))?
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
        let server_speech_watch = if runtime.is_none() && server.is_some() {
            watch_server_speech(config, server_shared, make_speech)
                .map_err(|error| {
                    log::error!("could not watch server speech settings: {error}");
                })
                .ok()
        } else {
            None
        };
        Self {
            runtime,
            server,
            speech,
            server_speech_watch,
        }
    }

    pub fn shutdown(&mut self) {
        self.server_speech_watch.take();
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.shutdown();
        }
        if let Some(server) = self.server.as_mut() {
            server.shutdown();
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
}

impl ServerHandle {
    fn start(host: String, port: u16, state: Arc<ServerState>) -> Result<Self> {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let shutdown_state = state.clone();
        std::thread::Builder::new()
            .name("localflow-server".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .max_blocking_threads(16)
                        .enable_all()
                        .build()?;
                    runtime.block_on(async move {
                        let server = Server::bind_with_state(&host, port, state).await?;
                        eprintln!("in-process server: listening {}", server.local_addr()?);
                        server
                            .run_until(async move {
                                let _ = stopped.await;
                            })
                            .await
                    })
                })();
                if let Err(error) = result {
                    log::error!("local server stopped: {error}");
                    eprintln!("local server unavailable: {error}");
                }
            })?;
        Ok(Self {
            stop: Some(stop),
            state: shutdown_state,
        })
    }
    fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let state = self.state.clone();
            let pending = crate::shutdown::register_pending_write();
            if let Err(error) = std::thread::Builder::new()
                .name("localflow-meeting-save".into())
                .spawn(move || {
                    if !pending.wait_for_others(std::time::Duration::from_secs(4)) {
                        log::warn!("timed out waiting for meeting notes before shutdown");
                    }
                    if let Err(error) = state.shutdown() {
                        log::error!("meeting shutdown failed: {error}");
                    }
                    let _ = stop.send(());
                    drop(pending);
                })
            {
                log::error!("could not start meeting shutdown: {error}");
            }
        }
    }
}

struct Capture(SharedSpeech);

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
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let mut handle = ServerHandle {
        stop: Some(stop),
        state,
    };
    handle.shutdown();
    handle.shutdown();
    assert!(!saved.load(Ordering::SeqCst));
    drop(notes);
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
