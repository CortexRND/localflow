//! Shared fakes (Python tests/test_pushtotalk.py FakeRecorder & co).
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use lf_core::config::Config;
use lf_input::paste::SelectionCapture;
use lf_runtime::controller::{Controller, ControllerDeps, ControllerHandle, Job};
use lf_runtime::engine::{AudioSource, Pcm, TextOutput};

#[derive(Default)]
pub struct AudioState {
    pub recording: bool,
    pub start_calls: usize,
    pub stop_calls: usize,
    pub fail_start: bool,
    /// Samples returned by stop().
    pub clip_len: usize,
}

#[derive(Clone, Default)]
pub struct FakeAudio(pub Arc<Mutex<AudioState>>);

impl AudioSource for FakeAudio {
    fn start(&mut self) -> Result<(), String> {
        let mut s = self.0.lock().unwrap();
        if s.fail_start {
            return Err("no mic".into());
        }
        s.start_calls += 1;
        s.recording = true;
        Ok(())
    }
    fn stop(&mut self) -> Pcm {
        let mut s = self.0.lock().unwrap();
        s.stop_calls += 1;
        s.recording = false;
        vec![0.1; s.clip_len]
    }
    fn is_recording(&self) -> bool {
        self.0.lock().unwrap().recording
    }
}

#[derive(Default)]
pub struct OutputLog {
    pub pasted: Vec<(String, String, Option<String>)>,
    pub restored: Vec<(String, Option<String>)>,
    pub selection: String,
    pub clipboard_before: Option<String>,
    pub captures: usize,
}

#[derive(Clone, Default)]
pub struct FakeOutput(pub Arc<Mutex<OutputLog>>);

impl TextOutput for FakeOutput {
    fn paste_text(&self, text: &str, method: &str, clipboard_before: Option<String>) {
        self.0
            .lock()
            .unwrap()
            .pasted
            .push((text.into(), method.into(), clipboard_before));
    }
    fn capture_selection(&self) -> SelectionCapture {
        let mut log = self.0.lock().unwrap();
        log.captures += 1;
        SelectionCapture {
            text: log.selection.clone(),
            clipboard_before: log.clipboard_before.clone(),
        }
    }
    fn restore_clipboard_if_matches(&self, expected: &str, clipboard_before: Option<&str>) {
        self.0
            .lock()
            .unwrap()
            .restored
            .push((expected.into(), clipboard_before.map(str::to_string)));
    }
}

pub struct Rig {
    pub handle: ControllerHandle,
    pub controller: Controller,
    pub audio: FakeAudio,
    pub output: FakeOutput,
    pub jobs: Arc<Mutex<Vec<Job>>>,
    pub sounds: Arc<Mutex<Vec<String>>>,
    pub states: Arc<Mutex<Vec<String>>>,
    pub now: Arc<Mutex<f64>>,
    pub toggles: Arc<Mutex<Vec<bool>>>,
    pub stored_enabled: Arc<Mutex<Result<bool, String>>>,
}

impl Rig {
    pub fn set_now(&self, t: f64) {
        *self.now.lock().unwrap() = t;
    }
    pub fn recording(&self) -> bool {
        self.audio.0.lock().unwrap().recording
    }
    pub fn start_calls(&self) -> usize {
        self.audio.0.lock().unwrap().start_calls
    }
    pub fn jobs(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }
}

pub fn config() -> Config {
    let mut c = Config::default();
    c.set_field("sounds_enabled", serde_json::json!(true));
    c.set_field("dictation_enabled", serde_json::json!(true));
    c
}

pub fn rig_with(config: Config) -> Rig {
    let audio = FakeAudio::default();
    audio.0.lock().unwrap().clip_len = 16000;
    let output = FakeOutput::default();
    let jobs: Arc<Mutex<Vec<Job>>> = Default::default();
    let sounds: Arc<Mutex<Vec<String>>> = Default::default();
    let states: Arc<Mutex<Vec<String>>> = Default::default();
    let now = Arc::new(Mutex::new(0.0));
    let toggles: Arc<Mutex<Vec<bool>>> = Default::default();
    // The "file": toggling flips this stored value (Err = save failure).
    let stored: Arc<Mutex<Result<bool, String>>> = Arc::new(Mutex::new(Ok(
        lf_runtime::settings::dictation_enabled(&config),
    )));
    let (j, s, st, n, t, sv) = (
        jobs.clone(),
        sounds.clone(),
        states.clone(),
        now.clone(),
        toggles.clone(),
        stored.clone(),
    );
    let deps = ControllerDeps {
        audio: Box::new(audio.clone()),
        jobs: Arc::new(move |job| j.lock().unwrap().push(job)),
        output: Arc::new(output.clone()),
        play_sound: Arc::new(move |cue| s.lock().unwrap().push(cue.into())),
        frontmost: Arc::new(|| None),
        on_state: Some(Arc::new(move |state| st.lock().unwrap().push(state.into()))),
        latest_text: Arc::new(|fallback| fallback.to_string()),
        toggle_store: Arc::new(move || {
            let mut stored = sv.lock().unwrap();
            let next = match &*stored {
                Ok(v) => !v,
                Err(e) => return Err(e.clone()),
            };
            *stored = Ok(next);
            t.lock().unwrap().push(next);
            Ok(next)
        }),
        clock: Arc::new(move || *n.lock().unwrap()),
        min_clip_seconds: lf_runtime::controller::MIN_CLIP_SECONDS,
        initial_last_text: "saved text".into(),
    };
    let (handle, controller) = Controller::new(Arc::new(config), deps);
    Rig {
        handle,
        controller,
        audio,
        output,
        jobs,
        sounds,
        states,
        now,
        toggles,
        stored_enabled: stored,
    }
}

pub fn rig() -> Rig {
    rig_with(config())
}
