use std::future::Future;
use std::sync::Arc;

use anyhow::Result;
use lf_core::config::{load_config, Config};
use lf_core::perf_ledger::{self, Kind, PerfEvent, PerfLedger, Status};
use lf_meetings::{system_audio, AudioSource, TimedTranscript, Transcriber};
use lf_meetings_backend::{CaptureFactory, MeetingSave, MeetingsBackend};
use lf_server::{backend::UnavailableBackend, Server, ServerState};
use lf_stt::server::{SharedSpeech, SttBackend};

use crate::{outln, Io};

/// Transcribes with the config the meeting started with, so another caller's
/// model switch (even a failed one) cannot change or break the meeting's model.
struct MeetingSpeech(SharedSpeech, Config);

impl Transcriber for MeetingSpeech {
    fn transcribe(&self, audio: &[f32], prompt: Option<&str>) -> Result<String> {
        Ok(self.0.transcribe(&self.1, audio, prompt)?.text)
    }

    /// The same single inference; the engine's segment timings ride along.
    fn transcribe_chunk(&self, audio: &[f32], prompt: Option<&str>) -> Result<TimedTranscript> {
        let transcript = self.0.transcribe(&self.1, audio, prompt)?;
        Ok(TimedTranscript {
            text: transcript.text,
            segments: transcript.segments,
        })
    }
}

/// Meetings share the `/transcribe` model, so neither caller loads a second copy.
struct MeetingCapture(SharedSpeech);

impl CaptureFactory for MeetingCapture {
    fn transcriber(&self, config: &Config) -> Result<Arc<dyn Transcriber>> {
        // Meeting capture resamples to 16 kHz itself; the dictation
        // `sample_rate` setting only governs the dictation recorder.
        self.0.load(config)?;
        Ok(Arc::new(MeetingSpeech(self.0.clone(), config.clone())))
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

    /// Built with `--features diarize`: the verified B6 models from the local
    /// Hugging Face cache, loaded on the collector's worker (never downloaded).
    #[cfg(feature = "diarize")]
    fn diarizer(
        &self,
        config: &Config,
    ) -> Result<Option<Box<dyn lf_meetings::diarize::SpeakerBackend>>> {
        if config.field_str("diarization_backend") != Some("sherpa-onnx") {
            return Ok(None);
        }
        Ok(Some(Box::new(lf_diarize::sherpa::SherpaBackend::new(
            lf_stt::model_storage::hf_hub_cache(None),
        ))))
    }
}

pub(crate) fn run(host: Option<String>, port: Option<u16>, io: &mut Io<'_>) -> Result<()> {
    let started = std::time::Instant::now();
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
    let ledger = PerfLedger::default();
    // Staged enrollment audio a killed server left behind, whatever the speaker
    // flags are now; creates nothing and never fails.
    lf_meetings::speakers::registry::sweep(&lf_meetings::speakers::default_speakers_root());
    let speech = SharedSpeech::new();
    let meetings = Arc::new(
        MeetingsBackend::new()?
            .with_model_cache(lf_stt::model_storage::hf_hub_cache(None))
            // The saved file is what `PUT /api/config` writes: the live speaker gate.
            .with_config_reader(Arc::new(load_config))
            .with_capture(Arc::new(MeetingCapture(speech.clone())))
            .with_perf_ledger(ledger.clone())
            .with_inner(Arc::new(SttBackend::with_speech(
                UnavailableBackend,
                speech,
            ))),
    );
    // Speaker routes follow the saved file (other routes keep the loaded config).
    let state = Arc::new(
        ServerState::load_with_backend(meetings.clone())?
            .with_speaker_config_reader(Arc::new(load_config)),
    );
    let server = runtime.block_on(Server::bind_with_state(host, port, state.clone()))?;
    // Ready to accept requests: config, backends and the bound socket.
    let elapsed = perf_ledger::ms(started.elapsed());
    ledger.record_if(
        &config,
        &PerfEvent {
            total_ms: Some(elapsed),
            ..PerfEvent::new(Kind::Startup, Status::Ok)
        },
    );
    outln!(
        io.out,
        "LocalFlow Rust server listening on {}",
        lf_server::server_url::base_url(host, server.local_addr()?.port())
    )?;
    io.out.flush()?;
    // One listener for the whole run, so a second signal is seen even while
    // the server still drains an in-flight `/meeting/stop`.
    let mut signals = runtime.block_on(async { Signals::new() })?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut serving = runtime.spawn(server.run_until(async {
        let _ = stopped.await;
    }));
    let early = runtime.block_on(async {
        tokio::select! {
            served = &mut serving => Some(served),
            () = signals.recv() => None,
        }
    });
    let active = early.is_none() && meetings.session_active();
    let _ = stop.send(());
    let handle = runtime.handle().clone();
    let shutdown = finish_shutdown(
        &runtime,
        io,
        active,
        move || {
            let served = early.unwrap_or_else(|| handle.block_on(serving));
            let shutdown = state.shutdown();
            served?.and(shutdown)
        },
        move || meetings.wait_for_saved_transcript(),
        async move { signals.recv().await },
    );
    // A skipped save leaves its LLM steps running; do not wait for them.
    runtime.shutdown_background();
    shutdown
}

/// Saves an active meeting before exit, announcing it. A second interrupt
/// skips the remaining LLM steps once the raw transcript is on disk.
fn finish_shutdown(
    runtime: &tokio::runtime::Runtime,
    io: &mut Io<'_>,
    meeting_active: bool,
    shutdown: impl FnOnce() -> Result<()> + Send + 'static,
    wait_for_saved_transcript: impl FnOnce() -> MeetingSave + Send + 'static,
    interrupt: impl Future<Output = ()>,
) -> Result<()> {
    if !meeting_active {
        return shutdown();
    }
    // Announcements are best-effort: a closed stdout must not stop the save.
    let _ = writeln!(io.out, "Saving meeting…").and_then(|()| io.out.flush());
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
    match runtime.block_on(runtime.spawn_blocking(wait_for_saved_transcript))? {
        MeetingSave::NoMeeting | MeetingSave::Note => Ok(()),
        MeetingSave::Transcript => {
            let _ = writeln!(io.out, "Meeting transcript saved without AI notes.")
                .and_then(|()| io.out.flush());
            Ok(())
        }
        MeetingSave::Unsaved => anyhow::bail!("could not save meeting during shutdown"),
    }
}

/// Ctrl-C, plus SIGTERM on Unix, as in `lf_server::Server::run`.
struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
}

impl Signals {
    fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                ctrl_c: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => {},
            _ = self.terminate.recv() => {},
        }
        #[cfg(windows)]
        self.ctrl_c.recv().await;
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
        model: String,
    }

    impl SpeechEngine for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn load(&mut self) -> Result<LoadInfo> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(self.model != "broken", "broken model");
            self.loaded = true;
            Ok(LoadInfo::default())
        }
        fn transcribe(&mut self, _: &[f32], _: Option<&str>) -> Result<Transcript> {
            Ok(Transcript {
                text: format!("{} words", self.model),
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
        let (speech, loads) = fake_speech(root.path());
        let transcribe = SttBackend::with_speech(UnavailableBackend, speech.clone());
        let capture = MeetingCapture(speech);
        assert!(!transcribe.transcriber_loaded());
        let first = capture.transcriber(&model("tiny")).unwrap();
        assert!(transcribe.transcriber_loaded());
        let second = capture.transcriber(&model("tiny")).unwrap();
        assert_eq!(first.transcribe(&[0.25], None).unwrap(), "tiny words");
        assert_eq!(second.transcribe(&[0.25], None).unwrap(), "tiny words");
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_model_switch_by_an_upload_does_not_break_the_running_meeting() {
        let root = tempfile::tempdir().unwrap();
        let (speech, loads) = fake_speech(root.path());
        let meeting = MeetingCapture(speech.clone())
            .transcriber(&model("tiny"))
            .unwrap();
        let upload = speech.transcribe(&model("broken"), &[0.25], None);
        assert_eq!(upload.unwrap_err().to_string(), "broken model");
        assert_eq!(meeting.transcribe(&[0.25], None).unwrap(), "tiny words");
        assert_eq!(
            speech
                .transcribe(&model("small"), &[0.25], None)
                .unwrap()
                .text,
            "small words"
        );
        assert_eq!(meeting.transcribe(&[0.25], None).unwrap(), "tiny words");
        assert_eq!(loads.load(Ordering::SeqCst), 5);
    }

    fn model(name: &str) -> Config {
        let mut config = Config::default();
        config.set_field("model_size", json!(name));
        config
    }

    fn fake_speech(root: &std::path::Path) -> (SharedSpeech, Arc<AtomicUsize>) {
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = loads.clone();
        let speech = SharedSpeech::with_factory(SttSlot::with_lease_root(root.into()), {
            move |config| {
                let model = config.field_str("model_size").unwrap_or("base").to_string();
                Ok(EngineSpec {
                    key: model.clone(),
                    backend: "whisper-cpp".into(),
                    model_size: model.clone(),
                    engine: Box::new(Fake {
                        loads: counter.clone(),
                        loaded: false,
                        model,
                    }),
                })
            }
        });
        (speech, loads)
    }

    fn shutdown_output(
        meeting_active: bool,
        shutdown: impl FnOnce() -> Result<()> + Send + 'static,
        wait_for_saved_transcript: impl FnOnce() -> MeetingSave + Send + 'static,
        interrupt: impl Future<Output = ()>,
    ) -> (Result<()>, String) {
        let mut out = Vec::new();
        let result = shutdown_writing_to(
            &mut out,
            meeting_active,
            shutdown,
            wait_for_saved_transcript,
            interrupt,
        );
        (result, String::from_utf8(out).unwrap())
    }

    fn shutdown_writing_to(
        out: &mut dyn std::io::Write,
        meeting_active: bool,
        shutdown: impl FnOnce() -> Result<()> + Send + 'static,
        wait_for_saved_transcript: impl FnOnce() -> MeetingSave + Send + 'static,
        interrupt: impl Future<Output = ()>,
    ) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let mut err = Vec::new();
        let mut input = std::io::Cursor::new(Vec::new());
        let mut clipboard = crate::testing::FakeClipboard::default();
        let mut io = Io {
            out,
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
        result
    }

    struct ClosedPipe;

    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    #[test]
    fn a_closed_stdout_neither_skips_nor_fails_the_meeting_save() {
        let (saved, saves) = mpsc::channel();
        let result = shutdown_writing_to(
            &mut ClosedPipe,
            true,
            move || {
                saved.send(()).unwrap();
                Ok(())
            },
            || unreachable!(),
            std::future::pending(),
        );
        assert!(result.is_ok());
        saves.try_recv().unwrap();
        let (never_finishes, blocked) = mpsc::channel::<()>();
        let result = shutdown_writing_to(
            &mut ClosedPipe,
            true,
            move || {
                let _ = blocked.recv();
                Ok(())
            },
            || MeetingSave::Transcript,
            std::future::ready(()),
        );
        assert!(result.is_ok());
        drop(never_finishes);
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
    fn second_interrupt_reports_what_was_actually_saved() {
        for (saved, outcome, out_expected) in [
            (MeetingSave::Note, Ok(()), "Saving meeting…\n"),
            (MeetingSave::NoMeeting, Ok(()), "Saving meeting…\n"),
            (
                MeetingSave::Unsaved,
                Err("could not save meeting during shutdown"),
                "Saving meeting…\n",
            ),
        ] {
            let (llm_running, llm_never_finishes) = mpsc::channel::<()>();
            let (result, out) = shutdown_output(
                true,
                move || {
                    let _ = llm_never_finishes.recv();
                    Ok(())
                },
                move || saved,
                std::future::ready(()),
            );
            assert_eq!(
                result.map_err(|error| error.to_string()),
                outcome.map_err(String::from)
            );
            assert_eq!(out, out_expected);
            drop(llm_running);
        }
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
                MeetingSave::Transcript
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
