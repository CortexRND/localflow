use anyhow::{anyhow, Result};
use lf_meetings::{
    system_audio::{find_loopback_device, resample, to_mono, InputDevice, SAMPLE_RATE},
    AudioCallback, AudioSource, MeetingSession, SystemAudioUnavailable, TimedText, Transcriber,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

#[derive(Clone, Default)]
struct Source {
    callback: Arc<Mutex<Option<AudioCallback>>>,
    stopped: Arc<AtomicBool>,
    fail_start: bool,
    fail_stop: bool,
}
impl Source {
    fn feed(&self, seconds: f64, volume: f32) {
        let callback = self.callback.lock().unwrap().clone().unwrap();
        callback(vec![volume; (seconds * SAMPLE_RATE as f64) as usize]);
    }
}
impl AudioSource for Source {
    fn start(&mut self, callback: AudioCallback) -> Result<(), SystemAudioUnavailable> {
        if self.fail_start {
            return Err(SystemAudioUnavailable("no permission".into()));
        }
        self.stopped.store(false, Ordering::SeqCst);
        *self.callback.lock().unwrap() = Some(callback);
        Ok(())
    }
    fn stop(&mut self) -> Result<(), SystemAudioUnavailable> {
        self.stopped.store(true, Ordering::SeqCst);
        *self.callback.lock().unwrap() = None;
        if self.fail_stop {
            return Err(SystemAudioUnavailable("stop failed".into()));
        }
        Ok(())
    }
    fn label(&self) -> &str {
        "fake"
    }
}
#[derive(Default)]
struct FakeTranscriber {
    calls: AtomicUsize,
    replies: Mutex<VecDeque<Result<String>>>,
    shared_lock: Option<Arc<Mutex<()>>>,
    completed: Option<mpsc::Sender<()>>,
}
impl Transcriber for FakeTranscriber {
    fn transcribe(&self, audio: &[f32], prompt: Option<&str>) -> Result<String> {
        assert!(audio.len() >= SAMPLE_RATE as usize);
        assert!(prompt.is_none());
        if let Some(lock) = &self.shared_lock {
            assert!(lock.try_lock().is_err());
        }
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(done) = &self.completed {
            let _ = done.send(());
        }
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(format!("text {call}")))
    }
}
fn session(
    transcriber: Arc<FakeTranscriber>,
    mic: Source,
    others: Option<Source>,
) -> MeetingSession {
    let lock = transcriber.shared_lock.clone().unwrap_or_default();
    MeetingSession::new(
        transcriber,
        lock,
        Box::new(mic),
        others.map(|s| Box::new(s) as Box<dyn AudioSource>),
        Duration::from_secs(3600),
    )
    .unwrap()
}
#[test]
fn session_transcribes_mic_and_system_audio_with_sources() {
    let mic = Source::default();
    let system = Source::default();
    let mut session = session(Arc::default(), mic.clone(), Some(system.clone()));
    assert_eq!(session.elapsed_seconds(), 0.0);
    session.start().unwrap();
    assert!(session.active);
    assert!(session.started_at.is_some());
    assert_eq!(session.system_audio_status(), "on");
    mic.feed(2.0, 0.1);
    system.feed(2.0, 0.1);
    session.drain_once();
    mic.feed(2.0, 0.1);
    let segments = session.stop();
    assert_eq!(
        segments
            .iter()
            .map(|s| (s.offset_seconds, s.source.as_str(), s.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (0.0, "me", "text 1"),
            (0.0, "others", "text 2"),
            (2.0, "me", "text 3")
        ]
    );
    assert!(system.stopped.load(Ordering::SeqCst));
    assert!(mic.stopped.load(Ordering::SeqCst));
    assert!(!session.active);
    assert!(session.elapsed_seconds() > 0.0);
}
#[test]
fn session_falls_back_to_mic_when_system_audio_fails() {
    let mic = Source::default();
    let mut session = session(
        Arc::default(),
        mic.clone(),
        Some(Source {
            fail_start: true,
            ..Source::default()
        }),
    );
    session.start().unwrap();
    assert!(session.active);
    assert_eq!(session.system_audio_status(), "error");
    assert_eq!(session.system_audio_error, "no permission");
    mic.feed(2.0, 0.1);
    assert_eq!(session.stop()[0].source, "me");
}
#[test]
fn session_empty_silence_short_chunks_and_empty_text_advance_source_offsets() {
    let mic = Source::default();
    let transcriber = Arc::new(FakeTranscriber {
        replies: Mutex::new([Ok(" \n\u{1c}".into()), Ok("  hello  ".into())].into()),
        ..Default::default()
    });
    let mut session = session(transcriber.clone(), mic.clone(), None);
    assert_eq!(session.system_audio_status(), "off");
    session.start().unwrap();
    session.drain_once();
    mic.feed(0.5, 0.1);
    session.drain_once();
    mic.feed(2.0, 0.009);
    session.drain_once();
    mic.feed(1.0, -0.01);
    session.drain_once();
    mic.feed(1.0, 0.1);
    let segments = session.stop();
    assert_eq!(transcriber.calls.load(Ordering::SeqCst), 2);
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].offset_seconds, 3.5);
    assert_eq!(segments[0].text, "hello");
}
#[test]
fn session_chunk_failure_does_not_skip_other_source_or_next_chunk() {
    let mic = Source::default();
    let other = Source {
        fail_stop: true,
        ..Source::default()
    };
    let transcriber = Arc::new(FakeTranscriber {
        replies: Mutex::new([Err(anyhow!("ASR failed"))].into()),
        ..Default::default()
    });
    let mut session = session(transcriber, mic.clone(), Some(other.clone()));
    session.start().unwrap();
    mic.feed(1.0, 0.1);
    other.feed(1.0, 0.1);
    session.drain_once();
    mic.feed(1.0, 0.1);
    let segments = session.stop();
    assert_eq!(segments.len(), 2);
    assert_eq!(
        (&*segments[0].source, segments[0].offset_seconds),
        ("others", 0.0)
    );
    assert_eq!(
        (&*segments[1].source, segments[1].offset_seconds),
        ("me", 1.0)
    );
}
#[test]
fn session_worker_drains_on_interval_with_shared_transcribe_lock() {
    let mic = Source::default();
    let lock = Arc::new(Mutex::new(()));
    let (done, completed) = mpsc::channel();
    let transcriber = Arc::new(FakeTranscriber {
        shared_lock: Some(lock.clone()),
        completed: Some(done),
        ..Default::default()
    });
    let mut session = MeetingSession::new(
        transcriber,
        lock,
        Box::new(mic.clone()),
        None,
        Duration::from_millis(5),
    )
    .unwrap();
    session.start().unwrap();
    session.start().unwrap();
    mic.feed(1.0, 0.1);
    completed.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(session.stop().len(), 1);
    assert_eq!(session.stop().len(), 1);
}
#[test]
fn session_microphone_failure_and_invalid_interval() {
    let mut session = session(
        Arc::default(),
        Source {
            fail_start: true,
            ..Source::default()
        },
        None,
    );
    assert!(session.start().is_err());
    assert!(!session.active);
    assert!(session.stop().is_empty());
    assert!(MeetingSession::new(
        Arc::new(FakeTranscriber::default()),
        Arc::default(),
        Box::<Source>::default(),
        None,
        Duration::ZERO
    )
    .is_err());
}
#[test]
fn resample_and_to_mono_match_python() {
    let mono = to_mono(&[1., -1., 0.5, 0.5], 2);
    assert_eq!(mono, [0., 0.5]);
    assert_eq!(resample(&mono, 2, 4), [0., 0.25, 0.5, 0.5]);
    assert!(resample(&[], 2, 4).is_empty());
    assert_eq!(resample(&[0.1], 16_000, 16_000), [0.1]);
    assert_eq!(resample(&[1., 2., 3., 4., 5.], 2, 1), [1., 3.]);
    assert_eq!(resample(&[0.25], 48_000, 1), [0.25]);
    assert_eq!(to_mono(&[1., 2., 3.], 1), [1., 2., 3.]);
    assert!(to_mono(&[], 2).is_empty());
}
#[test]
fn find_loopback_device_prefers_configured_name_and_hint_order() {
    let devices = [
        ("speakers", 0),
        ("Stereo Mix", 2),
        ("BlackHole 2ch", 2),
        ("mic", 1),
    ]
    .map(|(name, channels)| InputDevice {
        name: name.into(),
        max_input_channels: channels,
        default_sample_rate: 48000,
    });
    assert_eq!(find_loopback_device(&devices, ""), Some(2));
    assert_eq!(find_loopback_device(&devices, "STEREO"), Some(1));
    assert_eq!(find_loopback_device(&devices, "mic"), Some(3));
    assert_eq!(find_loopback_device(&devices, "speakers"), None);
    assert_eq!(find_loopback_device(&[], ""), None);
    assert_eq!(find_loopback_device(&devices, "absent"), None);
}
#[cfg(not(feature = "capture"))]
#[test]
fn open_system_audio_explains_when_nothing_found() {
    let error = lf_meetings::system_audio::open_system_audio(16000, "missing")
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "no input device matching 'missing'");
    #[cfg(not(target_os = "macos"))]
    assert_eq!(lf_meetings::system_audio::open_system_audio(16000, "").err().unwrap().to_string(), "no system audio source found: set [meetings] system_audio_device to a loopback/monitor input device");
}
struct Fixed(Result<&'static str, &'static str>);
impl Transcriber for Fixed {
    fn transcribe(&self, _: &[f32], prompt: Option<&str>) -> Result<String> {
        assert_eq!(prompt, Some("hint"));
        self.0.map(Into::into).map_err(|e| anyhow!(e))
    }
}
struct Timed;
impl Transcriber for Timed {
    fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String> {
        unreachable!()
    }
    fn transcribe_timed(&self, _: &[f32], _: Option<&str>) -> Result<Vec<TimedText>> {
        Ok(vec![TimedText {
            start: 0.5,
            end: 1.0,
            text: "own".into(),
        }])
    }
}
#[test]
fn transcribe_timed_defaults_to_one_whole_clip_span() {
    let audio = vec![0.0; SAMPLE_RATE as usize * 5 / 2];
    assert_eq!(
        Fixed(Ok("  hello there \n"))
            .transcribe_timed(&audio, Some("hint"))
            .unwrap(),
        [TimedText {
            start: 0.0,
            end: 2.5,
            text: "hello there".into(),
        }]
    );
    for blank in ["", " \t\n", "\u{1c}"] {
        assert!(Fixed(Ok(blank))
            .transcribe_timed(&audio, Some("hint"))
            .unwrap()
            .is_empty());
    }
    let error = Fixed(Err("boom")).transcribe_timed(&audio, Some("hint"));
    assert_eq!(error.unwrap_err().to_string(), "boom");
    let transcriber: Arc<dyn Transcriber> = Arc::new(Timed);
    assert_eq!(
        transcriber.transcribe_timed(&audio, None).unwrap(),
        [TimedText {
            start: 0.5,
            end: 1.0,
            text: "own".into(),
        }]
    );
}
