use std::future::Future;
use std::sync::Arc;

use anyhow::Result;
use lf_core::config::{load_config, Config};
use lf_meetings::{system_audio, AudioSource, Transcriber};
use lf_meetings_backend::{CaptureFactory, MeetingsBackend};
use lf_server::{backend::UnavailableBackend, Server, ServerState};
use lf_stt::server::{SharedSpeech, SttBackend};

use crate::{outln, Io};

struct MeetingSpeech(SharedSpeech);

impl Transcriber for MeetingSpeech {
    fn transcribe(&self, audio: &[f32], prompt: Option<&str>) -> Result<String> {
        Ok(self.0.transcribe_current(audio, prompt)?.text)
    }
}

/// Meetings share the `/transcribe` model, so neither caller loads a second copy.
struct MeetingCapture(SharedSpeech);

impl CaptureFactory for MeetingCapture {
    fn transcriber(&self, config: &Config) -> Result<Arc<dyn Transcriber>> {
        // Meeting capture resamples to 16 kHz itself; the dictation
        // `sample_rate` setting only governs the dictation recorder.
        self.0.load(config)?;
        Ok(Arc::new(MeetingSpeech(self.0.clone())))
    }

    fn microphone(&self, config: &Config) -> Result<Box<dyn AudioSource>> {
        Ok(Box::new(system_audio::CpalSource::microphone_named(
            config.field_str("input_device"),
        )?))
    }

    fn system_audio(&self, config: &Config) -> Result<Option<Box<dyn AudioSource>>> {
        Ok(Some(system_audio::open_system_audio(
            system_audio::SAMPLE_RATE,
            config.field_str("system_audio_device").unwrap_or_default(),
        )?))
    }
}

pub(crate) fn run(host: Option<String>, port: Option<u16>, io: &mut Io<'_>) -> Result<()> {
    let config = load_config()?;
    let host = host
        .as_deref()
        .unwrap_or_else(|| config.field_str("server_host").unwrap_or("127.0.0.1"));
    let port = match port {
        Some(port) => port,
        None => u16::try_from(config.field_i64("server_port").unwrap_or(8756))
            .map_err(|_| anyhow::anyhow!("server_port must be between 0 and 65535"))?,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(16)
        .enable_all()
        .build()?;
    let speech = SharedSpeech::new();
    let meetings = Arc::new(
        MeetingsBackend::new()?
            .with_capture(Arc::new(MeetingCapture(speech.clone())))
            .with_inner(Arc::new(SttBackend::with_speech(
                UnavailableBackend,
                speech,
            ))),
    );
    let state = Arc::new(ServerState::load_with_backend(meetings.clone())?);
    let server = runtime.block_on(Server::bind_with_state(host, port, state.clone()))?;
    outln!(
        io.out,
        "LocalFlow Rust server listening on {}",
        lf_server::server_url::base_url(host, server.local_addr()?.port())
    )?;
    io.out.flush()?;
    let served = runtime.block_on(server.run());
    let active = meetings.session_active();
    let shutdown = finish_shutdown(
        &runtime,
        io,
        active,
        move || state.shutdown(),
        move || meetings.wait_for_saved_transcript(),
        interrupted(),
    );
    // A skipped save leaves its LLM steps running; do not wait for them.
    runtime.shutdown_background();
    served.and(shutdown)
}

/// Saves an active meeting before exit, announcing it. A second interrupt
/// skips the remaining LLM steps once the raw transcript is on disk.
fn finish_shutdown(
    runtime: &tokio::runtime::Runtime,
    io: &mut Io<'_>,
    meeting_active: bool,
    shutdown: impl FnOnce() -> Result<()> + Send + 'static,
    wait_for_saved_transcript: impl FnOnce() -> bool + Send + 'static,
    interrupt: impl Future<Output = ()>,
) -> Result<()> {
    if !meeting_active {
        return shutdown();
    }
    outln!(io.out, "Saving meeting…")?;
    io.out.flush()?;
    let saving = runtime.spawn_blocking(shutdown);
    let saved = runtime.block_on(async {
        tokio::select! {
            result = saving => Some(result),
            () = interrupt => None,
        }
    });
    if let Some(result) = saved {
        return result?;
    }
    if runtime.block_on(runtime.spawn_blocking(wait_for_saved_transcript))? {
        outln!(io.out, "Meeting transcript saved without AI notes.")?;
        io.out.flush()?;
        Ok(())
    } else {
        anyhow::bail!("could not save meeting during shutdown")
    }
}

async fn interrupted() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_server::backend::Backend;
    use lf_stt::{EngineSpec, LoadInfo, SpeechEngine, SttSlot, Transcript};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    struct Fake {
        loads: Arc<AtomicUsize>,
        loaded: bool,
    }

    impl SpeechEngine for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn load(&mut self) -> Result<LoadInfo> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.loaded = true;
            Ok(LoadInfo::default())
        }
        fn transcribe(&mut self, _: &[f32], _: Option<&str>) -> Result<Transcript> {
            Ok(Transcript {
                text: "meeting words".into(),
                ..Transcript::default()
            })
        }
        fn unload(&mut self) {
            self.loaded = false;
        }
        fn loaded(&self) -> bool {
            self.loaded
        }
    }

    #[test]
    fn meetings_reuse_the_model_loaded_for_transcribe_and_report_it_loaded() {
        let root = tempfile::tempdir().unwrap();
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = loads.clone();
        let speech = SharedSpeech::with_factory(SttSlot::with_lease_root(root.path().into()), {
            move |config| {
                let model = config.field_str("model_size").unwrap_or("base").to_string();
                Ok(EngineSpec {
                    key: model.clone(),
                    backend: "whisper-cpp".into(),
                    model_size: model,
                    engine: Box::new(Fake {
                        loads: counter.clone(),
                        loaded: false,
                    }),
                })
            }
        });
        let transcribe = SttBackend::with_speech(UnavailableBackend, speech.clone());
        let capture = MeetingCapture(speech);
        assert!(!transcribe.transcriber_loaded());
        let first = capture.transcriber(&Config::default()).unwrap();
        assert!(transcribe.transcriber_loaded());
        let second = capture.transcriber(&Config::default()).unwrap();
        assert_eq!(first.transcribe(&[0.25], None).unwrap(), "meeting words");
        assert_eq!(second.transcribe(&[0.25], None).unwrap(), "meeting words");
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    fn shutdown_output(
        meeting_active: bool,
        shutdown: impl FnOnce() -> Result<()> + Send + 'static,
        wait_for_saved_transcript: impl FnOnce() -> bool + Send + 'static,
        interrupt: impl Future<Output = ()>,
    ) -> (Result<()>, String) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input = std::io::Cursor::new(Vec::new());
        let mut clipboard = crate::testing::FakeClipboard::default();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
            clipboard: &mut clipboard,
            server_url: None,
            terminal: false,
        };
        let result = finish_shutdown(
            &runtime,
            &mut io,
            meeting_active,
            shutdown,
            wait_for_saved_transcript,
            interrupt,
        );
        runtime.shutdown_background();
        assert!(err.is_empty());
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn shutdown_is_silent_without_a_meeting_and_announces_a_meeting_save() {
        let (result, out) =
            shutdown_output(false, || Ok(()), || unreachable!(), std::future::pending());
        assert!(result.is_ok());
        assert_eq!(out, "");
        let (result, out) = shutdown_output(
            true,
            || anyhow::bail!("could not save meeting during shutdown"),
            || unreachable!(),
            std::future::pending(),
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "could not save meeting during shutdown"
        );
        assert_eq!(out, "Saving meeting…\n");
    }

    #[test]
    fn second_interrupt_returns_once_the_raw_transcript_is_saved() {
        let (llm_running, llm_never_finishes) = mpsc::channel::<()>();
        let (saved_tx, saved_rx) = mpsc::channel();
        let (result, out) = shutdown_output(
            true,
            move || {
                let _ = llm_never_finishes.recv();
                Ok(())
            },
            move || {
                saved_tx.send(()).unwrap();
                true
            },
            std::future::ready(()),
        );
        assert!(result.is_ok());
        saved_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            out,
            "Saving meeting…\nMeeting transcript saved without AI notes.\n"
        );
        drop(llm_running);
    }

    #[test]
    fn meeting_transcriber_loads_up_front_ignoring_the_dictation_sample_rate() {
        // An unknown provider fails before any model download; reaching that
        // error from the factory proves the model is loaded before capture
        // and that the 22,050 Hz dictation setting was not checked.
        let mut config = Config::default();
        config.set_field("sample_rate", json!(22050));
        config.set_field("stt_backend", json!("test-unknown"));
        let error = MeetingCapture(SharedSpeech::new())
            .transcriber(&config)
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "unsupported speech provider: test-unknown"
        );
    }
}
