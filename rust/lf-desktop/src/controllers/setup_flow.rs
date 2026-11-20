//! The native setup wizard's state machine (GPUI-free).
//!
//! Onboarding rules are not reimplemented: choices come from
//! `lf_ui::setup_flow::initial_choices`, checks from `run_checks`, models
//! from `available_stt_models` / `recommend`, and Apply is
//! `setup_flow::apply` against the explicit config path (it reloads the
//! latest file under `config_write::with_lock` and only writes the setup
//! sections, so unrelated and concurrent edits survive). Model work goes
//! through `SetupModelBackend` (`FlowModelBackend` for real, a simulation in
//! the sandbox).
//!
//! Nothing is written until Apply. The microphone opens only after Test
//! microphone and is released on leaving the step, closing, or Back; a
//! running trial is cancelled the same way.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_core::config::Config;
use lf_ui::setup_backend::SetupModelBackend;
use lf_ui::setup_flow::{
    self as flow, Check, HardwareInfo, LaunchAgent, SetupChoices, SetupPlatform, SttModel,
    HOTKEY_OPTIONS, LLM_MODELS,
};

/// How long Test microphone listens before stopping by itself.
pub const MIC_TEST: Duration = Duration::from_secs(10);
/// No sound for this long: "We can't hear anything from this microphone."
pub const SILENT_AFTER: Duration = Duration::from_secs(5);
/// Linear RMS level counted as "heard" (about -34 dBFS).
pub const HEARD_LEVEL: f64 = 0.02;

/// An open, metering-only microphone stream (no audio is kept).
pub trait Meter: Send {
    /// Latest linear RMS level, 0.0–1.0.
    fn level(&self) -> f64;
    /// Close the device. Idempotent.
    fn stop(&mut self);
}

/// Opens metering streams (`lf_audio` for real, a canned curve in sandbox).
pub trait MeterInput: Send + Sync {
    fn open(&self, device: Option<&str>) -> Result<Box<dyn Meter>, String>;
}

/// `lf_audio::Recorder::new_meter`: publishes the RMS of each callback and
/// retains no samples.
pub struct AudioMeterInput;

struct AudioMeter(lf_audio::Recorder);

impl Meter for AudioMeter {
    fn level(&self) -> f64 {
        let dbfs = self.0.level_dbfs();
        if dbfs.is_finite() {
            10f64.powf(dbfs / 20.0).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
    fn stop(&mut self) {
        let _ = self.0.stop();
    }
}

impl MeterInput for AudioMeterInput {
    fn open(&self, device: Option<&str>) -> Result<Box<dyn Meter>, String> {
        let mut recorder = lf_audio::Recorder::new_meter(device.map(str::to_string));
        recorder.start().map_err(|e| {
            format!("Couldn't open the microphone: {e}. Check that it's connected and that LocalFlow may use it.")
        })?;
        Ok(Box::new(AudioMeter(recorder)))
    }
}

/// Everything the wizard needs from the outside, chosen once per mode.
#[derive(Clone)]
pub struct SetupDeps {
    pub platform: Arc<dyn SetupPlatform + Send + Sync>,
    pub models: Arc<dyn SetupModelBackend>,
    pub meter: Arc<dyn MeterInput>,
    pub launch_agent: Arc<dyn LaunchAgent + Send + Sync>,
    /// Sandbox: results are simulated and flagged as such.
    pub simulated: bool,
    /// Speech inspection, download and the trial really work in this build
    /// (runtime feature, or the sandbox simulation).
    pub native_speech: bool,
    /// Why they don't, with the next step.
    pub speech_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Checkup,
    Microphone,
    Speech,
    Cleanup,
    Shortcut,
    Trial,
    Review,
}

impl Step {
    pub const ALL: [Step; 7] = [
        Step::Checkup,
        Step::Microphone,
        Step::Speech,
        Step::Cleanup,
        Step::Shortcut,
        Step::Trial,
        Step::Review,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Step::Checkup => "Checkup",
            Step::Microphone => "Microphone",
            Step::Speech => "Speech model",
            Step::Cleanup => "Cleanup",
            Step::Shortcut => "Shortcut",
            Step::Trial => "Try it",
            Step::Review => "Review",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Step::Checkup => "Quick checkup",
            Step::Microphone => "Choose your microphone",
            Step::Speech => "Pick your speech model",
            Step::Cleanup => "Tidy up what you say",
            Step::Shortcut => "Choose your shortcut",
            Step::Trial => "Give it a try",
            Step::Review => "Review and finish",
        }
    }

    pub fn intro(self) -> &'static str {
        match self {
            Step::Checkup => "LocalFlow looks at what this computer already has.",
            Step::Microphone => "Audio is only measured, never saved.",
            Step::Speech => "Speech is turned into text on this computer.",
            Step::Cleanup => "Optional: fix punctuation and filler words after you speak.",
            Step::Shortcut => "Hold a key while you talk, or press a shortcut to start and stop.",
            Step::Trial => {
                "Record a few words to check everything works. Nothing is saved or pasted."
            }
            Step::Review => "Nothing is saved until you apply.",
        }
    }

    pub fn index(self) -> usize {
        Step::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// The read-only facts setup shows (reloaded on resume / step entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fact {
    Installed = 0,
    Devices = 1,
    Ollama = 2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loadable<T> {
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Loadable<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Loadable::Ready(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicTest {
    Off,
    Opening,
    Testing { level: u8, heard: bool },
    Heard,
    Silent,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trial {
    Ready,
    Running,
    Success(String),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupMode {
    Local,
    KeepProvider,
    Off,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applying {
    Idle,
    Busy,
    Failed(String),
    Done(String),
}

/// A bounded trial for a background worker (`try_dictation_cancellable`).
pub struct TrialJob {
    pub generation: u64,
    pub cancel: Arc<AtomicBool>,
    pub model: SttModel,
    pub device: Option<String>,
    pub language: Option<String>,
}

/// One read-only line of the review step, with the step that changes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewRow {
    pub label: &'static str,
    pub value: String,
    pub step: Step,
}

pub struct SetupFlow {
    pub initial: SetupChoices,
    pub choices: SetupChoices,
    pub step: Step,
    visited: [bool; 7],
    pub checks: Loadable<Vec<Check>>,
    pub devices: Loadable<Vec<String>>,
    pub mic: MicTest,
    mic_generation: u64,
    mic_stream: Option<Box<dyn Meter>>,
    mic_started: Option<Instant>,
    mic_heard_at: Option<Instant>,
    pub models: Vec<SttModel>,
    pub recommended: String,
    pub recommended_llm: Option<String>,
    pub installed: Loadable<HashMap<String, bool>>,
    pub cleanup: CleanupMode,
    /// The provider in the config when setup opened (non-Ollama is kept).
    pub provider: String,
    /// Whether cleanup was on when setup opened (KeepProvider keeps it so).
    pub provider_cleanup_on: bool,
    pub ollama: Loadable<(bool, Vec<String>)>,
    pub trial: Trial,
    trial_generation: u64,
    trial_cancel: Option<Arc<AtomicBool>>,
    /// Generation of a trial worker that hasn't returned yet (also after
    /// it was cancelled: it may still hold the microphone).
    trial_worker: Option<u64>,
    pub apply: Applying,
    pub hotkey_error: Option<String>,
    pub simulated: bool,
    pub darwin: bool,
    /// The config this draft started from: Apply writes only what the user
    /// changed relative to it (see [`apply`]).
    pub opened: Config,
    /// The latest config the user saw in a conflict (they chose to apply on
    /// top of it). `opened` stays the immutable intent baseline; this is
    /// only the revision the next Apply expects to find on disk.
    pub acknowledged: Option<Config>,
    /// Generations of the read-only facts (installed models, devices,
    /// Ollama list): a result from a superseded worker is dropped.
    fact_generations: [u64; 3],
}

fn platform_name(darwin: bool) -> &'static str {
    if darwin {
        "darwin"
    } else if cfg!(windows) {
        "win32"
    } else {
        "linux"
    }
}

impl SetupFlow {
    /// Start from the saved config (after a first run) or the hardware
    /// recommendation (`initial_choices`).
    pub fn new(
        config: &Config,
        hw: &HardwareInfo,
        platform: &dyn SetupPlatform,
        launch_agent: &dyn LaunchAgent,
        simulated: bool,
    ) -> Result<Self, String> {
        let mut choices =
            flow::initial_choices(config, hw, platform, launch_agent).map_err(|e| e.0)?;
        // Start at login is managed by `lf agent install/uninstall`; this
        // window has no launch agent of its own and never changes it.
        choices.start_on_login = false;
        let models = flow::available_stt_models(hw, platform);
        let (recommended, recommended_llm) = flow::recommend(hw, platform);
        let provider = config
            .field_str("llm_provider")
            .unwrap_or("ollama")
            .to_string();
        let cleanup = if provider != "ollama" {
            if config.field_bool("cleanup_enabled").unwrap_or(true) {
                CleanupMode::KeepProvider
            } else {
                CleanupMode::Off
            }
        } else if choices.llm_id.is_some() {
            CleanupMode::Local
        } else {
            CleanupMode::Off
        };
        let mut visited = [false; 7];
        visited[0] = true;
        let mut flow = SetupFlow {
            initial: choices.clone(),
            choices,
            step: Step::Checkup,
            visited,
            checks: Loadable::Idle,
            devices: Loadable::Idle,
            mic: MicTest::Off,
            mic_generation: 0,
            mic_stream: None,
            mic_started: None,
            mic_heard_at: None,
            models,
            recommended,
            recommended_llm,
            installed: Loadable::Idle,
            cleanup,
            provider,
            provider_cleanup_on: config.field_bool("cleanup_enabled").unwrap_or(true),
            ollama: Loadable::Idle,
            trial: Trial::Ready,
            trial_generation: 0,
            trial_cancel: None,
            trial_worker: None,
            apply: Applying::Idle,
            hotkey_error: None,
            simulated,
            darwin: platform.is_darwin(),
            opened: config.clone(),
            acknowledged: None,
            fact_generations: [0; 3],
        };
        flow.validate_hotkey();
        // Keep the cleanup mode coherent with the choices from the start.
        flow.set_cleanup(cleanup);
        flow.initial = flow.choices.clone();
        Ok(flow)
    }

    /// Choices differ from what setup opened with.
    pub fn dirty(&self) -> bool {
        self.choices != self.initial
    }

    pub fn visited(&self, step: Step) -> bool {
        self.visited[step.index()]
    }

    /// Problems that block Continue on the current step.
    pub fn blocker(&self) -> Option<String> {
        match self.step {
            Step::Shortcut => self.hotkey_error.clone(),
            Step::Review => self.hotkey_error.clone(),
            _ => None,
        }
    }

    /// Release the microphone and cancel a trial (leaving a step, closing,
    /// losing focus). Returns whether anything was running.
    pub fn release_audio(&mut self) -> bool {
        let mut released = false;
        self.mic_generation += 1;
        if let Some(mut stream) = self.mic_stream.take() {
            stream.stop();
            released = true;
        }
        if matches!(self.mic, MicTest::Opening | MicTest::Testing { .. }) {
            self.mic = MicTest::Off;
            released = true;
        }
        self.mic_started = None;
        self.mic_heard_at = None;
        if let Some(cancel) = self.trial_cancel.take() {
            cancel.store(true, Ordering::Release);
            self.trial_generation += 1;
            self.trial = Trial::Ready;
            released = true;
        }
        released
    }

    /// Move to `step`: backward and to visited steps always; forward only one
    /// past the furthest visited step, and only when the current step has no
    /// blocker. Leaving a step releases audio.
    pub fn go_to(&mut self, step: Step) -> bool {
        if step == self.step {
            return true;
        }
        let forward = step.index() > self.step.index();
        if forward && self.blocker().is_some() {
            return false;
        }
        let reachable =
            self.visited[step.index()] || (forward && step.index() == self.step.index() + 1);
        if !reachable || self.apply == Applying::Busy {
            return false;
        }
        self.release_audio();
        self.step = step;
        self.visited[step.index()] = true;
        true
    }

    pub fn forward(&mut self) -> bool {
        match Step::ALL.get(self.step.index() + 1) {
            Some(step) => self.go_to(*step),
            None => false,
        }
    }

    pub fn back(&mut self) -> bool {
        match self.step.index().checked_sub(1) {
            Some(i) => self.go_to(Step::ALL[i]),
            None => false,
        }
    }

    /// Close the overlay: audio released, choices kept for the session.
    pub fn close(&mut self) {
        self.release_audio();
    }

    // ----------------------------------------------- read-only facts ---

    /// Start (re)loading a fact: it shows as loading and only the result of
    /// this generation will be accepted.
    pub fn begin_fact(&mut self, fact: Fact) -> u64 {
        let slot = &mut self.fact_generations[fact as usize];
        *slot += 1;
        match fact {
            Fact::Installed => self.installed = Loadable::Loading,
            Fact::Devices => self.devices = Loadable::Loading,
            Fact::Ollama => self.ollama = Loadable::Loading,
        }
        *slot
    }

    fn current(&self, fact: Fact, generation: u64) -> bool {
        self.fact_generations[fact as usize] == generation
    }

    pub fn installed_done(
        &mut self,
        generation: u64,
        result: Result<HashMap<String, bool>, String>,
    ) -> bool {
        if !self.current(Fact::Installed, generation) {
            return false;
        }
        self.installed = result.map_or_else(Loadable::Failed, Loadable::Ready);
        true
    }

    pub fn devices_done(&mut self, generation: u64, result: Result<Vec<String>, String>) -> bool {
        if !self.current(Fact::Devices, generation) {
            return false;
        }
        self.devices = result.map_or_else(Loadable::Failed, Loadable::Ready);
        true
    }

    pub fn ollama_done(
        &mut self,
        generation: u64,
        result: Result<(bool, Vec<String>), String>,
    ) -> bool {
        if !self.current(Fact::Ollama, generation) {
            return false;
        }
        self.ollama = result.map_or_else(
            |e| Loadable::Failed(crate::controllers::jobs::one_line(&e)),
            Loadable::Ready,
        );
        true
    }

    /// Setup is shown again (or a fact may have changed elsewhere, e.g. a
    /// model downloaded or deleted in Models & keys): forget the read-only
    /// facts so the current step reloads them. Choices are untouched and
    /// in-flight workers become stale.
    pub fn invalidate_facts(&mut self) {
        for slot in &mut self.fact_generations {
            *slot += 1;
        }
        self.installed = Loadable::Idle;
        self.devices = Loadable::Idle;
        self.ollama = Loadable::Idle;
        if self.checks != Loadable::Loading {
            self.checks = Loadable::Idle;
        }
    }

    // ---------------------------------------------------------- checkup ---

    pub fn begin_checks(&mut self) -> bool {
        if self.checks == Loadable::Loading {
            return false;
        }
        self.checks = Loadable::Loading;
        true
    }

    pub fn checks_done(&mut self, checks: Vec<Check>) {
        self.checks = Loadable::Ready(checks);
    }

    pub fn failed_checks(&self) -> usize {
        self.checks
            .ready()
            .map_or(0, |c| c.iter().filter(|c| c.ok == Some(false)).count())
    }

    // ------------------------------------------------------- microphone ---

    pub fn set_device(&mut self, device: &str) {
        if self.choices.input_device != device {
            self.release_audio();
            self.mic = MicTest::Off;
            self.choices.input_device = device.to_string();
        }
    }

    /// Test microphone pressed: `(generation, device)` to open off the UI
    /// thread, or `None` when not on this step / already testing.
    pub fn begin_mic_test(&mut self) -> Option<(u64, Option<String>)> {
        if self.step != Step::Microphone
            || matches!(self.mic, MicTest::Opening | MicTest::Testing { .. })
        {
            return None;
        }
        self.mic_generation += 1;
        self.mic = MicTest::Opening;
        Some((
            self.mic_generation,
            Some(self.choices.input_device.clone()).filter(|d| !d.is_empty()),
        ))
    }

    /// The open finished. A stream for a stale generation (the user left the
    /// step or cancelled meanwhile) is closed at once.
    pub fn mic_opened(
        &mut self,
        generation: u64,
        opened: Result<Box<dyn Meter>, String>,
        now: Instant,
    ) {
        match opened {
            Ok(mut stream) => {
                if generation != self.mic_generation || self.mic != MicTest::Opening {
                    stream.stop();
                    return;
                }
                self.mic_stream = Some(stream);
                self.mic_started = Some(now);
                self.mic = MicTest::Testing {
                    level: 0,
                    heard: false,
                };
            }
            Err(error) => {
                if generation == self.mic_generation {
                    self.mic = MicTest::Failed(error);
                }
            }
        }
    }

    /// Poll the level; stops by itself after [`MIC_TEST`]. Returns whether a
    /// test is still running.
    pub fn tick_mic(&mut self, now: Instant) -> bool {
        let (Some(stream), Some(started)) = (self.mic_stream.as_ref(), self.mic_started) else {
            return false;
        };
        let level = stream.level();
        if level >= HEARD_LEVEL && self.mic_heard_at.is_none() {
            self.mic_heard_at = Some(now);
        }
        let heard = self.mic_heard_at.is_some();
        let elapsed = now.saturating_duration_since(started);
        if elapsed >= MIC_TEST || (!heard && elapsed >= SILENT_AFTER) {
            if let Some(mut stream) = self.mic_stream.take() {
                stream.stop();
            }
            self.mic_started = None;
            self.mic = if heard {
                MicTest::Heard
            } else {
                MicTest::Silent
            };
            return false;
        }
        self.mic = MicTest::Testing {
            level: (level.sqrt() * 100.0).round().clamp(0.0, 100.0) as u8,
            heard,
        };
        true
    }

    pub fn mic_open(&self) -> bool {
        self.mic_stream.is_some()
    }

    // ----------------------------------------------------------- speech ---

    pub fn model(&self, id: &str) -> Option<&SttModel> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn select_model(&mut self, id: &str) {
        if self.model(id).is_some() {
            self.choices.stt_id = id.to_string();
            self.trial = Trial::Ready;
        }
    }

    /// `Some(true)` installed, `Some(false)` missing, `None` unknown.
    pub fn installed(&self, id: &str) -> Option<bool> {
        self.installed.ready().and_then(|map| map.get(id).copied())
    }

    // ---------------------------------------------------------- cleanup ---

    /// Local cleanup is offered only with the Ollama provider: switching away
    /// from a remote provider belongs to Models & keys.
    pub fn local_allowed(&self) -> bool {
        self.provider == "ollama"
    }

    pub fn set_cleanup(&mut self, mode: CleanupMode) {
        let mode = match mode {
            CleanupMode::Local if !self.local_allowed() => CleanupMode::KeepProvider,
            CleanupMode::KeepProvider if self.local_allowed() => CleanupMode::Local,
            other => other,
        };
        self.cleanup = mode;
        match mode {
            CleanupMode::Local => {
                self.choices.llm_provider = "ollama".into();
                if self.choices.llm_id.is_none() {
                    self.choices.llm_id = self
                        .recommended_llm
                        .clone()
                        .or_else(|| Some(LLM_MODELS[0].id.to_string()));
                }
            }
            // `config_update` leaves `[llm]` untouched for a non-Ollama
            // provider: the remote provider and its cleanup flag are kept.
            CleanupMode::KeepProvider => {
                self.choices.llm_provider = self.provider.clone();
                self.choices.llm_id = None;
            }
            // `llm_provider "ollama"` + no model: `cleanup_enabled = false`.
            CleanupMode::Off => {
                self.choices.llm_provider = "ollama".into();
                self.choices.llm_id = None;
            }
        }
    }

    pub fn choose_llm(&mut self, id: &str) {
        if self.local_allowed() && LLM_MODELS.iter().any(|m| m.id == id) {
            self.cleanup = CleanupMode::Local;
            self.choices.llm_provider = "ollama".into();
            self.choices.llm_id = Some(id.to_string());
        }
    }

    /// Whether Ollama has the chosen cleanup model (`None` = unknown).
    pub fn llm_installed(&self) -> Option<bool> {
        let id = self.choices.llm_id.as_deref()?;
        let (_, models) = self.ollama.ready()?;
        Some(
            models
                .iter()
                .any(|m| m == id || m.trim_end_matches(":latest") == id),
        )
    }

    // --------------------------------------------------------- shortcut ---

    /// Pick a hotkey option; it's normalised and conflict-checked against
    /// the other four setup shortcuts with `lf_core::shortcuts`.
    pub fn set_hotkey(&mut self, key: &str) {
        self.choices.hotkey = key.to_string();
        self.validate_hotkey();
    }

    fn validate_hotkey(&mut self) {
        let platform = platform_name(self.darwin);
        let raw = self.choices.hotkey.trim().to_string();
        let normalized = if lf_input::is_hold_key(&raw) {
            raw
        } else {
            match lf_core::shortcuts::normalize_optional_combo(&raw, platform) {
                Ok(n) if !n.is_empty() => n,
                Ok(_) => {
                    self.hotkey_error = Some("Choose a key to hold.".into());
                    return;
                }
                Err(e) => {
                    self.hotkey_error = Some(format!("That shortcut isn't valid: {e}"));
                    return;
                }
            }
        };
        self.choices.hotkey = normalized;
        let mut probe = Config::defaults_for(self.darwin);
        for (field, value) in [
            ("hotkey", &self.choices.hotkey),
            ("paste_last_hotkey", &self.choices.paste_last_hotkey),
            ("command_hotkey", &self.choices.command_hotkey),
            ("polish_hotkey", &self.choices.polish_hotkey),
            ("prompt_hotkey", &self.choices.prompt_hotkey),
        ] {
            probe.set_field(field, serde_json::json!(value));
        }
        self.hotkey_error = lf_core::shortcuts::shortcut_conflicts(&probe, &["hotkey"], platform)
            .first()
            .map(|(_, other)| {
                format!(
                    "{} already uses this key. Pick another one, or change that shortcut in Settings later.",
                    match *other {
                        "paste_last_hotkey" => "Paste last",
                        "command_hotkey" => "Command mode",
                        "polish_hotkey" => "Polish",
                        "prompt_hotkey" => "Prompt",
                        other => other,
                    }
                )
            });
    }

    /// `(config key, label)` options with the platform's key names.
    pub fn hotkey_options(&self) -> Vec<(&'static str, String)> {
        HOTKEY_OPTIONS
            .iter()
            .map(|(key, label)| {
                let shown = if self.darwin {
                    label.to_string()
                } else if key.contains('+') {
                    format!(
                        "Press {} to start/stop",
                        lf_ui::hub_model::format_hotkey(Some(key))
                    )
                } else {
                    format!("Hold {}", lf_ui::hub_model::format_hotkey(Some(key)))
                };
                (*key, shown)
            })
            .collect()
    }

    // ------------------------------------------------------------ trial ---

    /// Why the trial can't run right now.
    pub fn trial_blocker(&self, native_speech: bool, runtime_busy: Option<&str>) -> Option<String> {
        if !native_speech {
            return Some(
                "This build can't run the trial because the speech engine isn't included.".into(),
            );
        }
        if let Some(busy) = runtime_busy {
            return Some(busy.to_string());
        }
        if self.installed(&self.choices.stt_id) == Some(false) {
            return Some("Download the speech model first.".into());
        }
        if self.trial_worker.is_some() && self.trial != Trial::Running {
            return Some("Stopping the previous trial…".into());
        }
        None
    }

    /// Start the bounded trial: the work for a background worker.
    pub fn begin_trial(
        &mut self,
        native_speech: bool,
        runtime_busy: Option<&str>,
    ) -> Option<TrialJob> {
        if self.step != Step::Trial
            || self.trial == Trial::Running
            || self.trial_worker.is_some()
            || self.trial_blocker(native_speech, runtime_busy).is_some()
        {
            return None;
        }
        let model = self.model(&self.choices.stt_id)?.clone();
        self.trial_generation += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.trial_cancel = Some(cancel.clone());
        self.trial_worker = Some(self.trial_generation);
        self.trial = Trial::Running;
        Some(TrialJob {
            generation: self.trial_generation,
            cancel,
            model,
            device: Some(self.choices.input_device.clone()).filter(|d| !d.is_empty()),
            language: self.choices.language.clone(),
        })
    }

    pub fn cancel_trial(&mut self) {
        if let Some(cancel) = self.trial_cancel.take() {
            cancel.store(true, Ordering::Release);
            self.trial_generation += 1;
        }
        self.trial = Trial::Ready;
    }

    /// The worker returned; stale results (cancelled, step left) are dropped.
    pub fn trial_done(&mut self, generation: u64, result: Result<String, String>) {
        if self.trial_worker == Some(generation) {
            self.trial_worker = None;
        }
        if generation != self.trial_generation {
            return;
        }
        self.trial_cancel = None;
        self.trial = match result {
            Ok(text) => Trial::Success(text),
            Err(error) => Trial::Failed(crate::controllers::jobs::one_line(&error)),
        };
    }

    pub fn trial_running(&self) -> bool {
        self.trial_cancel.is_some()
    }

    // ----------------------------------------------------------- review ---

    pub fn review_rows(&self) -> Vec<ReviewRow> {
        let model = self
            .model(&self.choices.stt_id)
            .map(|m| m.label.to_string())
            .unwrap_or_else(|| self.choices.stt_id.clone());
        let downloaded = match self.installed(&self.choices.stt_id) {
            Some(true) => " · downloaded",
            Some(false) => " · not downloaded yet",
            None => "",
        };
        let cleanup = match self.cleanup {
            CleanupMode::Local => format!(
                "With Ollama ({})",
                self.choices.llm_id.as_deref().unwrap_or("Ollama")
            ),
            CleanupMode::KeepProvider => format!(
                "Keep {} (cleanup {})",
                provider_label(&self.provider),
                if self.provider_cleanup_on {
                    "on"
                } else {
                    "off"
                }
            ),
            CleanupMode::Off => "No cleanup".into(),
        };
        let mut rows = vec![
            ReviewRow {
                label: "Microphone",
                value: if self.choices.input_device.is_empty() {
                    "System default".into()
                } else {
                    self.choices.input_device.clone()
                },
                step: Step::Microphone,
            },
            ReviewRow {
                label: "Speech model",
                value: format!("{model}{downloaded}"),
                step: Step::Speech,
            },
            ReviewRow {
                label: "Cleanup",
                value: cleanup,
                step: Step::Cleanup,
            },
            ReviewRow {
                label: "Shortcut",
                value: lf_ui::hub_model::format_hotkey(Some(&self.choices.hotkey)),
                step: Step::Shortcut,
            },
        ];
        if self.darwin {
            rows.push(ReviewRow {
                label: "Start at login",
                value: "Unchanged — managed with `lf agent install`".into(),
                step: Step::Review,
            });
        }
        rows
    }

    /// Things to know before applying (listed first on Review).
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.installed(&self.choices.stt_id) == Some(false) {
            out.push(
                "The speech model isn't downloaded yet. LocalFlow can't transcribe until it is."
                    .into(),
            );
        }
        if self.failed_checks() > 0 {
            out.push(format!(
                "{} checkup item(s) need attention. You can fix them later.",
                self.failed_checks()
            ));
        }
        if self.cleanup == CleanupMode::Local && self.llm_installed() == Some(false) {
            out.push(
                "The cleanup model isn't in Ollama yet; cleanup is skipped until it is.".into(),
            );
        }
        if let Some(error) = &self.hotkey_error {
            out.push(error.clone());
        }
        out
    }

    /// Apply pressed: the work for [`apply`], or `None` when blocked/busy.
    pub fn begin_apply(&mut self) -> Option<ApplyJob> {
        if self.apply == Applying::Busy || self.hotkey_error.is_some() {
            return None;
        }
        self.release_audio();
        self.apply = Applying::Busy;
        Some(ApplyJob {
            choices: self.choices.clone(),
            opened: self.opened.clone(),
            acknowledged: self.acknowledged.clone(),
        })
    }

    pub fn apply_done(&mut self, result: Result<String, ApplyFailure>) {
        self.apply = match result {
            Ok(path) => {
                self.initial = self.choices.clone();
                Applying::Done(path)
            }
            Err(ApplyFailure::Conflict { fields, latest }) => {
                // The user has now seen the newer values; a second Apply
                // deliberately replaces them — only for what they changed
                // (the intent baseline `opened` is untouched).
                self.acknowledged = Some(*latest);
                Applying::Failed(format!(
                    "{} changed outside setup since it opened, so nothing was saved. Apply again to replace {} with your choices here.",
                    fields.join(", "),
                    if fields.len() == 1 { "it" } else { "them" }
                ))
            }
            Err(ApplyFailure::Error(error)) => Applying::Failed(error),
        };
    }
}

impl Drop for SetupFlow {
    fn drop(&mut self) {
        self.release_audio();
    }
}

pub fn provider_label(id: &str) -> String {
    lf_providers::keys::get_preset(id)
        .map(|p| p.label.to_string())
        .unwrap_or_else(|_| id.to_string())
}

/// Checkup rows: `run_checks` plus, on Linux, the global-shortcut session
/// and the paste helper (both from what this machine reports).
pub fn checks(config: &Config, platform: &dyn SetupPlatform) -> Vec<Check> {
    let mut rows = flow::run_checks(config, platform);
    if platform.system() == "Linux" {
        for row in rows.iter_mut().filter(|r| r.key == "ffmpeg") {
            if row.ok == Some(false) {
                row.detail = "Not found — install ffmpeg with your package manager".into();
                row.fix_url = String::new();
            }
        }
        let session = lf_input::linux::session();
        let (ok, detail) = match session {
            lf_input::linux::Session::X11 => (
                None,
                "Supported on X11 — not confirmed until LocalFlow is running.".to_string(),
            ),
            lf_input::linux::Session::Wayland => (
                Some(false),
                lf_input::linux::WAYLAND_UNSUPPORTED.to_string(),
            ),
            lf_input::linux::Session::Unknown => (
                None,
                "Couldn't detect the desktop session; global shortcuts are unverified.".to_string(),
            ),
        };
        rows.push(Check::new(
            "global-shortcuts",
            "Global shortcuts",
            ok,
            detail,
            "",
            "",
        ));
        let tool = if session == lf_input::linux::Session::Wayland {
            "wtype"
        } else {
            "xdotool"
        };
        let found = platform.which(tool);
        rows.push(Check::new(
            "paste-tool",
            "Paste helper",
            Some(found),
            if found {
                format!("{tool} is installed")
            } else {
                format!("Not found — install {tool} so LocalFlow can paste")
            },
            "",
            "",
        ));
    }
    rows
}

/// Why Apply didn't save.
#[derive(Clone, Debug)]
pub enum ApplyFailure {
    /// Fields (or coupled field groups) the user changed in setup were also
    /// changed on disk since the draft opened, or since the last conflict
    /// they acknowledged; nothing was written.
    Conflict {
        fields: Vec<String>,
        latest: Box<Config>,
    },
    Error(String),
}

/// What [`apply`] needs from the draft.
#[derive(Clone, Debug)]
pub struct ApplyJob {
    pub choices: SetupChoices,
    /// Immutable intent baseline: the config the draft started from.
    pub opened: Config,
    /// The revision the user acknowledged in a conflict, if any.
    pub acknowledged: Option<Config>,
}

/// Fields that only make sense together. When the user changes any member,
/// the whole group is the intent: it conflicts if any member changed
/// concurrently, and is written as a unit. A local (Ollama) cleanup model is
/// never combined with a provider someone else switched to.
const INTENT_GROUPS: [&[&str]; 2] = [
    &["stt_backend", "model_size", "stt_device"],
    &[
        "llm_provider",
        "llm_base_url",
        "llm_key_ref",
        "llm_model",
        "cleanup_enabled",
    ],
];

const SHORTCUT_FIELDS: [&str; 5] = [
    "hotkey",
    "paste_last_hotkey",
    "command_hotkey",
    "polish_hotkey",
    "prompt_hotkey",
];

fn field_label(key: &str) -> &str {
    match key {
        "hotkey" => "Shortcut",
        "input_device" => "Microphone",
        "stt_backend" | "model_size" | "language" | "stt_device" => "Speech model",
        "llm_model" | "cleanup_enabled" | "llm_provider" | "llm_base_url" | "llm_key_ref" => {
            "Cleanup"
        }
        "cleanup_level" => "Cleanup level",
        "paste_last_hotkey" => "Paste last",
        "command_hotkey" => "Command mode",
        "polish_hotkey" => "Polish",
        "prompt_hotkey" => "Prompt",
        other => other,
    }
}

/// Write the draft to the explicit config file. Blocking; call off the UI
/// thread.
///
/// `setup_flow::config_update` turns the choices into the setup sections;
/// applied to `job.opened` (the immutable baseline) that shows what the user
/// changed, widened to whole [`INTENT_GROUPS`]. Under
/// `config_write::with_lock` the latest file is loaded (`missing` supplies
/// the explicit root's defaults when there is no file yet — no separate
/// preparatory write) and only those fields are written; everything else,
/// including setup fields the user left alone, keeps any newer value. A
/// changed field that differs on disk from the expected revision (the
/// acknowledged conflict, else the baseline) is a conflict. The merged
/// result's changed shortcuts are checked against every other shortcut
/// with `lf_core::shortcuts` before anything is saved. Start at login is
/// never touched (macOS: `lf agent`).
pub fn apply(
    job: &ApplyJob,
    config_path: &std::path::Path,
    missing: impl Fn() -> anyhow::Result<Config>,
) -> Result<String, ApplyFailure> {
    use flow::SetupConfigStore as _;
    let store = flow::FileConfigStore::at(config_path);
    let failure = |e: anyhow::Error| ApplyFailure::Error(format!("{e:#}"));
    let opened = &job.opened;
    let expected = job.acknowledged.as_ref().unwrap_or(opened);
    lf_ui::config_write::with_lock(|| {
        let updates =
            flow::config_update(&job.choices, opened).map_err(|e| ApplyFailure::Error(e.0))?;
        let edited_config =
            lf_core::config::apply_config_update(opened, &updates).map_err(failure)?;
        let mut edited: std::collections::BTreeSet<String> = edited_config
            .fields
            .keys()
            .filter(|k| edited_config.field(k) != opened.field(k))
            .cloned()
            .collect();
        for group in INTENT_GROUPS {
            if group.iter().any(|k| edited.contains(*k)) {
                edited.extend(group.iter().map(|k| k.to_string()));
            }
        }
        // Another process may create the file while we decide; never
        // replace a file we didn't read.
        for _ in 0..2 {
            let existed = config_path.exists();
            let latest = if existed {
                store.load().map_err(failure)?
            } else {
                missing().map_err(failure)?
            };
            let conflicts: std::collections::BTreeSet<&str> = edited
                .iter()
                .filter(|k| {
                    latest.field(k) != expected.field(k)
                        && latest.field(k) != edited_config.field(k)
                })
                .map(|k| field_label(k))
                .collect();
            if !conflicts.is_empty() {
                return Err(ApplyFailure::Conflict {
                    fields: conflicts.into_iter().map(str::to_string).collect(),
                    latest: Box::new(latest),
                });
            }
            let mut merged = latest.clone();
            for key in &edited {
                match edited_config.field(key) {
                    Some(value) => merged.set_field(key, value.clone()),
                    None => {
                        merged.fields.remove(key);
                    }
                }
            }
            let changed: Vec<&str> = SHORTCUT_FIELDS
                .iter()
                .copied()
                .filter(|k| edited.contains(*k))
                .collect();
            let platform = crate::controllers::settings::platform();
            if let Some((mine, other)) =
                lf_core::shortcuts::shortcut_conflicts(&merged, &changed, platform).first()
            {
                return Err(ApplyFailure::Error(format!(
                    "{} now uses the same key as your {} choice (changed elsewhere while setup was open). Nothing was saved; pick another key.",
                    field_label(other),
                    field_label(mine)
                )));
            }
            if config_path.exists() != existed {
                continue; // created meanwhile: decide again against it
            }
            return store
                .save(&merged)
                .map(|path| path.display().to_string())
                .map_err(failure);
        }
        Err(ApplyFailure::Error(
            "The config file kept changing while saving. Nothing was saved; try again.".into(),
        ))
    })
}

/// The native speech engines compiled into this binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeEngines {
    pub whisper_cpp: bool,
    pub parakeet_onnx: bool,
}

impl NativeEngines {
    /// `lf_stt::factory::backend_available` for runtime builds; none
    /// otherwise.
    pub fn compiled() -> Self {
        #[cfg(feature = "runtime")]
        {
            NativeEngines {
                whisper_cpp: lf_stt::factory::backend_available("faster-whisper"),
                parakeet_onnx: lf_stt::factory::backend_available("parakeet"),
            }
        }
        #[cfg(not(feature = "runtime"))]
        {
            NativeEngines {
                whisper_cpp: false,
                parakeet_onnx: false,
            }
        }
    }
}

/// Which catalogue backends setup may offer. faster-whisper sizes are
/// always offered (the Python runtime runs them; a native build runs the
/// same sizes on whisper.cpp). Everything else follows the canonical native
/// contract `setup_flow::native_backend_available`: Parakeet when its ONNX
/// engine is compiled in (any platform), MLX never — native Whisper is
/// whisper.cpp and isn't relabelled as MLX. The catalogue's own hardware
/// filter (`available_stt_models`) still applies on top.
pub fn desktop_runtime_available(backend: &str, native: NativeEngines) -> bool {
    backend == "faster-whisper"
        || flow::native_backend_available(backend, native.whisper_cpp, native.parakeet_onnx)
}

/// The platform the desktop app reports to setup: standard-library facts
/// from `HostPlatform`, real input devices from `lf_audio`, and the
/// speech runtimes this build has. Permissions it can't check stay unknown.
pub struct DesktopPlatform {
    host: flow::HostPlatform,
}

impl DesktopPlatform {
    pub fn new() -> Self {
        DesktopPlatform {
            host: flow::HostPlatform,
        }
    }
}

impl Default for DesktopPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl SetupPlatform for DesktopPlatform {
    fn system(&self) -> String {
        self.host.system()
    }
    fn machine(&self) -> String {
        self.host.machine()
    }
    fn which(&self, command: &str) -> bool {
        self.host.which(command)
    }
    fn ollama_installed(&self) -> bool {
        self.host.ollama_installed()
    }
    fn ollama_running(&self, base_url: &str) -> Option<bool> {
        let url = if base_url.is_empty() {
            "http://localhost:11434"
        } else {
            base_url
        };
        use lf_providers::LlmProvider as _;
        Some(
            lf_providers::ollama::OllamaLlm::new(url, "", 2, 8192)
                .list_models()
                .is_ok(),
        )
    }
    fn input_devices(&self) -> Result<Vec<String>, String> {
        Ok(lf_audio::list_input_devices())
    }
    fn runtime_available(&self, backend: &str) -> bool {
        desktop_runtime_available(backend, NativeEngines::compiled())
    }
    fn mic_authorization(&self) -> Option<flow::MicAuthorization> {
        None
    }
    fn accessibility_trusted(&self) -> Option<bool> {
        None
    }
    fn input_monitoring_granted(&self) -> Option<bool> {
        lf_input::listener::input_monitoring_granted()
    }
    fn detect_hardware(&self) -> Result<HardwareInfo, String> {
        self.host.detect_hardware()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow::{FakeLaunchAgent, FakeSetupPlatform};
    use std::sync::Mutex;

    fn hw() -> HardwareInfo {
        HardwareInfo::new("Linux", "x86_64", 16.0, false, 100.0)
    }

    fn apply_at(job: &ApplyJob, path: &std::path::Path) -> Result<String, ApplyFailure> {
        apply(job, path, || Ok(Config::defaults_for(false)))
    }

    /// A configured (setup_version 1) file at a temp path, returned loaded.
    fn configured(
        dir: &std::path::Path,
        edit: impl FnOnce(&mut Config),
    ) -> (std::path::PathBuf, Config) {
        let path = dir.join("config.toml");
        let mut config = Config::defaults_for(false);
        config.set_field("setup_version", serde_json::json!(1));
        config.set_field("hotkey", serde_json::json!("f19"));
        edit(&mut config);
        lf_core::config::save_config(&config, Some(&path)).unwrap();
        (path.clone(), lf_core::config::load_from(&path).unwrap())
    }

    fn external(path: &std::path::Path, edit: impl FnOnce(&mut Config)) {
        let mut config = lf_core::config::load_from(path).unwrap();
        edit(&mut config);
        lf_core::config::save_config(&config, Some(path)).unwrap();
    }

    #[test]
    fn resuming_shows_fresh_shared_inventory_and_drops_superseded_results() {
        use crate::controllers::model_storage::SpeechStorage;
        use lf_ui::setup_backend::SetupModelBackend;
        let state = crate::sandbox::SimSpeech::seeded();
        let backend =
            crate::sandbox::SimSetupBackend::with_state(state.clone(), Duration::from_millis(1));
        let storage = crate::sandbox::SimStorage::new(state);
        let mut flow = new_flow(&Config::defaults_for(false));
        flow.select_model("fw-small");
        let inspect = |flow: &mut SetupFlow| {
            let generation = flow.begin_fact(Fact::Installed);
            let found = backend.speech_downloads(&flow.models);
            assert!(flow.installed_done(generation, found));
        };
        inspect(&mut flow);
        assert_eq!(flow.installed("fw-small"), Some(true));
        // While setup is hidden, Models & keys deletes the Whisper models.
        let mut parakeet = Config::defaults_for(false);
        parakeet.set_field("stt_backend", serde_json::json!("parakeet"));
        storage
            .delete(crate::sandbox::lf_stt_repo::WHISPER_CPP, &parakeet)
            .unwrap();
        assert_eq!(
            flow.installed("fw-small"),
            Some(true),
            "stale until resumed"
        );
        // An inspection started before the resume returns late: dropped.
        let old = flow.begin_fact(Fact::Installed);
        flow.invalidate_facts();
        assert_eq!(flow.installed, Loadable::Idle);
        assert_eq!(flow.choices.stt_id, "fw-small", "choices survive a resume");
        let mut stale = HashMap::new();
        stale.insert("fw-small".to_string(), true);
        assert!(!flow.installed_done(old, Ok(stale)));
        inspect(&mut flow);
        assert_eq!(
            flow.installed("fw-small"),
            Some(false),
            "fresh after returning"
        );
        // A newly plugged microphone shows up on refresh even after a
        // successful non-empty list.
        let g = flow.begin_fact(Fact::Devices);
        assert!(flow.devices_done(g, Ok(vec!["Built-in".into()])));
        let g = flow.begin_fact(Fact::Devices);
        assert!(flow.devices_done(g, Ok(vec!["Built-in".into(), "USB Headset".into()])));
        assert_eq!(flow.devices.ready().map(Vec::len), Some(2));
        // Ollama list follows a cleanup pull made elsewhere.
        backend
            .pull_cleanup(
                "",
                "gemma3:4b",
                Box::new(|_| {}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        flow.invalidate_facts();
        let g = flow.begin_fact(Fact::Ollama);
        assert!(flow.ollama_done(g, backend.ollama("")));
        assert!(flow
            .ollama
            .ready()
            .unwrap()
            .1
            .contains(&"gemma3:4b".to_string()));
    }

    #[test]
    fn native_capability_exposes_compiled_parakeet_and_never_mlx() {
        let platform_for = |native: NativeEngines, apple: bool| FakeSetupPlatform {
            system: if apple { "Darwin" } else { "Linux" }.into(),
            machine: if apple { "arm64" } else { "x86_64" }.into(),
            runtimes: ["faster-whisper", "parakeet", "mlx-whisper"]
                .into_iter()
                .filter(|b| desktop_runtime_available(b, native))
                .map(String::from)
                .collect(),
            ..FakeSetupPlatform::default()
        };
        let ids = |native: NativeEngines, apple: bool| -> Vec<String> {
            let hw = HardwareInfo::new(
                if apple { "Darwin" } else { "Linux" },
                if apple { "arm64" } else { "x86_64" },
                16.0,
                apple,
                100.0,
            );
            flow::available_stt_models(&hw, &platform_for(native, apple))
                .into_iter()
                .map(|m| m.id)
                .collect()
        };
        let both = NativeEngines {
            whisper_cpp: true,
            parakeet_onnx: true,
        };
        let none = NativeEngines {
            whisper_cpp: false,
            parakeet_onnx: false,
        };
        // Runtime-enabled native Mac: the compiled Parakeet ONNX engine shows.
        let mac = ids(both, true);
        assert!(mac.contains(&"parakeet-v2".to_string()), "{mac:?}");
        assert!(
            mac.iter().all(|id| !id.starts_with("mlx-")),
            "native Whisper isn't MLX: {mac:?}"
        );
        assert!(mac.contains(&"fw-base".to_string()));
        // Without the engine (default build) it isn't offered.
        let default_mac = ids(none, true);
        assert!(!default_mac.contains(&"parakeet-v2".to_string()));
        assert!(
            default_mac.contains(&"fw-base".to_string()),
            "Python runtime sizes stay"
        );
        // The canonical catalogue keeps Parakeet Apple-silicon-only elsewhere.
        assert!(!ids(both, false).contains(&"parakeet-v2".to_string()));
        assert!(desktop_runtime_available("faster-whisper", none));
        assert!(!desktop_runtime_available("mlx-whisper", both));
        assert!(!desktop_runtime_available(
            "parakeet",
            NativeEngines {
                whisper_cpp: true,
                parakeet_onnx: false
            }
        ));
    }

    #[test]
    fn retry_after_a_conflict_writes_only_the_users_edits() {
        let dir = tempfile::tempdir().unwrap();
        let (path, opened) = configured(dir.path(), |_| {});
        let mut flow = new_flow(&opened);
        flow.set_hotkey("ctrl_r");
        // Elsewhere: the same shortcut AND hidden fields the wizard carries.
        external(&path, |c| {
            c.set_field("hotkey", serde_json::json!("alt_l"));
            c.set_field("max_recording_seconds", serde_json::json!(90));
            c.set_field("polish_hotkey", serde_json::json!("<ctrl>+<alt>+9"));
        });
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        assert!(matches!(&flow.apply, Applying::Failed(m) if m.contains("Shortcut")));
        assert_eq!(
            flow.opened.field_str("hotkey"),
            Some("f19"),
            "intent baseline kept"
        );
        // A later change after the acknowledgement is a new conflict.
        external(&path, |c| c.set_field("hotkey", serde_json::json!("f18")));
        let before = std::fs::read(&path).unwrap();
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        assert!(matches!(&flow.apply, Applying::Failed(m) if m.contains("Shortcut")));
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing written");
        // Acknowledged: replace only the hotkey, keep the hidden edits.
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        assert!(matches!(flow.apply, Applying::Done(_)), "{:?}", flow.apply);
        let saved = lf_core::config::load_from(&path).unwrap();
        assert_eq!(saved.field_str("hotkey"), Some("ctrl_r"));
        assert_eq!(saved.field_i64("max_recording_seconds"), Some(90));
        assert_eq!(saved.field_str("polish_hotkey"), Some("<ctrl>+<alt>+9"));
    }

    #[test]
    fn a_shortcut_taken_elsewhere_meanwhile_is_refused_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (path, opened) = configured(dir.path(), |_| {});
        let mut flow = new_flow(&opened);
        flow.set_hotkey("ctrl_r");
        assert!(flow.hotkey_error.is_none(), "free when setup opened");
        external(&path, |c| {
            c.set_field("command_hotkey", serde_json::json!("ctrl_r"))
        });
        let before = std::fs::read(&path).unwrap();
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        assert!(
            matches!(&flow.apply, Applying::Failed(m) if m.contains("Command mode")),
            "{:?}",
            flow.apply
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "zero writes");
        assert!(flow.dirty(), "draft kept");
    }

    #[test]
    fn a_speech_model_choice_is_not_split_from_a_concurrent_engine_change() {
        let dir = tempfile::tempdir().unwrap();
        let (path, opened) = configured(dir.path(), |c| {
            c.set_field("stt_backend", serde_json::json!("faster-whisper"));
            c.set_field("model_size", serde_json::json!("base"));
        });
        let mut flow = new_flow(&opened);
        assert_eq!(flow.choices.stt_id, "fw-base");
        flow.select_model("fw-small");
        external(&path, |c| {
            c.set_field("stt_backend", serde_json::json!("parakeet"))
        });
        let before = std::fs::read(&path).unwrap();
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        assert!(matches!(&flow.apply, Applying::Failed(m) if m.contains("Speech model")));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "no parakeet/small mix"
        );
        // Acknowledged: the reviewed selection is written as a whole.
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        let saved = lf_core::config::load_from(&path).unwrap();
        assert_eq!(saved.field_str("stt_backend"), Some("faster-whisper"));
        assert_eq!(saved.field_str("model_size"), Some("small"));
    }

    #[test]
    fn a_local_cleanup_choice_is_never_rerouted_to_a_remote_provider() {
        let dir = tempfile::tempdir().unwrap();
        let (path, opened) = configured(dir.path(), |c| {
            c.set_field("llm_provider", serde_json::json!("ollama"));
            c.set_field("llm_model", serde_json::json!("llama3.2:1b"));
            c.set_field("cleanup_enabled", serde_json::json!(true));
        });
        let mut flow = new_flow(&opened);
        assert_eq!(flow.cleanup, CleanupMode::Local);
        flow.choose_llm("qwen3:4b");
        external(&path, |c| {
            c.set_field("llm_provider", serde_json::json!("openai"));
            c.set_field(
                "llm_base_url",
                serde_json::json!("https://api.openai.com/v1"),
            );
        });
        let before = std::fs::read(&path).unwrap();
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        assert!(matches!(&flow.apply, Applying::Failed(m) if m.contains("Cleanup")));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "no OpenAI + local model mix"
        );
        // Acknowledged: the local choice is written coherently (Ollama).
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        let saved = lf_core::config::load_from(&path).unwrap();
        assert_eq!(saved.field_str("llm_provider"), Some("ollama"));
        assert_eq!(saved.field_str("llm_model"), Some("qwen3:4b"));
        assert_eq!(
            saved.field_str("llm_base_url"),
            opened.field_str("llm_base_url")
        );
    }

    #[test]
    fn untouched_selections_keep_concurrent_engine_and_provider_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (path, opened) = configured(dir.path(), |c| {
            c.set_field("stt_backend", serde_json::json!("faster-whisper"));
            c.set_field("model_size", serde_json::json!("base"));
            c.set_field("llm_provider", serde_json::json!("ollama"));
            c.set_field("llm_model", serde_json::json!("llama3.2:1b"));
            c.set_field("cleanup_enabled", serde_json::json!(true));
        });
        let mut flow = new_flow(&opened);
        flow.set_device("Desk Mic");
        external(&path, |c| {
            c.set_field("stt_backend", serde_json::json!("parakeet"));
            c.set_field("llm_provider", serde_json::json!("openai"));
            c.set_field("llm_model", serde_json::json!("gpt-5-mini"));
        });
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        assert!(matches!(flow.apply, Applying::Done(_)), "{:?}", flow.apply);
        let saved = lf_core::config::load_from(&path).unwrap();
        assert_eq!(saved.field_str("input_device"), Some("Desk Mic"));
        assert_eq!(saved.field_str("stt_backend"), Some("parakeet"));
        assert_eq!(saved.field_str("llm_provider"), Some("openai"));
        assert_eq!(saved.field_str("llm_model"), Some("gpt-5-mini"));
    }

    #[test]
    fn apply_without_a_config_file_starts_from_the_roots_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config").join("config.toml");
        let mut defaults = Config::defaults_for(false);
        defaults.set_field("meetings_enabled", serde_json::json!(false));
        let mut flow = new_flow(&defaults);
        flow.set_device("USB Mic");
        let job = flow.begin_apply().unwrap();
        let fallback = defaults.clone();
        flow.apply_done(apply(&job, &path, move || Ok(fallback.clone())));
        assert!(matches!(flow.apply, Applying::Done(_)), "{:?}", flow.apply);
        let saved = lf_core::config::load_from(&path).unwrap();
        assert_eq!(saved.field_str("input_device"), Some("USB Mic"));
        assert_eq!(
            saved.field_bool("meetings_enabled"),
            Some(false),
            "explicit-root defaults"
        );
        assert_eq!(saved.field_i64("setup_version"), Some(1));
    }

    fn new_flow(config: &Config) -> SetupFlow {
        let platform = FakeSetupPlatform {
            ollama_installed: true,
            ..FakeSetupPlatform::default()
        };
        SetupFlow::new(
            config,
            &hw(),
            &platform,
            &FakeLaunchAgent::new("not installed"),
            true,
        )
        .unwrap()
    }

    #[derive(Clone, Default)]
    struct Counter(Arc<Mutex<(usize, f64)>>);
    struct FakeMeter(Counter);
    impl Meter for FakeMeter {
        fn level(&self) -> f64 {
            self.0 .0.lock().unwrap().1
        }
        fn stop(&mut self) {
            self.0 .0.lock().unwrap().0 += 1;
        }
    }

    fn walk_to(flow: &mut SetupFlow, step: Step) {
        while flow.step != step {
            assert!(flow.forward(), "blocked at {:?}", flow.step);
        }
    }

    #[test]
    fn steps_are_reached_in_order_and_back_is_always_free() {
        let mut flow = new_flow(&Config::defaults_for(false));
        assert!(
            !flow.go_to(Step::Review),
            "unvisited steps aren't reachable"
        );
        flow.set_hotkey("f19");
        walk_to(&mut flow, Step::Review);
        assert!(flow.go_to(Step::Microphone));
        assert!(flow.go_to(Step::Review), "visited steps are");
        assert!(flow.back());
        assert_eq!(flow.step, Step::Trial);
    }

    #[test]
    fn shortcut_conflicts_block_continue_and_review() {
        let mut flow = new_flow(&Config::defaults_for(false));
        walk_to(&mut flow, Step::Shortcut);
        flow.set_hotkey("alt_r"); // the default command-mode key
        assert!(flow
            .hotkey_error
            .as_deref()
            .unwrap()
            .contains("Command mode"));
        assert!(!flow.forward());
        assert!(flow.begin_apply().is_none());
        flow.set_hotkey("<ctrl>+<alt>+1");
        assert!(flow.hotkey_error.is_some(), "Polish uses it");
        flow.set_hotkey("ctrl_r");
        assert!(flow.hotkey_error.is_none());
        assert_eq!(flow.choices.hotkey, "ctrl_r");
        assert!(flow.forward());
        let labels: Vec<String> = flow.hotkey_options().into_iter().map(|(_, l)| l).collect();
        assert!(
            labels.iter().all(|l| !l.contains('⌘')),
            "no Mac glyphs on Linux: {labels:?}"
        );
    }

    #[test]
    fn microphone_opens_only_on_request_and_is_released_exactly_once() {
        let mut flow = new_flow(&Config::defaults_for(false));
        let counter = Counter::default();
        assert!(
            flow.begin_mic_test().is_none(),
            "not on the microphone step"
        );
        walk_to(&mut flow, Step::Microphone);
        assert!(!flow.mic_open());
        let (generation, device) = flow.begin_mic_test().unwrap();
        assert_eq!(device, None);
        let now = Instant::now();
        flow.mic_opened(generation, Ok(Box::new(FakeMeter(counter.clone()))), now);
        assert!(flow.mic_open());
        counter.0.lock().unwrap().1 = 0.3;
        assert!(flow.tick_mic(now + Duration::from_secs(1)));
        assert!(matches!(flow.mic, MicTest::Testing { heard: true, .. }));
        assert!(flow.forward(), "leaving the step");
        assert!(!flow.mic_open());
        assert_eq!(counter.0.lock().unwrap().0, 1, "stopped once");
        flow.close();
        assert_eq!(counter.0.lock().unwrap().0, 1, "not twice");

        // A late open after leaving is closed immediately.
        flow.back();
        let (generation, _) = flow.begin_mic_test().unwrap();
        flow.back();
        let late = Counter::default();
        flow.mic_opened(generation, Ok(Box::new(FakeMeter(late.clone()))), now);
        assert_eq!(late.0.lock().unwrap().0, 1);
        assert!(!flow.mic_open());
    }

    #[test]
    fn deactivation_cancels_an_opening_meter_a_running_meter_and_a_trial() {
        let mut flow = new_flow(&Config::defaults_for(false));
        walk_to(&mut flow, Step::Microphone);
        // Opening (a permission prompt may be what took focus).
        let (generation, _) = flow.begin_mic_test().unwrap();
        assert!(flow.release_audio(), "a pending open is cancelled");
        assert_eq!(flow.mic, MicTest::Off);
        let late = Counter::default();
        flow.mic_opened(
            generation,
            Ok(Box::new(FakeMeter(late.clone()))),
            Instant::now(),
        );
        assert_eq!(
            late.0.lock().unwrap().0,
            1,
            "the late stream is closed at once"
        );
        assert!(!flow.mic_open());
        // The user can press Test again afterwards.
        let (generation, _) = flow.begin_mic_test().unwrap();
        let open = Counter::default();
        flow.mic_opened(
            generation,
            Ok(Box::new(FakeMeter(open.clone()))),
            Instant::now(),
        );
        assert!(flow.release_audio(), "a running meter is stopped");
        assert_eq!(open.0.lock().unwrap().0, 1);
        // A trial is cancelled and keeps its slot until the worker returns.
        walk_to(&mut flow, Step::Trial);
        let job = flow.begin_trial(true, None).unwrap();
        assert!(flow.release_audio());
        assert!(job.cancel.load(Ordering::Acquire));
        assert!(
            flow.begin_trial(true, None).is_none(),
            "worker may still hold the mic"
        );
        flow.trial_done(job.generation, Err("Trial cancelled.".into()));
        assert_eq!(flow.trial, Trial::Ready);
        assert!(flow.begin_trial(true, None).is_some());
    }

    #[test]
    fn silent_microphone_and_open_failure_and_auto_stop() {
        let mut flow = new_flow(&Config::defaults_for(false));
        walk_to(&mut flow, Step::Microphone);
        let counter = Counter::default();
        let (g, _) = flow.begin_mic_test().unwrap();
        let start = Instant::now();
        flow.mic_opened(g, Ok(Box::new(FakeMeter(counter.clone()))), start);
        assert!(!flow.tick_mic(start + SILENT_AFTER));
        assert_eq!(flow.mic, MicTest::Silent);
        assert_eq!(counter.0.lock().unwrap().0, 1);
        let (g, _) = flow.begin_mic_test().unwrap();
        flow.mic_opened(g, Err("permission denied".into()), start);
        assert_eq!(flow.mic, MicTest::Failed("permission denied".into()));
        let loud = Counter::default();
        loud.0.lock().unwrap().1 = 0.5;
        let (g, _) = flow.begin_mic_test().unwrap();
        flow.mic_opened(g, Ok(Box::new(FakeMeter(loud.clone()))), start);
        assert!(flow.tick_mic(start + Duration::from_secs(1)));
        assert!(!flow.tick_mic(start + MIC_TEST));
        assert_eq!(flow.mic, MicTest::Heard);
        assert_eq!(loud.0.lock().unwrap().0, 1);
    }

    #[test]
    fn trial_is_bounded_cancellable_and_requires_an_idle_runtime() {
        let mut flow = new_flow(&Config::defaults_for(false));
        walk_to(&mut flow, Step::Trial);
        assert!(flow
            .trial_blocker(false, None)
            .unwrap()
            .contains("speech engine"));
        assert!(flow.begin_trial(false, None).is_none());
        assert!(flow.begin_trial(true, Some("busy")).is_none());
        let job = flow.begin_trial(true, None).unwrap();
        let (g, cancel) = (job.generation, job.cancel);
        assert_eq!(job.model.id, flow.choices.stt_id);
        assert!(flow.begin_trial(true, None).is_none(), "one at a time");
        flow.cancel_trial();
        assert!(cancel.load(Ordering::Acquire));
        flow.trial_done(g, Ok("late".into()));
        assert_eq!(flow.trial, Trial::Ready, "a cancelled result is dropped");
        let job = flow.begin_trial(true, None).unwrap();
        let (g, cancel) = (job.generation, job.cancel);
        assert!(flow.back(), "leaving the step cancels");
        assert!(cancel.load(Ordering::Acquire));
        flow.forward();
        flow.trial_done(g, Ok("stale".into()));
        assert_eq!(flow.trial, Trial::Ready);
        // The last cancelled worker hasn't returned: no second trial yet.
        let g = flow.begin_trial(true, None).unwrap().generation;
        flow.cancel_trial();
        assert!(flow.begin_trial(true, None).is_none());
        assert!(flow
            .trial_blocker(true, None)
            .unwrap()
            .contains("previous trial"));
        flow.trial_done(g, Ok("late".into()));
        assert_eq!(flow.trial, Trial::Ready);
        let g = flow.begin_trial(true, None).unwrap().generation;
        flow.trial_done(g, Err("No speech was recognized.".into()));
        assert_eq!(
            flow.trial,
            Trial::Failed("No speech was recognized.".into())
        );
        let mut missing = HashMap::new();
        missing.insert(flow.choices.stt_id.clone(), false);
        flow.installed = Loadable::Ready(missing);
        assert!(flow.trial_blocker(true, None).unwrap().contains("Download"));
    }

    #[test]
    fn cleanup_modes_map_to_config_update_exactly() {
        let mut flow = new_flow(&Config::defaults_for(false));
        flow.set_cleanup(CleanupMode::Off);
        assert_eq!(
            (
                flow.choices.llm_provider.as_str(),
                flow.choices.llm_id.as_deref()
            ),
            ("ollama", None)
        );
        let update = flow::config_update(&flow.choices, &Config::defaults_for(false)).unwrap();
        assert_eq!(update["llm"]["cleanup_enabled"], false);
        flow.choose_llm("qwen3:4b");
        assert_eq!(flow.cleanup, CleanupMode::Local);
        let update = flow::config_update(&flow.choices, &Config::defaults_for(false)).unwrap();
        assert_eq!(update["llm"]["model"], "qwen3:4b");

        let mut remote = Config::defaults_for(false);
        remote.set_field("llm_provider", serde_json::json!("openai"));
        remote.set_field("llm_model", serde_json::json!("gpt-5-mini"));
        let mut flow = new_flow(&remote);
        assert_eq!(flow.cleanup, CleanupMode::KeepProvider);
        assert!(!flow.local_allowed());
        flow.set_cleanup(CleanupMode::Local);
        assert_eq!(
            flow.cleanup,
            CleanupMode::KeepProvider,
            "no silent provider switch"
        );
        flow.choose_llm("qwen3:4b");
        assert_eq!(flow.choices.llm_provider, "openai");
        let update = flow::config_update(&flow.choices, &remote).unwrap();
        assert!(
            update.get("llm").is_none(),
            "[llm] untouched for a remote provider"
        );
    }

    #[test]
    fn draft_survives_navigation_and_apply_preserves_concurrent_unrelated_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut original = Config::defaults_for(false);
        original.set_field("llm_model", serde_json::json!("llama3.2:1b"));
        lf_core::config::save_config(&original, Some(&path)).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut flow = new_flow(&lf_core::config::load_from(&path).unwrap());
        assert!(!flow.dirty());
        walk_to(&mut flow, Step::Microphone);
        flow.set_device("USB Mic");
        walk_to(&mut flow, Step::Shortcut);
        flow.set_hotkey("f19");
        flow.go_to(Step::Checkup);
        flow.close();
        assert_eq!(flow.choices.input_device, "USB Mic");
        assert_eq!(flow.choices.hotkey, "f19");
        assert!(flow.dirty());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "nothing written before Apply"
        );

        // Another process edits unrelated fields meanwhile.
        let mut external = lf_core::config::load_from(&path).unwrap();
        external.set_field("sounds_enabled", serde_json::json!(false));
        external.set_field("history_enabled", serde_json::json!(false));
        lf_core::config::save_config(&external, Some(&path)).unwrap();

        let job = flow.begin_apply().unwrap();
        assert!(flow.begin_apply().is_none(), "busy");
        let result = apply_at(&job, &path);
        flow.apply_done(result);
        assert!(matches!(flow.apply, Applying::Done(_)));
        assert!(!flow.dirty());
        let saved = lf_core::config::load_from(&path).unwrap();
        assert_eq!(saved.field_str("input_device"), Some("USB Mic"));
        assert_eq!(saved.field_str("hotkey"), Some("f19"));
        assert_eq!(saved.field_i64("setup_version"), Some(1));
        assert_eq!(
            saved.field_bool("sounds_enabled"),
            Some(false),
            "concurrent edit kept"
        );
        assert_eq!(saved.field_bool("history_enabled"), Some(false));
    }

    #[test]
    fn apply_keeps_concurrent_hidden_field_edits_and_reports_true_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut original = Config::defaults_for(false);
        original.set_field("setup_version", serde_json::json!(1));
        original.set_field("hotkey", serde_json::json!("f19"));
        original.set_field("max_recording_seconds", serde_json::json!(1200));
        lf_core::config::save_config(&original, Some(&path)).unwrap();
        let opened = lf_core::config::load_from(&path).unwrap();
        let mut flow = new_flow(&opened);
        flow.set_device("Desk Mic");
        // Another process edits fields the wizard carries but the user
        // didn't touch: the recording limit and another shortcut.
        let mut external = lf_core::config::load_from(&path).unwrap();
        external.set_field("max_recording_seconds", serde_json::json!(90));
        external.set_field("polish_hotkey", serde_json::json!("<ctrl>+<alt>+9"));
        lf_core::config::save_config(&external, Some(&path)).unwrap();
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        assert!(matches!(flow.apply, Applying::Done(_)), "{:?}", flow.apply);
        let saved = lf_core::config::load_from(&path).unwrap();
        assert_eq!(
            saved.field_str("input_device"),
            Some("Desk Mic"),
            "the user's edit"
        );
        assert_eq!(
            saved.field_i64("max_recording_seconds"),
            Some(90),
            "concurrent edit kept"
        );
        assert_eq!(saved.field_str("polish_hotkey"), Some("<ctrl>+<alt>+9"));

        // A field the user changed here was also changed on disk: conflict.
        let opened = lf_core::config::load_from(&path).unwrap();
        let mut flow = new_flow(&opened);
        flow.set_hotkey("ctrl_r");
        let mut external = lf_core::config::load_from(&path).unwrap();
        external.set_field("hotkey", serde_json::json!("alt_l"));
        lf_core::config::save_config(&external, Some(&path)).unwrap();
        let before = std::fs::read(&path).unwrap();
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        let Applying::Failed(message) = &flow.apply else {
            panic!("expected a conflict, got {:?}", flow.apply);
        };
        assert!(message.contains("Shortcut"), "{message}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing written");
        // Applying again after seeing the conflict replaces it explicitly.
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        assert!(matches!(flow.apply, Applying::Done(_)));
        assert_eq!(
            lf_core::config::load_from(&path)
                .unwrap()
                .field_str("hotkey"),
            Some("ctrl_r")
        );
    }

    #[test]
    fn apply_failure_keeps_the_draft() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not = [valid toml").unwrap();
        let mut flow = new_flow(&Config::defaults_for(false));
        flow.set_hotkey("f19");
        let job = flow.begin_apply().unwrap();
        flow.apply_done(apply_at(&job, &path));
        assert!(matches!(flow.apply, Applying::Failed(_)));
        assert!(flow.dirty(), "choices kept");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not = [valid toml");
        assert!(flow.begin_apply().is_some(), "retry possible");
    }

    #[test]
    fn review_rows_and_warnings_come_from_live_state() {
        let mut flow = new_flow(&Config::defaults_for(false));
        let mut installed = HashMap::new();
        installed.insert(flow.choices.stt_id.clone(), false);
        flow.installed = Loadable::Ready(installed);
        assert!(flow.warnings()[0].contains("isn't downloaded"));
        flow.set_device("Desk Mic");
        let rows = flow.review_rows();
        assert_eq!(rows[0].value, "Desk Mic");
        assert!(rows[1].value.contains("not downloaded"));
        assert_eq!(rows[1].step, Step::Speech);
    }

    #[test]
    fn linux_checkup_adds_session_and_paste_rows() {
        let platform = FakeSetupPlatform::default();
        let rows = checks(&Config::defaults_for(false), &platform);
        assert!(rows.iter().any(|r| r.key == "global-shortcuts"));
        assert!(rows
            .iter()
            .any(|r| r.key == "paste-tool" && r.ok == Some(false)));
        let ffmpeg = rows.iter().find(|r| r.key == "ffmpeg").unwrap();
        assert!(!ffmpeg.detail.contains("brew"));
    }
}
