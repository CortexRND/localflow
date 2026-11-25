use crate::{
    system_audio::{AudioCallback, AudioSource, SAMPLE_RATE},
    Segment, TimedText, SOURCE_ME, SOURCE_OTHERS,
};
use anyhow::Result;
use chrono::{Local, NaiveDateTime};
use std::{
    sync::{mpsc, Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub trait Transcriber: Send + Sync {
    fn transcribe(&self, audio: &[f32], prompt: Option<&str>) -> Result<String>;
    /// Segment timings for `audio`; implementations without timing return one span for the whole clip.
    fn transcribe_timed(&self, audio: &[f32], prompt: Option<&str>) -> Result<Vec<TimedText>> {
        let text = self.transcribe(audio, prompt)?;
        let text = crate::trim(&text);
        Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![TimedText {
                start: 0.0,
                end: audio.len() as f64 / SAMPLE_RATE as f64,
                text: text.into(),
            }]
        })
    }
}
#[derive(Default)]
struct SourceBuffer {
    audio: Vec<f32>,
    consumed_seconds: f64,
}
#[derive(Default)]
struct SessionState {
    mic: Mutex<SourceBuffer>,
    others: Mutex<SourceBuffer>,
    segments: Mutex<Vec<Segment>>,
    stop: (Mutex<bool>, Condvar),
}
impl SessionState {
    fn callback(self: &Arc<Self>, others: bool) -> AudioCallback {
        let state = self.clone();
        Arc::new(move |audio| {
            let buffer = if others { &state.others } else { &state.mic };
            buffer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .audio
                .extend(audio);
        })
    }
    fn drain(&self, transcriber: &dyn Transcriber, lock: &Mutex<()>) {
        for (source, buffer) in [(SOURCE_ME, &self.mic), (SOURCE_OTHERS, &self.others)] {
            let (audio, offset) = {
                let mut buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
                let offset = buffer.consumed_seconds;
                let audio = std::mem::take(&mut buffer.audio);
                buffer.consumed_seconds += audio.len() as f64 / SAMPLE_RATE as f64;
                (audio, offset)
            };
            if audio.len() < SAMPLE_RATE as usize || audio.iter().all(|v| v.abs() < 0.01) {
                continue;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                transcriber.transcribe(&audio, None)
            }));
            if let Ok(Ok(text)) = result {
                let text = crate::trim(&text);
                if !text.is_empty() {
                    let mut segments = self.segments.lock().unwrap_or_else(|e| e.into_inner());
                    let position = segments.partition_point(|s| s.offset_seconds <= offset);
                    segments.insert(
                        position,
                        Segment {
                            offset_seconds: offset,
                            text: text.into(),
                            source: source.into(),
                            end_seconds: None,
                            speaker_id: None,
                            speaker_label: None,
                        },
                    );
                }
            } else {
                log::warn!("chunk transcription failed ({source})");
            }
        }
    }
}
pub struct MeetingSession {
    pub started_at: Option<NaiveDateTime>,
    pub active: bool,
    pub system_audio_error: String,
    microphone: Box<dyn AudioSource>,
    system_audio: Option<Box<dyn AudioSource>>,
    transcriber: Arc<dyn Transcriber>,
    transcribe_lock: Arc<Mutex<()>>,
    chunk_seconds: Duration,
    state: Arc<SessionState>,
    worker: Option<(JoinHandle<()>, mpsc::Receiver<()>)>,
    start_monotonic: Option<Instant>,
}
impl MeetingSession {
    pub fn new(
        transcriber: Arc<dyn Transcriber>,
        transcribe_lock: Arc<Mutex<()>>,
        microphone: Box<dyn AudioSource>,
        system_audio: Option<Box<dyn AudioSource>>,
        chunk_seconds: Duration,
    ) -> Result<Self> {
        anyhow::ensure!(!chunk_seconds.is_zero(), "chunk_seconds must be positive");
        Ok(Self {
            started_at: None,
            active: false,
            system_audio_error: String::new(),
            microphone,
            system_audio,
            transcriber,
            transcribe_lock,
            chunk_seconds,
            state: Arc::default(),
            worker: None,
            start_monotonic: None,
        })
    }
    pub fn start(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        self.microphone.start(self.state.callback(false))?;
        if let Some(source) = self.system_audio.as_mut() {
            if let Err(error) = source.start(self.state.callback(true)) {
                self.system_audio_error = if error.0.is_empty() {
                    "SystemAudioUnavailable".into()
                } else {
                    error.to_string()
                };
                self.system_audio = None;
            }
        }
        self.started_at = Some(Local::now().naive_local());
        self.start_monotonic = Some(Instant::now());
        self.active = true;
        *self.state.stop.0.lock().unwrap_or_else(|e| e.into_inner()) = false;
        let state = self.state.clone();
        let transcriber = self.transcriber.clone();
        let lock = self.transcribe_lock.clone();
        let interval = self.chunk_seconds;
        let (tx, rx) = mpsc::channel();
        self.worker = Some((
            thread::spawn(move || {
                loop {
                    let stop = state.stop.0.lock().unwrap_or_else(|e| e.into_inner());
                    let (stop, _) = state
                        .stop
                        .1
                        .wait_timeout_while(stop, interval, |stop| !*stop)
                        .unwrap_or_else(|e| e.into_inner());
                    if *stop {
                        break;
                    }
                    drop(stop);
                    state.drain(&*transcriber, &lock);
                }
                let _ = tx.send(());
            }),
            rx,
        ));
        Ok(())
    }
    pub fn drain_once(&self) {
        self.state.drain(&*self.transcriber, &self.transcribe_lock);
    }
    pub fn segments(&self) -> Vec<Segment> {
        self.state
            .segments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn elapsed_seconds(&self) -> f64 {
        self.start_monotonic
            .map(|start| start.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }
    pub fn system_audio_status(&self) -> &'static str {
        if self.system_audio.is_some() {
            "on"
        } else if self.system_audio_error.is_empty() {
            "off"
        } else {
            "error"
        }
    }
    pub fn stop(&mut self) -> Vec<Segment> {
        *self.state.stop.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.state.stop.1.notify_all();
        if let Err(_error) = self.microphone.stop() {
            log::warn!("stopping microphone failed");
        }
        if let Some(source) = self.system_audio.as_mut() {
            if source.stop().is_err() {
                log::warn!("stopping system audio failed");
            }
        }
        self.active = false;
        if let Some((worker, done)) = self.worker.take() {
            if done.recv_timeout(Duration::from_secs(120)).is_ok() {
                let _ = worker.join();
            }
        }
        self.drain_once();
        self.segments()
    }
}
impl Drop for MeetingSession {
    fn drop(&mut self) {
        if self.active {
            self.stop();
        }
    }
}
