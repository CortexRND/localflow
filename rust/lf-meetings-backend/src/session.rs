use anyhow::{anyhow, Context, Result};
use chrono::NaiveDateTime;
use lf_core::config::Config;
use lf_meetings::{
    diarize::{collector, Collector, Options, SpeakerBackend},
    AudioSource, MeetingSession, Segment, SpeakerLabels, Transcriber,
};
use std::{
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};

/// Factories execute on the session's owning thread, including for !Send devices.
pub trait CaptureFactory: Send + Sync + 'static {
    /// Returns a transcriber that is ready to transcribe (its model loaded).
    /// An error aborts the meeting start before any audio is captured.
    fn transcriber(&self, config: &Config) -> Result<Arc<dyn Transcriber>>;
    fn microphone(&self, config: &Config) -> Result<Box<dyn AudioSource>>;
    fn system_audio(&self, _config: &Config) -> Result<Option<Box<dyn AudioSource>>> {
        anyhow::bail!("system audio unavailable")
    }
    /// The speaker-label backend, asked only when `speaker_diarization` is on.
    /// It is loaded on the collector's worker, so this must not load models.
    /// `None` or an error leaves the meeting channel-labelled.
    fn diarizer(&self, _config: &Config) -> Result<Option<Box<dyn SpeakerBackend>>> {
        Ok(None)
    }
}

pub struct UnavailableCapture;
impl CaptureFactory for UnavailableCapture {
    fn transcriber(&self, _: &Config) -> Result<Arc<dyn Transcriber>> {
        anyhow::bail!("transcriber unavailable")
    }
    fn microphone(&self, _: &Config) -> Result<Box<dyn AudioSource>> {
        anyhow::bail!("microphone unavailable")
    }
}

pub(crate) struct Snapshot {
    pub started_at: NaiveDateTime,
    pub seconds: f64,
    pub segments: Vec<Segment>,
    pub system_audio: String,
    pub system_audio_error: String,
}
pub(crate) struct Finished {
    pub started_at: NaiveDateTime,
    pub seconds: f64,
    /// Channel-labelled, exactly as captured.
    pub segments: Vec<Segment>,
    /// Applied only after the recovery checkpoint holds `segments`.
    pub speaker_labels: Option<SpeakerLabels>,
}

fn speaker_collector(config: &Config, factory: &dyn CaptureFactory) -> Option<(Collector, bool)> {
    if config.field_bool("speaker_diarization") != Some(true) {
        return None;
    }
    let backend = match factory.diarizer(config) {
        Ok(backend) => backend?,
        Err(error) => {
            eprintln!("speaker labels unavailable: {error}");
            return None;
        }
    };
    let options = Options {
        max_speakers: collector::max_speakers(
            config.field_i64("diarization_max_speakers").unwrap_or(0),
        ),
        ..Options::default()
    };
    match Collector::start(backend, options) {
        Ok(collector) => Some((collector, config.field_bool("diarize_mic") == Some(true))),
        Err(error) => {
            eprintln!("speaker labels unavailable: {error}");
            None
        }
    }
}
enum Command {
    Snapshot(mpsc::Sender<Snapshot>),
    Stop(Option<mpsc::Sender<Finished>>),
}
pub(crate) struct SessionActor(mpsc::Sender<Command>);
impl SessionActor {
    pub fn open(
        config: Config,
        factory: Arc<dyn CaptureFactory>,
        lock: Arc<Mutex<()>>,
    ) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("localflow-meeting-session".into())
            .spawn(move || {
                let session = (|| -> Result<MeetingSession> {
                    let (system_audio, error) =
                        if config.field_bool("capture_system_audio") == Some(true) {
                            match factory.system_audio(&config) {
                                Ok(source) => (source, String::new()),
                                Err(error) => (None, error.to_string()),
                            }
                        } else {
                            (None, String::new())
                        };
                    let microphone = factory.microphone(&config)?;
                    let transcriber = factory.transcriber(&config)?;
                    let mut session = MeetingSession::new(
                        transcriber,
                        lock,
                        microphone,
                        system_audio,
                        Duration::from_secs(
                            config
                                .field_i64("meeting_chunk_seconds")
                                .unwrap_or(30)
                                .try_into()
                                .context("invalid chunk_seconds")?,
                        ),
                    )?;
                    if let Some((collector, diarize_mic)) = speaker_collector(&config, &*factory) {
                        session = session.with_diarization(collector, diarize_mic);
                    }
                    session.system_audio_error = error;
                    session.start()?;
                    Ok(session)
                })();
                let mut session = match session {
                    Ok(session) => {
                        if ready_tx.send(Ok(())).is_err() {
                            return;
                        }
                        session
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                while let Ok(command) = rx.recv() {
                    match command {
                        Command::Snapshot(reply) => {
                            let _ = reply.send(Snapshot {
                                started_at: session.started_at.expect("started session"),
                                seconds: session.elapsed_seconds(),
                                segments: session.segments(),
                                system_audio: session.system_audio_status().into(),
                                system_audio_error: session.system_audio_error.clone(),
                            });
                        }
                        Command::Stop(reply) => {
                            let segments = session.stop();
                            let speaker_labels = session.take_speaker_labels();
                            if let Some(reply) = reply {
                                let _ = reply.send(Finished {
                                    started_at: session.started_at.expect("started session"),
                                    seconds: session.elapsed_seconds(),
                                    segments,
                                    speaker_labels,
                                });
                            }
                            break;
                        }
                    }
                }
            })?;
        ready_rx.recv().context("session worker stopped")??;
        Ok(Self(tx))
    }
    pub fn snapshot(&self) -> Option<Snapshot> {
        let (tx, rx) = mpsc::channel();
        self.0.send(Command::Snapshot(tx)).ok()?;
        rx.recv().ok()
    }
    pub fn stop(&self) -> Result<Finished> {
        let (tx, rx) = mpsc::channel();
        self.0
            .send(Command::Stop(Some(tx)))
            .map_err(|_| anyhow!("session worker stopped"))?;
        rx.recv().context("session worker stopped")
    }
}
impl Drop for SessionActor {
    fn drop(&mut self) {
        let _ = self.0.send(Command::Stop(None));
    }
}
