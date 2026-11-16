//! The dictation controller: a port of `localflow/pushtotalk.py`.
//!
//! Keyboard callbacks only enqueue events; one consumer thread owns the
//! microphone and the gesture state machine. The consumer blocks on its
//! channel with no timeout while idle (0% CPU) and only sets a deadline for
//! the double-tap window and the max-duration warning/stop.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};

/// Where finished recordings go (the worker queue).
pub type JobSink = Arc<dyn Fn(Job) + Send + Sync>;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lf_core::config::Config;

use crate::context::AppInfo;
use crate::engine::{AudioSource, Pcm, TextOutput};
use crate::settings::{
    bool_field, dictation_enabled, f64_field, i64_field, str_field, DICTATION_ENABLED,
};

pub const MIN_CLIP_SECONDS: f64 = 0.3;
pub const TAP_MAX: f64 = MIN_CLIP_SECONDS;
pub const DOUBLE_TAP_WINDOW: f64 = 0.4;
pub const TRIPLE_CANCEL_WINDOW: f64 = 0.5;

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Dictate,
    Command,
    Transform,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Dictate => "dictate",
            JobKind::Command => "command",
            JobKind::Transform => "transform",
        }
    }
}

/// `localflow/jobs.py` `Job`.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub kind: JobKind,
    pub audio: Option<Pcm>,
    pub app_info: Option<AppInfo>,
    pub selection: String,
    pub preset: Option<String>,
    pub clipboard_before: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Start(f64),
    Stop(f64),
    Down(f64),
    Up(f64),
    Cancel(f64),
    CommandDown(f64),
    CommandUp(f64),
    CommandCancel(f64),
    PasteLast(f64),
    DictationToggle(f64),
    Transform(String),
    Result {
        text: Option<String>,
        clipboard_before: Option<String>,
        error: bool,
        clipboard_expected: Option<String>,
    },
    Config(Arc<Config>),
    Shutdown,
}

impl Event {
    fn timestamp(&self) -> Option<f64> {
        match self {
            Event::Start(t)
            | Event::Stop(t)
            | Event::Down(t)
            | Event::Up(t)
            | Event::Cancel(t)
            | Event::CommandDown(t)
            | Event::CommandUp(t)
            | Event::CommandCancel(t)
            | Event::PasteLast(t)
            | Event::DictationToggle(t) => Some(*t),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    Idle,
    Held,
    Tapped,
    Locked,
    AwaitUp,
    Command,
}

pub type StateSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Everything the controller talks to.
pub struct ControllerDeps {
    pub audio: Box<dyn AudioSource>,
    pub jobs: JobSink,
    pub output: Arc<dyn TextOutput>,
    pub play_sound: Arc<dyn Fn(&str) + Send + Sync>,
    pub frontmost: Arc<dyn Fn() -> Option<AppInfo> + Send + Sync>,
    pub on_state: Option<StateSink>,
    /// `history_store.latest_text(fallback)` (or the fallback itself).
    pub latest_text: Arc<dyn Fn(&str) -> String + Send + Sync>,
    /// Reload, flip and save `dictation_enabled`; the new value.
    pub toggle_store: Arc<dyn Fn() -> Result<bool, String> + Send + Sync>,
    pub clock: Clock,
    pub min_clip_seconds: f64,
    pub initial_last_text: String,
}

struct Shared {
    toggle_intent: Mutex<bool>,
    queued: AtomicUsize,
    busy: AtomicBool,
    config: Mutex<Arc<Config>>,
}

/// The enqueue-only side, cloned into keyboard callbacks and the worker.
#[derive(Clone)]
pub struct ControllerHandle {
    tx: Sender<Event>,
    clock: Clock,
    shared: Arc<Shared>,
}

impl ControllerHandle {
    pub fn send(&self, event: Event) {
        self.shared.queued.fetch_add(1, Ordering::SeqCst);
        if self.tx.send(event).is_err() {
            self.shared.queued.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    pub fn note_start(&self) {
        self.send(Event::Start(self.now()));
    }
    pub fn note_stop(&self) {
        self.send(Event::Stop(self.now()));
    }
    pub fn note_down(&self) {
        self.send(Event::Down(self.now()));
    }
    pub fn note_up(&self) {
        self.send(Event::Up(self.now()));
    }
    pub fn note_cancel(&self) {
        self.send(Event::Cancel(self.now()));
    }
    pub fn note_command_down(&self) {
        self.send(Event::CommandDown(self.now()));
    }
    pub fn note_command_up(&self) {
        self.send(Event::CommandUp(self.now()));
    }
    pub fn note_command_cancel(&self) {
        self.send(Event::CommandCancel(self.now()));
    }
    pub fn note_paste_last(&self) {
        self.send(Event::PasteLast(self.now()));
    }
    pub fn note_dictation_toggle(&self) {
        self.send(Event::DictationToggle(self.now()));
    }
    pub fn note_transform(&self, preset: &str) {
        self.send(Event::Transform(preset.to_string()));
    }
    pub fn note_job_result(
        &self,
        text: Option<String>,
        clipboard_before: Option<String>,
        error: bool,
        clipboard_expected: Option<String>,
    ) {
        self.send(Event::Result {
            text,
            clipboard_before,
            error,
            clipboard_expected,
        });
    }

    /// Toggle-chord gesture: flips the intent so a rapid double press resolves
    /// to start+stop in order.
    pub fn note_toggle(&self) {
        let mut intent = self
            .shared
            .toggle_intent
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *intent = !*intent;
        if *intent {
            self.note_start();
        } else {
            self.note_stop();
        }
    }

    /// Swap in a reloaded config; applied between events on the consumer.
    pub fn update_config(&self, config: Arc<Config>) {
        self.send(Event::Config(config));
    }

    /// The consumer's current config (after the last applied update).
    pub fn config(&self) -> Arc<Config> {
        self.shared
            .config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// No recording, gesture or command in progress and nothing queued.
    pub fn is_idle(&self) -> bool {
        !self.shared.busy.load(Ordering::SeqCst) && self.shared.queued.load(Ordering::SeqCst) == 0
    }

    pub fn shutdown(&self) {
        self.send(Event::Shutdown);
    }
}

pub struct Controller {
    rx: Receiver<Event>,
    shared: Arc<Shared>,
    deps: ControllerDeps,
    config: Arc<Config>,
    app_state: String,
    recording_app_info: Option<AppInfo>,
    legacy_active: bool,
    gesture: Gesture,
    key_held: bool,
    down_at: Option<f64>,
    tapped_at: Option<f64>,
    locked_at: Option<f64>,
    recording_started_at: Option<f64>,
    max_warned: bool,
    command_active: bool,
    command_selection: String,
    command_clipboard_before: Option<String>,
    last_text: String,
    stopped: bool,
}

impl Controller {
    pub fn new(config: Arc<Config>, deps: ControllerDeps) -> (ControllerHandle, Controller) {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            toggle_intent: Mutex::new(false),
            queued: AtomicUsize::new(0),
            busy: AtomicBool::new(false),
            config: Mutex::new(config.clone()),
        });
        let handle = ControllerHandle {
            tx,
            clock: deps.clock.clone(),
            shared: shared.clone(),
        };
        let last_text = deps.initial_last_text.clone();
        let controller = Controller {
            rx,
            shared,
            deps,
            config,
            app_state: "idle".into(),
            recording_app_info: None,
            legacy_active: false,
            gesture: Gesture::Idle,
            key_held: false,
            down_at: None,
            tapped_at: None,
            locked_at: None,
            recording_started_at: None,
            max_warned: false,
            command_active: false,
            command_selection: String::new(),
            command_clipboard_before: None,
            last_text,
            stopped: false,
        };
        (handle, controller)
    }

    /// Run the consumer on its own thread.
    pub fn spawn(mut self) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("pushtotalk-consumer".into())
            .spawn(move || self.run())
            .expect("spawn controller thread")
    }

    pub fn gesture(&self) -> Gesture {
        self.gesture
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn is_recording(&self) -> bool {
        self.deps.audio.is_recording()
    }

    fn take(&self, event: Result<Event, ()>) -> Option<Event> {
        let event = event.ok()?;
        self.shared.queued.fetch_sub(1, Ordering::SeqCst);
        Some(event)
    }

    fn try_next(&self) -> Option<Event> {
        match self.rx.try_recv() {
            Ok(e) => self.take(Ok(e)),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Block until shutdown.
    pub fn run(&mut self) {
        let mut pending: Option<Event> = None;
        while !self.stopped {
            let event = match pending.take() {
                Some(e) => e,
                None => {
                    let next = match self.timer_timeout() {
                        None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                        Some(t) => self.rx.recv_timeout(t),
                    };
                    match next {
                        Ok(e) => match self.take(Ok(e)) {
                            Some(e) => e,
                            None => continue,
                        },
                        Err(RecvTimeoutError::Timeout) => {
                            self.check_timers((self.deps.clock)());
                            self.publish_busy();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => Event::Shutdown,
                    }
                }
            };
            pending = self.handle(event);
            self.publish_busy();
        }
    }

    /// Process every queued event, then check timers once (tests).
    pub fn drain(&mut self) {
        let mut pending = None;
        loop {
            let event = match pending.take().or_else(|| self.try_next()) {
                Some(e) => e,
                None => break,
            };
            pending = self.handle(event);
            if self.stopped {
                break;
            }
        }
        self.check_timers((self.deps.clock)());
        self.publish_busy();
    }

    fn publish_busy(&self) {
        let busy = self.gesture != Gesture::Idle
            || self.legacy_active
            || self.command_active
            || self.deps.audio.is_recording();
        self.shared.busy.store(busy, Ordering::SeqCst);
    }

    fn reset_toggle_intent(&self) {
        *self
            .shared
            .toggle_intent
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = false;
    }

    /// One event; returns a looked-ahead event to handle next, if any.
    pub fn handle(&mut self, event: Event) -> Option<Event> {
        let mut pending = None;
        match event {
            Event::Shutdown => {
                if self.deps.audio.is_recording() {
                    log::warn!("shutdown: discarded in-progress recording");
                    if self.command_active {
                        self.deps.output.restore_clipboard_if_matches(
                            &self.command_selection,
                            self.command_clipboard_before.as_deref(),
                        );
                    }
                    self.discard_recording();
                }
                self.stopped = true;
                return None;
            }
            Event::Result {
                text,
                clipboard_before,
                error,
                clipboard_expected,
            } => {
                self.handle_result(text, clipboard_before, error, clipboard_expected);
                return None;
            }
            Event::Transform(preset) => {
                self.handle_transform(&preset);
                return None;
            }
            Event::Config(config) => {
                self.apply_config(config);
                if !self.deps.audio.is_recording() {
                    self.reset_toggle_intent();
                }
                return None;
            }
            _ => {}
        }
        if let Event::CommandCancel(_) = event {
            self.cancel_command();
            return None;
        }
        let timestamp = event.timestamp().unwrap_or_else(|| (self.deps.clock)());
        self.check_timers(timestamp);
        if let Event::DictationToggle(_) = event {
            match (self.deps.toggle_store)() {
                Ok(enabled) => {
                    let mut config = (*self.config).clone();
                    config.set_field(DICTATION_ENABLED, serde_json::Value::Bool(enabled));
                    self.apply_config(Arc::new(config));
                }
                Err(error) => log::error!("could not toggle dictation: {error}"),
            }
            return None;
        }
        // Off: every gesture is ignored, so the mic is never opened.
        if !dictation_enabled(&self.config) && !matches!(event, Event::PasteLast(_)) {
            return None;
        }
        match event {
            Event::Start(_) => {
                if self.deps.audio.is_recording() {
                    return None;
                }
                match self.try_next() {
                    Some(Event::Shutdown) => pending = Some(Event::Shutdown),
                    Some(next) => {
                        if let Event::Stop(stop_at) = next {
                            if stop_at - timestamp < self.deps.min_clip_seconds {
                                log::info!(
                                    "spam press coalesced ({:.2}s), dropped",
                                    stop_at - timestamp
                                );
                                return None;
                            }
                        }
                        pending = Some(next);
                    }
                    None => {}
                }
                if self.do_start(None, "recording") {
                    self.legacy_active = true;
                }
            }
            Event::Stop(_) => {
                if self.deps.audio.is_recording() {
                    self.legacy_active = false;
                    self.do_stop(JobKind::Dictate, String::new(), None, false);
                }
            }
            Event::Down(_) => pending = self.handle_down(timestamp),
            Event::Up(_) => self.handle_up(timestamp),
            Event::Cancel(_) => self.handle_cancel(),
            Event::CommandDown(_) => self.start_command(),
            Event::CommandUp(_) => self.stop_command(),
            Event::PasteLast(_) => {
                let text = (self.deps.latest_text)(&self.last_text);
                if !text.is_empty() {
                    self.deps
                        .output
                        .paste_text(&text, &self.paste_method(), None);
                }
            }
            _ => {}
        }
        pending
    }

    fn paste_method(&self) -> String {
        let method = str_field(&self.config, "paste_method");
        if method.is_empty() {
            "auto".into()
        } else {
            method
        }
    }

    fn apply_config(&mut self, config: Arc<Config>) {
        self.config = config.clone();
        *self.shared.config.lock().unwrap_or_else(|p| p.into_inner()) = config;
        if !dictation_enabled(&self.config) {
            // Turning dictation off discards whatever is in progress.
            self.handle_cancel();
            self.gesture = Gesture::Idle;
            self.key_held = false;
            self.tapped_at = None;
            self.locked_at = None;
            self.reset_toggle_intent();
        }
    }

    fn publish_state(&mut self, state: &str) {
        if state == self.app_state {
            return;
        }
        self.app_state = state.to_string();
        if let Some(sink) = &self.deps.on_state {
            sink(state);
        }
    }

    fn play(&self, cue: &str) {
        if bool_field(&self.config, "sounds_enabled", true) {
            (self.deps.play_sound)(cue);
        }
    }

    fn max_recording_seconds(&self) -> f64 {
        f64_field(&self.config, "max_recording_seconds", 1200.0)
    }

    /// How long the consumer may block; None = until the next event.
    pub fn timer_timeout(&self) -> Option<Duration> {
        let now = (self.deps.clock)();
        let mut deadlines = Vec::new();
        if self.gesture == Gesture::Tapped {
            if let Some(t) = self.tapped_at {
                deadlines.push(t + DOUBLE_TAP_WINDOW);
            }
        }
        let maximum = self.max_recording_seconds();
        if let Some(started) = self.recording_started_at {
            if maximum > 0.0 {
                if !self.max_warned {
                    deadlines.push(started + (maximum - 60.0).max(0.0));
                }
                deadlines.push(started + maximum);
            }
        }
        let next = deadlines.into_iter().fold(f64::INFINITY, f64::min);
        if next.is_infinite() {
            return None;
        }
        Some(Duration::from_secs_f64((next - now).max(0.001)))
    }

    pub fn check_timers(&mut self, now: f64) {
        if self.gesture == Gesture::Tapped
            && self.tapped_at.is_some_and(|t| now - t >= DOUBLE_TAP_WINDOW)
        {
            self.discard_recording();
            self.gesture = Gesture::Idle;
            self.tapped_at = None;
            self.key_held = false;
            self.publish_state("idle");
        }
        let maximum = self.max_recording_seconds();
        let Some(started) = self.recording_started_at else {
            return;
        };
        if maximum <= 0.0 {
            return;
        }
        let elapsed = (now - started).max(0.0);
        if !self.max_warned && elapsed >= (maximum - 60.0).max(0.0) {
            self.max_warned = true;
            self.play("warn");
        }
        if elapsed >= maximum {
            log::info!("max duration reached ({maximum:.0}s); stopping");
            self.legacy_active = false;
            self.reset_toggle_intent();
            if self.command_active {
                self.stop_command();
            } else {
                self.do_stop(JobKind::Dictate, String::new(), None, false);
            }
            self.gesture = if self.key_held {
                Gesture::AwaitUp
            } else {
                Gesture::Idle
            };
            self.tapped_at = None;
            self.locked_at = None;
        }
    }

    fn handle_down(&mut self, timestamp: f64) -> Option<Event> {
        match self.gesture {
            Gesture::Idle => {
                let mut pending = None;
                match self.try_next() {
                    Some(Event::Shutdown) => return Some(Event::Shutdown),
                    Some(Event::Up(up_at)) if (0.0..=TAP_MAX).contains(&(up_at - timestamp)) => {
                        // A queued quick tap: never open the mic for it.
                        self.gesture = Gesture::Tapped;
                        self.tapped_at = Some(up_at);
                        self.key_held = false;
                        return None;
                    }
                    Some(next) => pending = Some(next),
                    None => {}
                }
                self.key_held = true;
                self.down_at = Some(timestamp);
                self.gesture = if self.do_start(None, "recording") {
                    Gesture::Held
                } else {
                    Gesture::AwaitUp
                };
                pending
            }
            Gesture::Tapped => {
                let tapped_at = self.tapped_at.unwrap_or(timestamp);
                if timestamp - tapped_at > DOUBLE_TAP_WINDOW {
                    self.discard_recording();
                    self.gesture = Gesture::Idle;
                    self.tapped_at = None;
                    self.publish_state("idle");
                    return self.handle_down(timestamp);
                }
                self.key_held = true;
                self.locked_at = Some(timestamp);
                if !self.deps.audio.is_recording() && !self.do_start(None, "recording") {
                    self.gesture = Gesture::AwaitUp;
                    return None;
                }
                self.gesture = Gesture::Locked;
                self.publish_state("locked");
                self.play("lock");
                None
            }
            Gesture::Locked => {
                let locked_at = self.locked_at.unwrap_or(timestamp);
                self.key_held = true;
                if timestamp - locked_at <= TRIPLE_CANCEL_WINDOW {
                    self.discard_recording();
                    self.play("cancel");
                    self.publish_state("idle");
                } else {
                    self.do_stop(JobKind::Dictate, String::new(), None, false);
                }
                self.gesture = Gesture::AwaitUp;
                self.locked_at = None;
                None
            }
            _ => None,
        }
    }

    fn handle_up(&mut self, timestamp: f64) {
        match self.gesture {
            Gesture::AwaitUp => {
                self.key_held = false;
                self.gesture = Gesture::Idle;
            }
            Gesture::Held => {
                self.key_held = false;
                let down_at = self.down_at.unwrap_or(timestamp);
                if timestamp - down_at >= TAP_MAX {
                    self.do_stop(JobKind::Dictate, String::new(), None, false);
                    self.gesture = Gesture::Idle;
                } else {
                    self.gesture = Gesture::Tapped;
                    self.tapped_at = Some(timestamp);
                }
                self.down_at = None;
            }
            Gesture::Locked => self.key_held = false,
            _ => {}
        }
    }

    fn handle_cancel(&mut self) {
        let active = matches!(
            self.gesture,
            Gesture::Held | Gesture::Tapped | Gesture::Locked | Gesture::Command
        );
        if !active && !(self.legacy_active && self.deps.audio.is_recording()) {
            return;
        }
        if self.command_active {
            self.deps.output.restore_clipboard_if_matches(
                &self.command_selection,
                self.command_clipboard_before.as_deref(),
            );
        }
        self.discard_recording();
        self.legacy_active = false;
        self.play("cancel");
        self.gesture = if self.key_held {
            Gesture::AwaitUp
        } else {
            Gesture::Idle
        };
        self.tapped_at = None;
        self.locked_at = None;
        self.reset_toggle_intent();
        self.publish_state("idle");
        self.command_active = false;
        self.command_selection.clear();
        self.command_clipboard_before = None;
    }

    fn start_command(&mut self) {
        if self.gesture != Gesture::Idle || self.deps.audio.is_recording() {
            return;
        }
        let captured = self.deps.output.capture_selection();
        let app_info = (self.deps.frontmost)();
        self.command_selection = captured.text;
        self.command_clipboard_before = captured.clipboard_before;
        if self.do_start(Some(app_info), "command") {
            self.command_active = true;
            self.gesture = Gesture::Command;
        }
    }

    fn stop_command(&mut self) {
        if !self.command_active {
            return;
        }
        self.command_active = false;
        let selection = std::mem::take(&mut self.command_selection);
        let before = self.command_clipboard_before.take();
        self.do_stop(JobKind::Command, selection, before, true);
        self.gesture = Gesture::Idle;
    }

    fn cancel_command(&mut self) {
        if !self.command_active {
            return;
        }
        self.deps.output.restore_clipboard_if_matches(
            &self.command_selection,
            self.command_clipboard_before.as_deref(),
        );
        self.command_active = false;
        self.discard_recording();
        self.command_selection.clear();
        self.command_clipboard_before = None;
        self.gesture = Gesture::Idle;
    }

    fn handle_transform(&mut self, preset: &str) {
        let captured = self.deps.output.capture_selection();
        if captured.text.is_empty() {
            self.deps
                .output
                .restore_clipboard_if_matches(&captured.text, captured.clipboard_before.as_deref());
            log::error!("{preset} needs selected text");
            self.play("error");
            return;
        }
        let job = Job {
            kind: JobKind::Transform,
            audio: None,
            app_info: (self.deps.frontmost)(),
            selection: captured.text,
            preset: Some(preset.to_string()),
            clipboard_before: captured.clipboard_before,
        };
        (self.deps.jobs)(job);
        self.publish_state("processing");
    }

    fn handle_result(
        &mut self,
        text: Option<String>,
        clipboard_before: Option<String>,
        error: bool,
        clipboard_expected: Option<String>,
    ) {
        let Some(text) = text.filter(|t| !t.is_empty()) else {
            if let Some(expected) = clipboard_expected {
                self.deps
                    .output
                    .restore_clipboard_if_matches(&expected, clipboard_before.as_deref());
            }
            if error {
                log::error!("text transform returned no output");
                self.play("error");
            }
            self.publish_state("idle");
            return;
        };
        self.deps
            .output
            .paste_text(&text, &self.paste_method(), clipboard_before);
        self.last_text = text;
        self.publish_state("idle");
    }

    /// Open the mic. Never when dictation is off.
    fn do_start(&mut self, app_info: Option<Option<AppInfo>>, state: &str) -> bool {
        if !dictation_enabled(&self.config) {
            return false;
        }
        self.recording_app_info = match app_info {
            Some(info) => info,
            None => (self.deps.frontmost)(),
        };
        if let Err(error) = self.deps.audio.start() {
            self.recording_app_info = None;
            log::error!("mic error: {error}");
            self.publish_state("idle");
            return false;
        }
        self.recording_started_at = Some((self.deps.clock)());
        self.max_warned = false;
        self.play("start");
        self.publish_state(state);
        true
    }

    fn do_stop(
        &mut self,
        kind: JobKind,
        selection: String,
        clipboard_before: Option<String>,
        force: bool,
    ) {
        if !self.deps.audio.is_recording() {
            return;
        }
        let audio = self.deps.audio.stop();
        let app_info = self.recording_app_info.take();
        self.recording_started_at = None;
        self.max_warned = false;
        self.play("stop");
        let rate = i64_field(&self.config, "sample_rate", 16000);
        let duration = if rate > 0 {
            audio.len() as f64 / rate as f64
        } else {
            0.0
        };
        if duration < self.deps.min_clip_seconds && !force {
            self.publish_state("idle");
            return;
        }
        (self.deps.jobs)(Job {
            kind,
            audio: Some(audio),
            app_info,
            selection,
            preset: None,
            clipboard_before,
        });
        self.publish_state("processing");
    }

    fn discard_recording(&mut self) {
        if self.deps.audio.is_recording() {
            let _ = self.deps.audio.stop();
        }
        self.recording_app_info = None;
        self.recording_started_at = None;
        self.max_warned = false;
    }
}
