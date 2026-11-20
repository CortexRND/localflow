//! Native setup wizard (port of `localflow/setup_mac.py`).
//!
//! The shell and all seven pages are real AppKit controls. Machine-dependent
//! work is injected: checks and config application use the existing
//! `setup_flow` implementations, while speech downloads, dictation trials and
//! microphone metering require explicit platform services. The default model
//! service and an absent microphone service return honest errors; they never
//! make an unavailable device, download or trial look successful.
//!
//! [`SetupWindow`] owns its AppKit delegates and action targets. They and every
//! callback hold only `Weak` back-references. Worker replies carry operation
//! ids, so a reply from an old recheck, cancelled download or closed window
//! cannot mutate current state. All AppKit access, event draining and meter
//! polling happen on the main run loop. Rebuilds re-read the appearance palette
//! and render from state updated eagerly by controls, preserving every choice.
#![allow(deprecated)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use lf_core::config::Config;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSAlert, NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSButton, NSButtonType, NSControlStateValueOff, NSControlStateValueOn, NSFocusRingType,
    NSPopUpButton, NSProgressIndicator, NSScrollView, NSScrollerStyle, NSTextAlignment,
    NSTextField, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSNotification, NSPoint, NSSize, NSString, NSURL};

use super::actions::{ActionTarget, Timer};
use super::style::{
    color, fit_button, font, hide_from_accessibility, make_pill, measure_text, rect, segmented,
    style_button, style_surface, tokens_for_window, ButtonKind, FlippedView, HalftoneView,
    SurfaceStyle,
};
use super::widgets::{button, label, popup, remove_subviews, LabelStyle};
use crate::setup_flow::{
    apply as apply_choices, available_stt_models, footer_status, footer_status_frame,
    initial_choices, run_checks, setup_footer_layout, visible_ollama_error, Check, FileConfigStore,
    HardwareInfo, HostPlatform, LaunchAgent, SetupChoices, SetupConfigStore, SetupError,
    SetupPlatform, SttModel, UnavailableLaunchAgent, FOOTER_STATUS_MAX_LINES, HOTKEY_OPTIONS,
    LLM_MODELS,
};
use crate::tokens::{
    check_status as token_status, surface_tint, Palette, Weight, ACTION_HEIGHT, CALLOUT_HEIGHT,
    CARD_BORDER_WIDTH, CARD_GAP, CARD_PADDING, CHECK_CARD_HEIGHT, CONTROL_GAP, LINE_HEIGHT_BODY,
    LINE_HEIGHT_HEADING, PAGE_TITLE_HEIGHT, SHADOW_OFFSET, SUN_TEXT, TYPE_BODY, TYPE_CAPTION,
    TYPE_DISPLAY, TYPE_HEADING, WINDOW_MIN_HEIGHT, WINDOW_MIN_WIDTH,
};

/// Window title shared with Python.
pub const SETUP_TITLE: &str = "LocalFlow Setup";
/// Number of wizard pages.
pub const SETUP_STEPS: usize = 7;
/// Main run-loop event and microphone polling interval.
pub const SETUP_TICK_SECONDS: f64 = 0.15;
/// Download stall notice interval (`_stall_timeout`).
pub const DOWNLOAD_STALL_SECONDS: u64 = 30;

pub const STEP_TITLES: [&str; SETUP_STEPS] = [
    "Let's get you talking.",
    "Quick checkup.",
    "Pick your speech model.",
    "Choose your cleanup model.",
    "Mic and hotkey.",
    "Give it a try.",
    "All set.",
];

pub const STEP_SUBTITLES: [&str; SETUP_STEPS] = [
    "Choose a local voice setup that fits your machine.",
    "Let's make sure LocalFlow can hear you and type for you.",
    "Pick the speech model that transcribes your voice.",
    "Optional cleanup keeps your transcript clear and readable.",
    "Choose how LocalFlow listens and responds.",
    "Try a short dictation before you finish.",
    "Review your choices and start dictating.",
];

/// Label of a check's action. Ready checks never expose an action even if the
/// underlying `Check` carries one (the snapshot fixtures deliberately do).
pub fn check_action_label(check: &Check) -> Option<&str> {
    if check.ok == Some(true) || check.fix_label.is_empty() {
        return None;
    }
    Some(match check.key.as_str() {
        "microphone" => "Allow microphone",
        "accessibility" => "Allow accessibility",
        "input-monitoring" => "Allow input monitoring",
        "ffmpeg" => "Install ffmpeg",
        "ollama-installed" => "Get Ollama",
        "ollama-running" => "Start Ollama",
        "microphone-device" => "Check microphone",
        _ => check.fix_label.as_str(),
    })
}

/// `(label, tint token, text token)` for a check row.
pub fn check_status(ok: Option<bool>, pending: bool) -> (&'static str, &'static str, &'static str) {
    let key = if pending {
        "checking"
    } else {
        match ok {
            Some(true) => "ready",
            None => "unknown",
            Some(false) => "needs",
        }
    };
    token_status(key).unwrap_or(("Unknown", "sun", "ink"))
}

/// Original check indices, action/unknown checks first and ready checks last.
/// The stable sort and original indices are important: an action always opens
/// the URL belonging to its own check, not its new visual position.
pub fn ordered_check_indexes(checks: &[Check]) -> Vec<usize> {
    let mut indexes: Vec<usize> = (0..checks.len()).collect();
    indexes.sort_by_key(|&index| checks[index].ok == Some(true));
    indexes
}

fn valid_setup_step(step: usize) -> bool {
    step < SETUP_STEPS
}

/// Read-only state useful to the parent router and native integration tests.
#[derive(Clone, Debug, PartialEq)]
pub struct SetupUiState {
    pub step: usize,
    pub title: String,
    pub saved: bool,
    pub closed: bool,
    pub checks: Option<Vec<Check>>,
    pub check_error: Option<String>,
    pub choices: SetupChoices,
    pub cleanup_level: String,
    pub downloaded: HashMap<String, bool>,
    pub ollama_running: bool,
    pub ollama_models: Vec<String>,
    pub busy: Vec<String>,
    pub status_message: String,
    pub error_message: String,
    pub mic_level: Option<f64>,
}

// --------------------------------------------------------------- services ---

/// Machine checks, run off the main thread.
pub trait ChecksRunner: Send + Sync {
    fn run(&self, config: &Config) -> std::result::Result<Vec<Check>, String>;
}

/// Default checks adapter. `HostPlatform` intentionally reports unknown for
/// devices and permissions unless the parent injects a fuller platform.
pub struct FlowChecksRunner {
    platform: Arc<dyn SetupPlatform + Send + Sync>,
}

impl FlowChecksRunner {
    pub fn new(platform: Arc<dyn SetupPlatform + Send + Sync>) -> Self {
        Self { platform }
    }
}

impl Default for FlowChecksRunner {
    fn default() -> Self {
        Self::new(Arc::new(HostPlatform))
    }
}

impl ChecksRunner for FlowChecksRunner {
    fn run(&self, config: &Config) -> std::result::Result<Vec<Check>, String> {
        Ok(run_checks(config, self.platform.as_ref()))
    }
}

/// Persists choices through the real `setup_flow::apply` implementation.
pub trait ApplyRunner: Send + Sync {
    fn apply(&self, choices: &SetupChoices) -> std::result::Result<PathBuf, String>;
}

pub struct FlowApplyRunner {
    store: Arc<dyn SetupConfigStore + Send + Sync>,
    platform: Arc<dyn SetupPlatform + Send + Sync>,
    launch_agent: Arc<dyn LaunchAgent + Send + Sync>,
}

impl FlowApplyRunner {
    pub fn new(
        store: Arc<dyn SetupConfigStore + Send + Sync>,
        platform: Arc<dyn SetupPlatform + Send + Sync>,
        launch_agent: Arc<dyn LaunchAgent + Send + Sync>,
    ) -> Self {
        Self {
            store,
            platform,
            launch_agent,
        }
    }
}

impl Default for FlowApplyRunner {
    fn default() -> Self {
        // The launch agent is deliberately unavailable, not a no-op. On macOS
        // the parent must inject its real launchd adapter; otherwise apply
        // reports the same explicit error as setup_flow.
        Self::new(
            Arc::new(FileConfigStore::default_location()),
            Arc::new(HostPlatform),
            Arc::new(UnavailableLaunchAgent),
        )
    }
}

impl ApplyRunner for FlowApplyRunner {
    fn apply(&self, choices: &SetupChoices) -> std::result::Result<PathBuf, String> {
        apply_choices(
            choices,
            self.store.as_ref(),
            self.platform.as_ref(),
            self.launch_agent.as_ref(),
        )
        .map_err(|error| error.to_string())
    }
}

pub use crate::setup_backend::{
    DownloadProgress, FlowModelBackend, ProgressCallback, SetupModelBackend,
};

/// Thread-safe live microphone meter. `start` may open audio hardware and is
/// called on a worker. `level` and `stop` may be called on the main thread.
/// Levels are clamped before they reach AppKit; an error is shown, never zeroed
/// into a misleading successful/silent reading.
pub trait MicLevelSource: Send + Sync {
    fn start(
        &self,
        device: Option<&str>,
        cancelled: Arc<AtomicBool>,
    ) -> std::result::Result<(), String>;
    fn level(&self) -> std::result::Result<f64, String>;
    fn stop(&self);
}

/// Main-thread actions the surrounding app can supply without coupling this
/// window to the Hub or the runtime router.
#[derive(Clone, Default)]
pub struct SetupHooks {
    pub open_settings: Option<Rc<dyn Fn()>>,
    pub launch_runtime: Option<Rc<dyn Fn()>>,
}

pub struct SetupServices {
    pub checks: Arc<dyn ChecksRunner>,
    pub apply: Arc<dyn ApplyRunner>,
    pub models: Arc<dyn SetupModelBackend>,
    /// `None` produces a visible microphone-unavailable error when metering is
    /// requested. Snapshot fixtures set snapshot mode and never open audio.
    pub mic: Option<Arc<dyn MicLevelSource>>,
    pub hooks: SetupHooks,
}

impl Default for SetupServices {
    fn default() -> Self {
        Self {
            checks: Arc::new(FlowChecksRunner::default()),
            apply: Arc::new(FlowApplyRunner::default()),
            models: Arc::new(FlowModelBackend::default()),
            mic: None,
            hooks: SetupHooks::default(),
        }
    }
}

/// Immutable initial data. Constructing it through [`SetupContext::detect`]
/// invokes the shared initial-choice and model-availability rules.
pub struct SetupContext {
    pub config: Config,
    pub hardware: HardwareInfo,
    pub models: Vec<SttModel>,
    pub choices: SetupChoices,
    pub input_devices: std::result::Result<Vec<String>, String>,
}

impl SetupContext {
    pub fn detect(
        config: Config,
        hardware: HardwareInfo,
        platform: &dyn SetupPlatform,
        launch_agent: &dyn LaunchAgent,
    ) -> std::result::Result<Self, SetupError> {
        let choices = initial_choices(&config, &hardware, platform, launch_agent)?;
        Ok(Self {
            models: available_stt_models(&hardware, platform),
            input_devices: platform.input_devices(),
            config,
            hardware,
            choices,
        })
    }

    pub fn from_choices(
        config: Config,
        hardware: HardwareInfo,
        models: Vec<SttModel>,
        choices: SetupChoices,
        input_devices: std::result::Result<Vec<String>, String>,
    ) -> Self {
        Self {
            config,
            hardware,
            models,
            choices,
            input_devices,
        }
    }
}

// --------------------------------------------------------------- job state ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobKind {
    Checks,
    SpeechChecks,
    OllamaCheck,
    SpeechDownload,
    CleanupDownload,
    Trial,
    Apply,
    Monitor,
}

impl JobKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Checks => "checks",
            Self::SpeechChecks => "stt-checks",
            Self::OllamaCheck => "ollama-check",
            Self::SpeechDownload => "stt-download",
            Self::CleanupDownload => "llm-pull",
            Self::Trial => "trial",
            Self::Apply => "apply",
            Self::Monitor => "monitor",
        }
    }

    fn is_download(self) -> bool {
        matches!(self, Self::SpeechDownload | Self::CleanupDownload)
    }
}

enum JobOutput {
    Checks(Vec<Check>),
    SpeechDownloads(HashMap<String, bool>),
    Ollama(bool, Vec<String>),
    Downloaded,
    Trial(String),
    Applied(PathBuf),
    MonitorStarted,
}

struct JobEvent {
    kind: JobKind,
    id: u64,
    result: std::result::Result<JobOutput, String>,
}

enum SetupEvent {
    Job(JobEvent),
    Progress {
        kind: JobKind,
        id: u64,
        progress: DownloadProgress,
    },
}

struct State {
    config: Config,
    hardware: HardwareInfo,
    models: Vec<SttModel>,
    input_devices: std::result::Result<Vec<String>, String>,
    choices: SetupChoices,
    stt_id: String,
    llm_id: Option<String>,
    device: String,
    hotkey: String,
    language: Option<String>,
    cleanup_level: String,
    pause_media: bool,
    start_on_login: bool,
    step: usize,
    saved: bool,
    snapshot_mode: bool,
    checks: Option<Vec<Check>>,
    check_error: Option<String>,
    downloaded: HashMap<String, bool>,
    ollama_running: bool,
    ollama_models: Vec<String>,
    ollama_checked: bool,
    /// Last failed Ollama check. Kept apart from `error_message` so it only
    /// shows while the current cleanup selection needs Ollama.
    ollama_error: Option<String>,
    status_message: String,
    error_message: String,
    busy: BTreeSet<&'static str>,
    operations: HashMap<&'static str, u64>,
    next_job: u64,
    cancel: Option<(u64, Arc<AtomicBool>)>,
    last_progress: Option<(Instant, u64)>,
    stalled: bool,
    scroll_y: f64,
    mic_level: Option<f64>,
    /// Device whose stream was started (or failed). Rebuilds don't repeatedly
    /// reopen the same failed/unavailable device; changing device or leaving
    /// this page resets it.
    mic_attempted: Option<String>,
    closed: bool,
}

impl State {
    fn new(context: SetupContext) -> Self {
        let choices = context.choices;
        let device_error = context
            .input_devices
            .as_ref()
            .err()
            .map(|error| format!("Input devices couldn't be listed: {error}"));
        Self {
            config: context.config,
            hardware: context.hardware,
            models: context.models,
            input_devices: context.input_devices,
            stt_id: choices.stt_id.clone(),
            llm_id: choices.llm_id.clone(),
            device: choices.input_device.clone(),
            hotkey: choices.hotkey.clone(),
            language: choices.language.clone(),
            cleanup_level: choices.cleanup_level.clone(),
            pause_media: choices.pause_media_while_dictating,
            start_on_login: choices.start_on_login,
            choices,
            step: 0,
            saved: false,
            snapshot_mode: false,
            checks: None,
            check_error: None,
            downloaded: HashMap::new(),
            ollama_running: false,
            ollama_models: Vec::new(),
            ollama_checked: false,
            ollama_error: None,
            status_message: String::new(),
            error_message: device_error.unwrap_or_default(),
            busy: BTreeSet::new(),
            operations: HashMap::new(),
            next_job: 0,
            cancel: None,
            last_progress: None,
            stalled: false,
            scroll_y: 0.0,
            mic_level: None,
            mic_attempted: None,
            closed: false,
        }
    }

    fn busy_job(&self, kind: JobKind) -> bool {
        self.busy.contains(kind.as_str())
    }

    fn model_busy(&self) -> bool {
        self.busy_job(JobKind::SpeechDownload)
            || self.busy_job(JobKind::CleanupDownload)
            || self.busy_job(JobKind::Trial)
    }

    fn sync_choices(&mut self) {
        self.choices.stt_id = self.stt_id.clone();
        self.choices.llm_id = self.llm_id.clone();
        self.choices.input_device = self.device.clone();
        self.choices.hotkey = self.hotkey.clone();
        self.choices.language = self.language.clone();
        self.choices.cleanup_level = self.cleanup_level.clone();
        self.choices.pause_media_while_dictating = self.pause_media;
        self.choices.start_on_login = self.start_on_login;
    }

    fn current_stt(&self) -> Option<&SttModel> {
        self.models.iter().find(|model| model.id == self.stt_id)
    }
}

struct ActiveMic {
    source: Arc<dyn MicLevelSource>,
}

struct PendingMic {
    source: Arc<dyn MicLevelSource>,
    cancelled: Arc<AtomicBool>,
}

// ------------------------------------------------------------ AppKit glue ---

struct DelegateState {
    setup: Weak<SetupWindow>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateState]
    #[name = "LFSetupWindowDelegate"]
    struct SetupDelegate;

    unsafe impl NSObjectProtocol for SetupDelegate {}

    unsafe impl NSWindowDelegate for SetupDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            if let Some(setup) = self.ivars().setup.upgrade() {
                setup.teardown();
            }
        }

        #[unsafe(method(windowDidEndLiveResize:))]
        fn window_did_end_live_resize(&self, _notification: &NSNotification) {
            if let Some(setup) = self.ivars().setup.upgrade() {
                setup.rebuild();
            }
        }
    }

    impl SetupDelegate {
        // AppKit sends this optional notification although objc2 does not list
        // it in NSWindowDelegate.
        #[unsafe(method(windowDidChangeEffectiveAppearance:))]
        fn appearance_changed(&self, _notification: &NSNotification) {
            if let Some(setup) = self.ivars().setup.upgrade() {
                setup.rebuild();
            }
        }
    }
);

impl SetupDelegate {
    fn new(mtm: MainThreadMarker, setup: Weak<SetupWindow>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(DelegateState { setup });
        // SAFETY: plain NSObject initialization on the main thread.
        unsafe { msg_send![super(this), init] }
    }
}

/// Sets the busy flag back even if a render unexpectedly unwinds.
struct RenderGuard<'a>(&'a Cell<bool>);

impl Drop for RenderGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

fn set_layer_background(view: &NSView, token: &str, palette: &Palette) {
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        layer.setBackgroundColor(Some(&color(token, palette).CGColor()));
    }
}

fn accessible_button(button: &NSButton, name: &str) {
    button.setAccessibilityLabel(Some(&NSString::from_str(name)));
}

fn callout_title(error: bool, download_feedback: bool) -> &'static str {
    match (download_feedback, error) {
        (true, true) => "Download error",
        (true, false) => "Download progress",
        (false, true) => "Setup error",
        (false, false) => "Progress",
    }
}

/// Python's `{size_mb:,}` formatting for model sizes.
fn format_model_size_mb(size_mb: u32) -> String {
    let digits = size_mb.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len().saturating_sub(1) / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

// -------------------------------------------------------------- setup view ---

/// One native setup/checkup window. Construct with [`SetupWindow::new`], retain
/// the returned `Rc` in the parent router, and call [`SetupWindow::show`].
/// Closing through either AppKit or [`SetupWindow::close`] is idempotent.
pub struct SetupWindow {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    root: Retained<FlippedView>,
    page: Retained<FlippedView>,
    brand: Retained<NSTextField>,
    step_label: Retained<NSTextField>,
    title: Retained<NSTextField>,
    subtitle: Retained<NSTextField>,
    status: Retained<NSTextField>,
    progress: Vec<Retained<NSView>>,
    mark: Retained<NSView>,
    divider: Retained<NSView>,
    back_button: Retained<NSButton>,
    primary_button: Retained<NSButton>,
    cancel_button: Retained<NSButton>,
    page_target: Retained<ActionTarget>,
    shell_target: Retained<ActionTarget>,
    delegate: Retained<SetupDelegate>,
    services: Rc<SetupServices>,
    state: RefCell<State>,
    palette: RefCell<Palette>,
    sender: mpsc::Sender<SetupEvent>,
    receiver: RefCell<mpsc::Receiver<SetupEvent>>,
    active_mic: RefCell<Option<ActiveMic>>,
    /// Source whose asynchronous `start` has not replied yet. Keeping it here
    /// lets close/device-change stop a stream even if opening finishes late.
    pending_mic: RefCell<Option<PendingMic>>,
    meter: RefCell<Option<Retained<NSProgressIndicator>>>,
    page_scroll: RefCell<Option<Retained<NSScrollView>>>,
    ticker: RefCell<Option<Timer>>,
    deferred: RefCell<Vec<Timer>>,
    rendering: Cell<bool>,
    me: Weak<SetupWindow>,
}

impl SetupWindow {
    /// Build the window without ordering it front. No worker starts until the
    /// first rendered page requires it; creating this on a background thread
    /// is prevented at compile time by `MainThreadMarker`.
    pub fn new(mtm: MainThreadMarker, context: SetupContext, services: SetupServices) -> Rc<Self> {
        Rc::new_cyclic(|me: &Weak<SetupWindow>| {
            Self::build_window(mtm, me.clone(), context, services)
        })
        .initialize()
    }

    fn initialize(self: Rc<Self>) -> Rc<Self> {
        let weak = Rc::downgrade(&self);
        *self.ticker.borrow_mut() = Some(Timer::schedule(SETUP_TICK_SECONDS, true, move || {
            if let Some(setup) = weak.upgrade() {
                setup.pump();
            }
        }));
        self.rebuild();
        self
    }

    fn build_window(
        mtm: MainThreadMarker,
        me: Weak<SetupWindow>,
        context: SetupContext,
        services: SetupServices,
    ) -> Self {
        let width = f64::from(WINDOW_MIN_WIDTH);
        let height = f64::from(WINDOW_MIN_HEIGHT);
        let window_style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // SAFETY: plain AppKit window initialization on the main thread.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc::<NSWindow>(),
                rect(0.0, 0.0, width, height),
                window_style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: retained by this controller; AppKit must not release it too.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(SETUP_TITLE));
        window.setContentMinSize(NSSize::new(width, height));
        window.center();
        let palette = tokens_for_window(&window);
        let root = FlippedView::new(mtm, rect(0.0, 0.0, width, height));
        set_layer_background(&root, "paper", &palette);
        let root_view: &NSView = &root;
        window.setContentView(Some(root_view));

        let brand = Self::label(
            mtm,
            "localflow",
            56.0,
            22.0,
            180.0,
            24.0,
            LabelStyle {
                size: 17.0,
                weight: Weight::Bold,
                ..LabelStyle::default()
            },
            &palette,
        );
        let step_label = Self::label(
            mtm,
            "",
            width - 140.0,
            27.0,
            108.0,
            16.0,
            LabelStyle {
                size: 11.0,
                weight: Weight::Bold,
                height: 16.0,
                ..LabelStyle::default()
            },
            &palette,
        );
        step_label.setAlignment(NSTextAlignment::Right);
        step_label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        root.addSubview(&brand);
        let mut progress = Vec::with_capacity(SETUP_STEPS);
        for index in 0..SETUP_STEPS {
            let segment = NSView::initWithFrame(
                mtm.alloc::<NSView>(),
                rect(32.0 + index as f64 * 106.0, 62.0, 102.0, 4.0),
            );
            hide_from_accessibility(&segment);
            root.addSubview(&segment);
            progress.push(segment);
        }
        let mark = NSView::initWithFrame(mtm.alloc::<NSView>(), rect(32.0, 25.0, 14.0, 14.0));
        set_layer_background(&mark, "sun", &palette);
        if let Some(layer) = mark.layer() {
            layer.setCornerRadius(7.0);
        }
        hide_from_accessibility(&mark);
        root.addSubview(&mark);

        let title = Self::label(
            mtm,
            "",
            32.0,
            86.0,
            width - 64.0,
            f64::from(PAGE_TITLE_HEIGHT),
            LabelStyle {
                size: TYPE_DISPLAY.size,
                weight: TYPE_DISPLAY.weight,
                height: f64::from(PAGE_TITLE_HEIGHT),
                ..LabelStyle::default()
            },
            &palette,
        );
        title.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let subtitle = Self::label(
            mtm,
            "",
            32.0,
            130.0,
            width - 64.0,
            24.0,
            LabelStyle {
                size: 13.0,
                color: "muted",
                height: 24.0,
                wrap: true,
                ..LabelStyle::default()
            },
            &palette,
        );
        subtitle.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let page = FlippedView::new(mtm, rect(32.0, 174.0, width - 64.0, 456.0));
        page.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        root.addSubview(&title);
        root.addSubview(&subtitle);
        root.addSubview(&page);

        let divider = NSView::initWithFrame(
            mtm.alloc::<NSView>(),
            rect(32.0, height - 58.0, width - 64.0, 1.0),
        );
        set_layer_background(&divider, "line", &palette);
        divider.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        hide_from_accessibility(&divider);
        root.addSubview(&divider);

        let shell_target = ActionTarget::new(mtm);
        let make_shell_button = |title: &str,
                                 x: f64,
                                 y: f64,
                                 width: f64,
                                 kind: ButtonKind,
                                 action: fn(&SetupWindow)| {
            let button = button(mtm, title, x, y, width, 34.0, kind, &palette);
            accessible_button(&button, title);
            let weak = me.clone();
            shell_target.on_action(&button, move || {
                if let Some(setup) = weak.upgrade() {
                    setup.defer(action);
                }
            });
            root.addSubview(&button);
            button
        };
        let back_button = make_shell_button(
            "Back",
            32.0,
            height - 42.0,
            100.0,
            ButtonKind::Secondary,
            Self::back,
        );
        back_button.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        let status = Self::label(
            mtm,
            "",
            152.0,
            height - 41.0,
            width - 350.0,
            20.0,
            LabelStyle {
                size: 11.0,
                color: "muted",
                height: 20.0,
                wrap: true,
                ..LabelStyle::default()
            },
            &palette,
        );
        // Long errors wrap; `layout_footer` sizes the frame for these lines
        // and the tooltip keeps the full text.
        status.setMaximumNumberOfLines(FOOTER_STATUS_MAX_LINES as isize);
        status.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        root.addSubview(&status);
        let primary_button = make_shell_button(
            "Get started",
            width - 164.0,
            height - 46.0,
            132.0,
            ButtonKind::Primary,
            Self::primary,
        );
        primary_button.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        let cancel_button = make_shell_button(
            "Cancel",
            width - 280.0,
            height - 46.0,
            100.0,
            ButtonKind::Secondary,
            Self::cancel_download,
        );
        cancel_button.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        let delegate = SetupDelegate::new(mtm, me.clone());
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        let (sender, receiver) = mpsc::channel();

        Self {
            mtm,
            window,
            root,
            page,
            brand,
            step_label,
            title,
            subtitle,
            status,
            progress,
            mark,
            divider,
            back_button,
            primary_button,
            cancel_button,
            page_target: ActionTarget::new(mtm),
            shell_target,
            delegate,
            services: Rc::new(services),
            state: RefCell::new(State::new(context)),
            palette: RefCell::new(palette),
            sender,
            receiver: RefCell::new(receiver),
            active_mic: RefCell::new(None),
            pending_mic: RefCell::new(None),
            meter: RefCell::new(None),
            page_scroll: RefCell::new(None),
            ticker: RefCell::new(None),
            deferred: RefCell::new(Vec::new()),
            rendering: Cell::new(false),
            me,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn label(
        mtm: MainThreadMarker,
        text: &str,
        x: f64,
        y: f64,
        width: f64,
        _height: f64,
        style: LabelStyle,
        palette: &Palette,
    ) -> Retained<NSTextField> {
        label(mtm, text, x, y, width, style, palette)
    }

    pub fn window(&self) -> &NSWindow {
        &self.window
    }

    pub fn content_view(&self) -> Retained<NSView> {
        Retained::into_super(self.root.clone())
    }

    pub fn delegate_object(&self) -> &NSObject {
        &self.delegate
    }

    pub fn current_step(&self) -> usize {
        self.state.borrow().step
    }

    pub fn is_closed(&self) -> bool {
        self.state.borrow().closed
    }

    pub fn palette(&self) -> Palette {
        *self.palette.borrow()
    }

    pub fn ui_state(&self) -> SetupUiState {
        let mut state = self.state.borrow_mut();
        state.sync_choices();
        SetupUiState {
            step: state.step,
            title: STEP_TITLES[state.step].to_string(),
            saved: state.saved,
            closed: state.closed,
            checks: state.checks.clone(),
            check_error: state.check_error.clone(),
            choices: state.choices.clone(),
            cleanup_level: state.cleanup_level.clone(),
            downloaded: state.downloaded.clone(),
            ollama_running: state.ollama_running,
            ollama_models: state.ollama_models.clone(),
            busy: state.busy.iter().map(|name| (*name).to_string()).collect(),
            status_message: state.status_message.clone(),
            error_message: state.error_message.clone(),
            mic_level: state.mic_level,
        }
    }

    /// Bring this existing setup forward; do not create another window.
    pub fn show(&self) {
        if self.is_closed() {
            return;
        }
        self.window.deminiaturize(None);
        self.window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(self.mtm).activateIgnoringOtherApps(true);
    }

    /// Resize content, respecting the AppKit minimum, and immediately relayout
    /// the current step. Used by the wide snapshot and by router integrations.
    pub fn resize_content(&self, width: f64, height: f64) {
        self.window.setContentSize(NSSize::new(
            width.max(f64::from(WINDOW_MIN_WIDTH)),
            height.max(f64::from(WINDOW_MIN_HEIGHT)),
        ));
        self.rebuild();
    }

    fn defer(&self, work: impl Fn(&SetupWindow) + 'static) {
        let weak = self.me.clone();
        let mut timers = self.deferred.borrow_mut();
        timers.retain(Timer::is_valid);
        timers.push(Timer::schedule(0.0, false, move || {
            if let Some(setup) = weak.upgrade() {
                work(&setup);
            }
        }));
    }

    /// Full relayout with palette refresh. Control callbacks already committed
    /// their values into state, including while a popup/switch is edited.
    pub fn rebuild(&self) {
        if self.is_closed() {
            return;
        }
        if self.rendering.replace(true) {
            self.defer(Self::rebuild);
            return;
        }
        let _guard = RenderGuard(&self.rendering);
        if let Some(scroll) = self.page_scroll.borrow().as_ref() {
            self.state.borrow_mut().scroll_y = scroll.contentView().bounds().origin.y;
        }
        let palette = tokens_for_window(&self.window);
        *self.palette.borrow_mut() = palette;
        self.restyle_shell(palette);
        self.page_target.clear();
        remove_subviews(&self.page);
        *self.meter.borrow_mut() = None;
        *self.page_scroll.borrow_mut() = None;

        let step = self.state.borrow().step;
        match step {
            0 => self.render_welcome(),
            1 => self.render_checks(),
            2 => self.render_speech(),
            3 => self.render_cleanup(),
            4 => self.render_mic(),
            5 => self.render_trial(),
            6 => self.render_finish(),
            _ => unreachable!("setup step is kept in range"),
        }
        self.render_footer();
        if let Some(scroll) = self.page_scroll.borrow().as_ref() {
            let y = self.state.borrow().scroll_y;
            let max_y = (scroll
                .documentView()
                .map_or(0.0, |document| document.frame().size.height)
                - scroll.contentView().bounds().size.height)
                .max(0.0);
            if let Some(document) = scroll.documentView() {
                document.scrollPoint(NSPoint::new(0.0, y.min(max_y)));
            }
        }
        if step == 4 && !self.state.borrow().snapshot_mode {
            self.start_monitor();
        }
    }

    fn restyle_shell(&self, palette: Palette) {
        set_layer_background(&self.root, "paper", &palette);
        set_layer_background(&self.mark, "sun", &palette);
        set_layer_background(&self.divider, "line", &palette);
        for (label, token) in [
            (&self.brand, "ink"),
            (&self.step_label, "ink"),
            (&self.title, "ink"),
            (&self.subtitle, "muted"),
        ] {
            label.setTextColor(Some(&color(token, &palette)));
        }
        style_button(&self.back_button, ButtonKind::Secondary, &palette);
        style_button(&self.primary_button, ButtonKind::Primary, &palette);
        style_button(&self.cancel_button, ButtonKind::Secondary, &palette);
        let state = self.state.borrow();
        self.title
            .setStringValue(&NSString::from_str(STEP_TITLES[state.step]));
        self.subtitle
            .setStringValue(&NSString::from_str(STEP_SUBTITLES[state.step]));
        self.step_label.setStringValue(&NSString::from_str(&format!(
            "STEP {} OF {SETUP_STEPS}",
            state.step + 1
        )));
        for (index, segment) in self.progress.iter().enumerate() {
            set_layer_background(
                segment,
                if index <= state.step {
                    "cobalt"
                } else {
                    "line"
                },
                &palette,
            );
        }
    }

    fn add_text(&self, text: &str, cursor: &mut f64, size: f64, bold: bool, height: f64) {
        let palette = *self.palette.borrow();
        let width = self.page.bounds().size.width;
        self.page.addSubview(&label(
            self.mtm,
            text,
            8.0,
            *cursor,
            (width - 26.0).max(0.0),
            LabelStyle {
                size,
                color: if bold { "ink" } else { "muted" },
                height,
                weight: if bold { Weight::Bold } else { Weight::Regular },
                wrap: true,
            },
            &palette,
        ));
        *cursor += height + 12.0;
    }

    #[allow(clippy::too_many_arguments)]
    fn page_button(
        &self,
        parent: &NSView,
        title: &str,
        x: f64,
        y: f64,
        width: f64,
        kind: ButtonKind,
        accessibility: &str,
        action: impl Fn(&SetupWindow) + 'static,
    ) -> Retained<NSButton> {
        let palette = *self.palette.borrow();
        let button = button(
            self.mtm,
            title,
            x,
            y,
            width,
            f64::from(ACTION_HEIGHT),
            kind,
            &palette,
        );
        accessible_button(&button, accessibility);
        let weak = self.me.clone();
        self.page_target.on_action(&button, move || {
            if let Some(setup) = weak.upgrade() {
                action(&setup);
            }
        });
        parent.addSubview(&button);
        button
    }

    fn add_callout(&self, cursor: &mut f64, error: bool, download_feedback: bool) {
        let palette = *self.palette.borrow();
        let state = self.state.borrow();
        let message = if error {
            state.error_message.clone()
        } else {
            state.status_message.clone()
        };
        drop(state);
        if message.is_empty() {
            return;
        }
        let width = (self.page.bounds().size.width - 16.0).max(0.0);
        let card = FlippedView::new(self.mtm, rect(8.0, *cursor, width, 58.0));
        style_surface(&card, &palette, &SurfaceStyle::default());
        let bar = NSView::initWithFrame(self.mtm.alloc::<NSView>(), rect(0.0, 0.0, 4.0, 58.0));
        set_layer_background(&bar, if error { "red" } else { "cobalt" }, &palette);
        hide_from_accessibility(&bar);
        card.addSubview(&bar);
        card.addSubview(&label(
            self.mtm,
            callout_title(error, download_feedback),
            16.0,
            8.0,
            width - 40.0,
            LabelStyle {
                size: 13.0,
                height: 18.0,
                weight: Weight::Bold,
                ..LabelStyle::default()
            },
            &palette,
        ));
        card.addSubview(&label(
            self.mtm,
            &message,
            16.0,
            30.0,
            width - 40.0,
            LabelStyle {
                size: 11.0,
                color: "muted",
                height: 18.0,
                ..LabelStyle::default()
            },
            &palette,
        ));
        self.page.addSubview(&card);
        *cursor += 70.0;
    }

    fn make_scroll(
        &self,
        height: f64,
        y: f64,
        frame_height: Option<f64>,
        reserve: f64,
        x: f64,
    ) -> Retained<FlippedView> {
        let page_size = self.page.bounds().size;
        let frame_height = frame_height.unwrap_or((page_size.height - y - reserve).max(0.0));
        let scroll = NSScrollView::initWithFrame(
            self.mtm.alloc::<NSScrollView>(),
            rect(x, y, (page_size.width - 2.0 * x).max(0.0), frame_height),
        );
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(false);
        scroll.setScrollerStyle(NSScrollerStyle::Legacy);
        scroll.setBorderType(objc2_app_kit::NSBorderType::NoBorder);
        scroll.setDrawsBackground(false);
        let content_width = scroll.contentSize().width;
        let document = FlippedView::new(
            self.mtm,
            rect(0.0, 0.0, content_width, height.max(frame_height)),
        );
        document.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let document_view: &NSView = &document;
        scroll.setDocumentView(Some(document_view));
        self.page.addSubview(&scroll);
        scroll.tile();
        scroll.reflectScrolledClipView(&scroll.contentView());
        *self.page_scroll.borrow_mut() = Some(scroll);
        document
    }

    // ------------------------------------------------------------- pages ---

    fn render_welcome(&self) {
        let palette = *self.palette.borrow();
        let halftone = HalftoneView::new(self.mtm, rect(548.0, 248.0, 180.0, 160.0), palette);
        halftone.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        hide_from_accessibility(&halftone);
        self.page.addSubview(&halftone);
        let state = self.state.borrow();
        // detect() used the injected runtime/platform to make these choices;
        // never re-recommend with HostPlatform (which intentionally knows no
        // speech runtimes). The speech page tags and preselects the same model.
        let stt = self.recommended_stt();
        let llm = state.choices.llm_id.clone();
        let mut cursor = 8.0;
        self.add_text(
            "A local voice setup, ready in a few minutes.",
            &mut cursor,
            20.0,
            true,
            34.0,
        );
        self.add_text(
            &format!(
                "{:.0} GB memory  ·  {:.0} GB free disk  ·  {}",
                state.hardware.ram_gb,
                state.hardware.free_disk_gb,
                if state.hardware.apple_silicon {
                    "Apple Silicon"
                } else {
                    "CPU"
                }
            ),
            &mut cursor,
            13.0,
            false,
            26.0,
        );
        self.add_text(
            &format!(
                "Suggested speech model: {stt}  ·  Cleanup: {}",
                llm.as_deref().unwrap_or("off")
            ),
            &mut cursor,
            13.0,
            false,
            26.0,
        );
        self.add_text(
            "Your audio and models stay on this machine.",
            &mut cursor,
            13.0,
            false,
            26.0,
        );
    }

    fn render_checks(&self) {
        let palette = *self.palette.borrow();
        let page_width = self.page.bounds().size.width;
        let recheck = self.page_button(
            &self.page,
            "Recheck",
            0.0,
            0.0,
            0.0,
            ButtonKind::Secondary,
            "Recheck this Mac",
            |setup| setup.recheck(),
        );
        fit_button(&recheck, 0.0, 0.0);
        let recheck_width = recheck.frame().size.width;
        recheck.setFrameOrigin(NSPoint::new(page_width - recheck_width, 0.0));
        recheck.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        let intro = label(
            self.mtm,
            "Nothing here blocks setup. Fix what you can; revisit any time.",
            0.0,
            0.0,
            (page_width - recheck_width - f64::from(CARD_GAP)).max(0.0),
            LabelStyle {
                size: TYPE_CAPTION.size,
                color: "muted",
                height: f64::from(ACTION_HEIGHT),
                wrap: true,
                ..LabelStyle::default()
            },
            &palette,
        );
        intro.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        self.page.addSubview(&intro);
        let y = f64::from(ACTION_HEIGHT + CARD_GAP);
        let (checks, error, snapshot) = {
            let state = self.state.borrow();
            (
                state.checks.clone(),
                state.check_error.clone(),
                state.snapshot_mode,
            )
        };
        let Some(checks) = checks else {
            self.page.addSubview(&label(
                self.mtm,
                "Checking this machine…",
                0.0,
                y,
                page_width,
                LabelStyle {
                    size: TYPE_BODY.size,
                    color: "muted",
                    height: f64::from(CALLOUT_HEIGHT),
                    ..LabelStyle::default()
                },
                &palette,
            ));
            if !snapshot && !self.state.borrow().busy_job(JobKind::Checks) {
                self.defer(|setup| setup.start_checks());
            }
            return;
        };
        let mut callout_y = y;
        if let Some(error) = error {
            let message = format!("Checkup couldn't finish: {error}");
            let card = FlippedView::new(
                self.mtm,
                rect(0.0, callout_y, page_width - f64::from(SHADOW_OFFSET), 42.0),
            );
            style_surface(
                &card,
                &palette,
                &SurfaceStyle {
                    border: "red",
                    ..SurfaceStyle::default()
                },
            );
            card.addSubview(&label(
                self.mtm,
                &message,
                f64::from(CARD_PADDING),
                8.0,
                page_width - 2.0 * f64::from(CARD_PADDING + SHADOW_OFFSET),
                LabelStyle {
                    size: 12.0,
                    color: "red",
                    height: 26.0,
                    wrap: true,
                    ..LabelStyle::default()
                },
                &palette,
            ));
            self.page.addSubview(&card);
            callout_y += 54.0;
        } else if checks.is_empty() {
            self.page.addSubview(&label(
                self.mtm,
                "No checks were returned. Recheck to inspect this Mac again.",
                0.0,
                callout_y,
                page_width,
                LabelStyle {
                    size: TYPE_BODY.size,
                    color: "muted",
                    height: f64::from(CALLOUT_HEIGHT),
                    wrap: true,
                    ..LabelStyle::default()
                },
                &palette,
            ));
            return;
        } else if checks.iter().all(|check| check.ok == Some(true)) {
            let card = FlippedView::new(
                self.mtm,
                rect(
                    0.0,
                    callout_y,
                    page_width - f64::from(SHADOW_OFFSET),
                    f64::from(CALLOUT_HEIGHT),
                ),
            );
            style_surface(
                &card,
                &palette,
                &SurfaceStyle {
                    fill: "sun",
                    border: "ink",
                    width: Some(f64::from(CARD_BORDER_WIDTH)),
                    ..SurfaceStyle::default()
                },
            );
            card.addSubview(&label(
                self.mtm,
                "All set — nothing to fix.",
                f64::from(CARD_PADDING),
                0.0,
                page_width - f64::from(2 * CARD_PADDING + SHADOW_OFFSET),
                LabelStyle {
                    size: TYPE_BODY.size,
                    color: SUN_TEXT,
                    height: f64::from(CALLOUT_HEIGHT),
                    weight: Weight::Bold,
                    ..LabelStyle::default()
                },
                &palette,
            ));
            self.page.addSubview(&card);
            callout_y += f64::from(CALLOUT_HEIGHT + CARD_GAP);
        }
        let order = ordered_check_indexes(&checks);
        let step = f64::from(CHECK_CARD_HEIGHT + CARD_GAP);
        let document = self.make_scroll(
            order.len() as f64 * step + f64::from(SHADOW_OFFSET),
            callout_y,
            None,
            0.0,
            0.0,
        );
        for (position, index) in order.into_iter().enumerate() {
            self.check_card(&document, &checks[index], index, position as f64 * step);
        }
    }

    fn check_card(&self, document: &FlippedView, check: &Check, index: usize, y: f64) {
        let palette = *self.palette.borrow();
        let card_width = document.bounds().size.width - f64::from(SHADOW_OFFSET);
        let card = FlippedView::new(
            self.mtm,
            rect(0.0, y, card_width, f64::from(CHECK_CARD_HEIGHT)),
        );
        card.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        style_surface(
            &card,
            &palette,
            &SurfaceStyle {
                border: "ink",
                shadow: true,
                width: Some(f64::from(CARD_BORDER_WIDTH)),
                ..SurfaceStyle::default()
            },
        );
        let (text, tint, text_token) = check_status(check.ok, false);
        let pill = make_pill(self.mtm, text, tint, text_token, &palette);
        let pill_width = pill.frame().size.width;
        pill.setFrameOrigin(NSPoint::new(
            card_width - f64::from(CARD_PADDING) - pill_width,
            (f64::from(CHECK_CARD_HEIGHT) - pill.frame().size.height) / 2.0,
        ));
        pill.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        let mut controls_left = pill.frame().origin.x;
        if let Some(action_label) = check_action_label(check) {
            let action = self.page_button(
                &card,
                action_label,
                0.0,
                0.0,
                0.0,
                ButtonKind::Secondary,
                &format!("{action_label}: {}", check.label),
                move |setup| setup.fix_check(index),
            );
            action.setTag(index as isize);
            fit_button(&action, 0.0, 0.0);
            let action_width = action.frame().size.width;
            action.setFrameOrigin(NSPoint::new(
                controls_left - f64::from(CONTROL_GAP) - action_width,
                (f64::from(CHECK_CARD_HEIGHT - ACTION_HEIGHT)) / 2.0,
            ));
            action.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            controls_left = action.frame().origin.x;
        }
        let text_width = (controls_left - f64::from(CARD_GAP + CARD_PADDING)).max(20.0);
        card.addSubview(&label(
            self.mtm,
            &check.label,
            f64::from(CARD_PADDING),
            f64::from(CARD_PADDING),
            text_width,
            LabelStyle {
                size: TYPE_HEADING.size,
                height: f64::from(LINE_HEIGHT_HEADING),
                weight: Weight::Bold,
                ..LabelStyle::default()
            },
            &palette,
        ));
        let detail = label(
            self.mtm,
            &check.detail,
            f64::from(CARD_PADDING),
            f64::from(CARD_PADDING + LINE_HEIGHT_HEADING),
            text_width,
            LabelStyle {
                size: TYPE_BODY.size,
                color: "muted",
                height: f64::from(LINE_HEIGHT_BODY),
                ..LabelStyle::default()
            },
            &palette,
        );
        detail.setToolTip(Some(&NSString::from_str(&check.detail)));
        card.addSubview(&detail);
        card.addSubview(&pill);
        document.addSubview(&card);
    }

    fn render_speech(&self) {
        let mut cursor = 8.0;
        self.add_text(
            "Models stay on this machine. Size is shown before download.",
            &mut cursor,
            11.0,
            false,
            18.0,
        );
        let (has_message, error, download_feedback) = {
            let state = self.state.borrow();
            (
                !state.status_message.is_empty() || !state.error_message.is_empty(),
                !state.error_message.is_empty(),
                state.busy_job(JobKind::SpeechDownload) || !state.status_message.is_empty(),
            )
        };
        if has_message {
            self.add_callout(&mut cursor, error, download_feedback);
        }
        let (models, selected, downloaded, snapshot, checking) = {
            let state = self.state.borrow();
            (
                state.models.clone(),
                state.stt_id.clone(),
                state.downloaded.clone(),
                state.snapshot_mode,
                state.busy_job(JobKind::SpeechChecks),
            )
        };
        let recommended = self.recommended_stt();
        let document = self.make_scroll(
            models.len() as f64 * 84.0 + f64::from(SHADOW_OFFSET),
            cursor,
            None,
            0.0,
            8.0,
        );
        if models.is_empty() {
            document.addSubview(&label(
                self.mtm,
                "No speech runtime is available. Install a supported runtime, then reopen setup.",
                0.0,
                16.0,
                document.bounds().size.width,
                LabelStyle {
                    size: 13.0,
                    color: "red",
                    height: 42.0,
                    wrap: true,
                    ..LabelStyle::default()
                },
                &self.palette.borrow(),
            ));
        }
        for (index, model) in models.iter().enumerate() {
            let mut tags = Vec::new();
            if let Some(format) = self.services.models.speech_format(model) {
                tags.push(format);
            }
            if model.id == recommended {
                tags.push("Recommended");
            }
            if downloaded.get(&model.id).copied().unwrap_or(false) {
                tags.push("Downloaded");
            }
            if model.english_only {
                tags.push("English only");
            }
            let size = if let Some(detail) = self.services.models.speech_detail(model) {
                detail
            } else if model.size_mb >= 1000 {
                format!("{:.1} GB", f64::from(model.size_mb) / 1000.0)
            } else {
                format!("{} MB", format_model_size_mb(model.size_mb))
            };
            self.model_card(
                &document,
                model.label,
                &size,
                index as f64 * 84.0,
                selected == model.id,
                &format!("Speech model: {}", model.label),
                &tags,
                Some((model.speed, model.accuracy)),
                move |setup| setup.select_stt(index),
            );
        }
        if !snapshot && downloaded.is_empty() && !checking {
            self.defer(|setup| setup.start_speech_checks());
        }
    }

    fn recommended_stt(&self) -> String {
        // Recommend using the initial choice if it is still in the available
        // catalog: detect() already applied runtime and hardware availability.
        let state = self.state.borrow();
        if state
            .models
            .iter()
            .any(|model| model.id == state.choices.stt_id)
        {
            return state.choices.stt_id.clone();
        }
        state
            .models
            .first()
            .map(|model| model.id.clone())
            .unwrap_or_default()
    }

    fn render_cleanup(&self) {
        let mut cursor = 8.0;
        self.cleanup_level_control(&mut cursor);
        let provider = self.state.borrow().choices.llm_provider.clone();
        if provider != "ollama" {
            self.add_text(
                &format!(
                    "Configured provider: {provider}. Manage models with this provider in Settings."
                ),
                &mut cursor,
                13.0,
                false,
                26.0,
            );
            self.page_button(
                &self.page,
                "Change in Settings",
                8.0,
                cursor,
                190.0,
                ButtonKind::Secondary,
                "Change cleanup provider in Settings",
                |setup| setup.open_settings(),
            );
            return;
        }
        let (selected, installed, checked, running, snapshot) = {
            let state = self.state.borrow();
            (
                state.llm_id.clone(),
                state.ollama_models.clone(),
                state.ollama_checked,
                state.ollama_running,
                state.snapshot_mode,
            )
        };
        let mut options = vec![(
            None,
            "No cleanup (raw transcript)".to_string(),
            "Fastest; no Ollama required.".to_string(),
        )];
        options.extend(LLM_MODELS.iter().map(|model| {
            (
                Some(model.id.to_string()),
                model.label.to_string(),
                format!(
                    "{} MB  ·  {} GB RAM recommended",
                    format_model_size_mb(model.size_mb),
                    model.min_ram_gb
                ),
            )
        }));
        let document = self.make_scroll(
            options.len() as f64 * 84.0 + f64::from(SHADOW_OFFSET),
            cursor,
            None,
            f64::from(ACTION_HEIGHT + CARD_GAP),
            8.0,
        );
        for (index, (id, title, detail)) in options.iter().enumerate() {
            let tags: Vec<&str> = if id.as_ref().is_some_and(|id| installed.contains(id)) {
                vec!["Downloaded"]
            } else {
                Vec::new()
            };
            self.model_card(
                &document,
                title,
                detail,
                index as f64 * 84.0,
                selected == *id,
                &format!("Cleanup model: {title}"),
                &tags,
                None,
                move |setup| setup.select_llm(index),
            );
        }
        if !snapshot && !checked && !self.state.borrow().busy_job(JobKind::OllamaCheck) {
            self.defer(|setup| setup.start_ollama_check());
        }
        let page_size = self.page.bounds().size;
        let y = page_size.height - f64::from(ACTION_HEIGHT);
        let ollama_installed = self
            .state
            .borrow()
            .checks
            .as_ref()
            .and_then(|checks| {
                checks
                    .iter()
                    .find(|check| check.key == "ollama-installed")
                    .and_then(|check| check.ok)
            })
            .unwrap_or(false);
        let title = if running {
            "Refresh models"
        } else if ollama_installed {
            "Recheck Ollama"
        } else {
            "Get Ollama"
        };
        let action = self.page_button(
            &self.page,
            title,
            f64::from(CONTROL_GAP),
            y,
            0.0,
            ButtonKind::Secondary,
            title,
            move |setup| {
                if setup.state.borrow().ollama_running || ollama_installed {
                    setup.recheck_ollama();
                } else {
                    setup.open_url("https://ollama.com/download");
                }
            },
        );
        fit_button(&action, f64::from(CONTROL_GAP), y);
        action.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        if !running {
            let left = f64::from(CONTROL_GAP) + action.frame().size.width + f64::from(CONTROL_GAP);
            let hint = label(
                self.mtm,
                "Ollama is not running. Cleanup is optional.",
                left,
                y,
                (page_size.width - left - f64::from(CONTROL_GAP)).max(0.0),
                LabelStyle {
                    size: TYPE_CAPTION.size,
                    color: "muted",
                    height: f64::from(ACTION_HEIGHT),
                    ..LabelStyle::default()
                },
                &self.palette.borrow(),
            );
            hint.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewMinYMargin,
            );
            self.page.addSubview(&hint);
        }
    }

    fn cleanup_level_control(&self, cursor: &mut f64) {
        if self.state.borrow().llm_id.is_none() {
            return;
        }
        self.add_text("Cleanup level", cursor, 13.0, true, 20.0);
        let palette = *self.palette.borrow();
        let group = FlippedView::new(self.mtm, rect(8.0, *cursor, 204.0, 36.0));
        let selected_level = self.state.borrow().cleanup_level.clone();
        let mut buttons = Vec::new();
        for (index, (level, title)) in [("light", "Light"), ("medium", "Medium")]
            .into_iter()
            .enumerate()
        {
            let button = self.page_button(
                &group,
                title,
                index as f64 * 102.0,
                1.0,
                102.0,
                if selected_level == level {
                    ButtonKind::Selected
                } else {
                    ButtonKind::Secondary
                },
                &format!("Cleanup level: {title}"),
                move |setup| setup.select_cleanup_level(level),
            );
            button.setTag(index as isize);
            buttons.push(button);
        }
        segmented(
            &group,
            &buttons,
            if selected_level == "medium" { 1 } else { 0 },
            &palette,
        );
        self.page.addSubview(&group);
        *cursor += 48.0;
    }

    #[allow(clippy::too_many_arguments)]
    fn model_card(
        &self,
        parent: &FlippedView,
        title: &str,
        detail: &str,
        y: f64,
        selected: bool,
        accessibility: &str,
        tags: &[&str],
        meters: Option<(u8, u8)>,
        action: impl Fn(&SetupWindow) + 'static,
    ) {
        let palette = *self.palette.borrow();
        let card_width = parent.bounds().size.width - 8.0;
        let card = FlippedView::new(self.mtm, rect(0.0, y, card_width, 76.0));
        card.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        style_surface(
            &card,
            &palette,
            &SurfaceStyle {
                border: if selected { "ink" } else { "line" },
                shadow: selected,
                ..SurfaceStyle::default()
            },
        );
        let radio = NSView::initWithFrame(self.mtm.alloc::<NSView>(), rect(17.0, 28.0, 16.0, 16.0));
        radio.setWantsLayer(true);
        if let Some(layer) = radio.layer() {
            layer.setCornerRadius(8.0);
            layer.setBorderWidth(2.0);
            layer.setBorderColor(Some(&color("ink", &palette).CGColor()));
            layer.setBackgroundColor(Some(&color("surface", &palette).CGColor()));
        }
        hide_from_accessibility(&radio);
        if selected {
            let dot = NSView::initWithFrame(self.mtm.alloc::<NSView>(), rect(4.0, 4.0, 8.0, 8.0));
            set_layer_background(&dot, "sun", &palette);
            if let Some(layer) = dot.layer() {
                layer.setCornerRadius(4.0);
            }
            hide_from_accessibility(&dot);
            radio.addSubview(&dot);
        }
        card.addSubview(&radio);
        let title_label = label(
            self.mtm,
            title,
            46.0,
            10.0,
            390.0,
            LabelStyle {
                size: 15.0,
                height: 22.0,
                weight: Weight::Bold,
                ..LabelStyle::default()
            },
            &palette,
        );
        title_label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        card.addSubview(&title_label);
        // Match `_add_card_button`: AppKit's label fitting includes the text
        // cell's metrics that raw NSString measurement omits. Reserve eight
        // points of horizontal inset on each side for every pill.
        let measure = |text: &str| {
            let pill_label = label(
                self.mtm,
                text,
                0.0,
                0.0,
                200.0,
                LabelStyle {
                    size: 10.0,
                    height: 16.0,
                    weight: Weight::Bold,
                    ..LabelStyle::default()
                },
                &palette,
            );
            pill_label.sizeToFit();
            pill_label.frame().size.width + 16.0
        };
        let total_pills: f64 = tags.iter().map(|tag| measure(tag)).sum::<f64>()
            + 4.0 * tags.len().saturating_sub(1) as f64;
        let mut pill_x = card_width - 12.0 - total_pills;
        for tag in tags {
            let pill_width = measure(tag);
            let pill = FlippedView::new(self.mtm, rect(pill_x, 10.0, pill_width, 20.0));
            pill.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            let (fill, text_color) = match *tag {
                "Downloaded" => (
                    surface_tint("success", &palette, 0.12)
                        .unwrap_or_else(|| palette.success.to_string()),
                    "success",
                ),
                "English only" | "GGML" | "ONNX" => (palette.surface.to_string(), "muted"),
                _ => (palette.sun.to_string(), SUN_TEXT),
            };
            pill.setWantsLayer(true);
            if let Some(layer) = pill.layer() {
                layer.setBackgroundColor(Some(&color(&fill, &palette).CGColor()));
                layer.setCornerRadius(10.0);
            }
            pill.addSubview(&label(
                self.mtm,
                tag,
                8.0,
                2.0,
                pill_width - 16.0,
                LabelStyle {
                    size: 10.0,
                    color: text_color,
                    height: 16.0,
                    weight: Weight::Bold,
                    ..LabelStyle::default()
                },
                &palette,
            ));
            card.addSubview(&pill);
            pill_x += pill_width + 4.0;
        }
        if let Some((speed, accuracy)) = meters {
            card.addSubview(&label(
                self.mtm,
                detail,
                46.0,
                43.0,
                70.0,
                LabelStyle {
                    size: 11.0,
                    color: "muted",
                    height: 18.0,
                    ..LabelStyle::default()
                },
                &palette,
            ));
            let mut meter_x = 130.0;
            for (name, score) in [("Speed", speed), ("Accuracy", accuracy)] {
                card.addSubview(&label(
                    self.mtm,
                    name,
                    meter_x,
                    43.0,
                    60.0,
                    LabelStyle {
                        size: 11.0,
                        color: "muted",
                        height: 18.0,
                        ..LabelStyle::default()
                    },
                    &palette,
                ));
                meter_x += 64.0;
                for index in 0..5 {
                    let dot = NSView::initWithFrame(
                        self.mtm.alloc::<NSView>(),
                        rect(meter_x + f64::from(index) * 11.0, 48.0, 7.0, 7.0),
                    );
                    set_layer_background(
                        &dot,
                        if index < i32::from(score) {
                            "ink"
                        } else {
                            "line"
                        },
                        &palette,
                    );
                    if let Some(layer) = dot.layer() {
                        layer.setCornerRadius(3.5);
                    }
                    hide_from_accessibility(&dot);
                    card.addSubview(&dot);
                }
                meter_x += 59.0;
            }
        } else {
            let detail_label = label(
                self.mtm,
                detail,
                46.0,
                43.0,
                card_width - 58.0,
                LabelStyle {
                    size: 11.0,
                    color: "muted",
                    height: 18.0,
                    ..LabelStyle::default()
                },
                &palette,
            );
            detail_label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
            card.addSubview(&detail_label);
        }
        // The overlay is a genuine NSButton: full keyboard/focus/action and an
        // explicit name even though its visible title is empty.
        let click = NSButton::initWithFrame(self.mtm.alloc::<NSButton>(), card.bounds());
        click.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        click.setTitle(&NSString::from_str(""));
        click.setBordered(false);
        click.setFocusRingType(NSFocusRingType::Exterior);
        accessible_button(&click, accessibility);
        let weak = self.me.clone();
        self.page_target.on_action(&click, move || {
            if let Some(setup) = weak.upgrade() {
                action(&setup);
            }
        });
        card.addSubview(&click);
        parent.addSubview(&card);
    }

    fn render_mic(&self) {
        let palette = *self.palette.borrow();
        let page_width = self.page.bounds().size.width;
        let mut cursor = 8.0;
        self.page.addSubview(&label(
            self.mtm,
            "Microphone",
            8.0,
            cursor,
            200.0,
            LabelStyle {
                size: 11.0,
                height: 20.0,
                weight: Weight::Bold,
                ..LabelStyle::default()
            },
            &palette,
        ));
        let (devices, device, hotkey, pause, choices) = {
            let state = self.state.borrow();
            (
                state.input_devices.clone(),
                state.device.clone(),
                state.hotkey.clone(),
                state.pause_media,
                state.choices.clone(),
            )
        };
        let mut titles = vec!["System default".to_string()];
        titles.extend(devices.unwrap_or_default());
        let selected = device
            .is_empty()
            .then_some(0)
            .or_else(|| titles.iter().position(|title| title == &device))
            .unwrap_or(0);
        let devices_popup = popup(
            self.mtm,
            &titles,
            selected,
            8.0,
            cursor + 22.0,
            page_width - 26.0,
            "Microphone",
        );
        devices_popup.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let weak = self.me.clone();
        let sender = devices_popup.clone();
        self.page_target.on_action(&devices_popup, move || {
            if let Some(setup) = weak.upgrade() {
                setup.device_changed(&sender);
            }
        });
        self.page.addSubview(&devices_popup);
        cursor += 66.0;
        self.page.addSubview(&label(
            self.mtm,
            "Hotkey",
            8.0,
            cursor,
            200.0,
            LabelStyle {
                size: 11.0,
                height: 20.0,
                weight: Weight::Bold,
                ..LabelStyle::default()
            },
            &palette,
        ));
        let hotkey_titles: Vec<String> = HOTKEY_OPTIONS
            .iter()
            .map(|(_, title)| (*title).to_string())
            .collect();
        let selected_hotkey = HOTKEY_OPTIONS
            .iter()
            .position(|(value, _)| *value == hotkey)
            .unwrap_or(0);
        let hotkeys = popup(
            self.mtm,
            &hotkey_titles,
            selected_hotkey,
            8.0,
            cursor + 22.0,
            page_width - 26.0,
            "Dictation hotkey",
        );
        hotkeys.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let weak = self.me.clone();
        let sender = hotkeys.clone();
        self.page_target.on_action(&hotkeys, move || {
            if let Some(setup) = weak.upgrade() {
                let index = sender.indexOfSelectedItem().max(0) as usize;
                if let Some((value, _)) = HOTKEY_OPTIONS.get(index) {
                    setup.state.borrow_mut().hotkey = (*value).to_string();
                }
            }
        });
        self.page.addSubview(&hotkeys);
        cursor += 64.0;

        let card_width = page_width - 26.0;
        let shortcut = FlippedView::new(self.mtm, rect(8.0, cursor, card_width, 66.0));
        shortcut.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        style_surface(&shortcut, &palette, &SurfaceStyle::default());
        shortcut.addSubview(&label(
            self.mtm,
            &format!(
                "Hold {hotkey}  ·  Double-tap locks  ·  Esc cancels\nPaste last {}  ·  Command {}  ·  Polish {}  ·  Prompt {}",
                or_disabled(&choices.paste_last_hotkey),
                or_disabled(&choices.command_hotkey),
                or_disabled(&choices.polish_hotkey),
                or_disabled(&choices.prompt_hotkey),
            ),
            16.0,
            12.0,
            card_width - 32.0,
            LabelStyle {
                size: 11.0,
                color: "muted",
                height: 44.0,
                wrap: true,
                ..LabelStyle::default()
            },
            &palette,
        ));
        self.page.addSubview(&shortcut);
        cursor += 76.0;
        let pause_switch =
            NSButton::initWithFrame(self.mtm.alloc::<NSButton>(), rect(8.0, cursor, 320.0, 24.0));
        pause_switch.setButtonType(NSButtonType::Switch);
        pause_switch.setTitle(&NSString::from_str("Pause music while I talk"));
        pause_switch.setState(if pause {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        accessible_button(&pause_switch, "Pause music while I talk");
        let weak = self.me.clone();
        let sender = pause_switch.clone();
        self.page_target.on_action(&pause_switch, move || {
            if let Some(setup) = weak.upgrade() {
                setup.state.borrow_mut().pause_media = sender.state() == NSControlStateValueOn;
            }
        });
        self.page.addSubview(&pause_switch);
        cursor += 34.0;
        self.add_text("Live mic level", &mut cursor, 11.0, true, 18.0);
        let meter = NSProgressIndicator::initWithFrame(
            self.mtm.alloc::<NSProgressIndicator>(),
            rect(8.0, cursor, card_width, 18.0),
        );
        meter.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        meter.setIndeterminate(false);
        meter.setMinValue(0.0);
        meter.setMaxValue(1.0);
        let level = self.state.borrow().mic_level.unwrap_or(0.0);
        meter.setDoubleValue(level.clamp(0.0, 1.0));
        meter.setAccessibilityLabel(Some(&NSString::from_str("Live microphone level")));
        self.page.addSubview(&meter);
        *self.meter.borrow_mut() = Some(meter);
    }

    fn render_trial(&self) {
        let mut cursor = 8.0;
        let (status, error) = {
            let state = self.state.borrow();
            (state.status_message.clone(), state.error_message.clone())
        };
        if !status.is_empty() {
            self.add_text(&status, &mut cursor, 14.0, false, 30.0);
        }
        if !error.is_empty() {
            self.add_callout(&mut cursor, true, false);
        }
        self.page_button(
            &self.page,
            "Skip",
            8.0,
            cursor,
            110.0,
            ButtonKind::Secondary,
            "Skip dictation trial",
            |setup| setup.advance(),
        );
    }

    fn render_finish(&self) {
        let mut cursor = 8.0;
        let (state_saved, summary, mic_hotkey, start, darwin) = {
            let state = self.state.borrow();
            (
                state.saved,
                format!(
                    "Speech: {}  ·  Cleanup: {}",
                    state.stt_id,
                    state.llm_id.as_deref().unwrap_or("off")
                ),
                format!(
                    "Microphone: {}  ·  Hotkey: {}",
                    if state.device.is_empty() {
                        "System default"
                    } else {
                        &state.device
                    },
                    state.hotkey
                ),
                state.start_on_login,
                state.hardware.system == "Darwin",
            )
        };
        self.add_text(&summary, &mut cursor, 15.0, true, 26.0);
        self.add_text(&mic_hotkey, &mut cursor, 13.0, false, 26.0);
        if darwin {
            let palette = *self.palette.borrow();
            let login = NSButton::initWithFrame(
                self.mtm.alloc::<NSButton>(),
                rect(8.0, cursor, 320.0, 28.0),
            );
            login.setButtonType(NSButtonType::Switch);
            login.setTitle(&NSString::from_str("Start LocalFlow on login"));
            login.setState(if start {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            accessible_button(&login, "Start LocalFlow on login");
            let weak = self.me.clone();
            let sender = login.clone();
            self.page_target.on_action(&login, move || {
                if let Some(setup) = weak.upgrade() {
                    setup.state.borrow_mut().start_on_login =
                        sender.state() == NSControlStateValueOn;
                }
            });
            self.page.addSubview(&login);
            let _ = palette;
            cursor += 36.0;
        }
        if state_saved {
            self.page_button(
                &self.page,
                "Launch localflow now",
                8.0,
                cursor,
                190.0,
                ButtonKind::Secondary,
                "Launch LocalFlow now",
                |setup| {
                    if let Some(launch) = &setup.services.hooks.launch_runtime {
                        launch();
                    } else {
                        setup.state.borrow_mut().error_message =
                            "Runtime launch isn't connected to this setup window.".into();
                        setup.rebuild();
                    }
                },
            );
        }
    }

    fn render_footer(&self) {
        let palette = *self.palette.borrow();
        let state = self.state.borrow();
        self.back_button.setHidden(state.step == 0 || state.saved);
        self.back_button.setEnabled(!state.model_busy());
        self.primary_button.setEnabled(state.busy.is_empty());
        self.cancel_button
            .setHidden(state.cancel.is_none() || !matches!(state.step, 2 | 3 | 5));
        let cancel_title = if state.busy_job(JobKind::Trial) {
            "Cancel recording"
        } else {
            "Cancel download"
        };
        accessible_button(&self.cancel_button, cancel_title);
        let primary = if state.saved {
            "Close"
        } else if state.step == 2 {
            if state
                .downloaded
                .get(&state.stt_id)
                .copied()
                .unwrap_or(false)
            {
                "Continue"
            } else {
                "Download & continue"
            }
        } else if state.step == 3 {
            if state.choices.llm_provider != "ollama"
                || state.llm_id.is_none()
                || !state.ollama_running
                || state
                    .llm_id
                    .as_ref()
                    .is_some_and(|id| state.ollama_models.contains(id))
            {
                "Continue"
            } else {
                "Download & continue"
            }
        } else if state.step == 5 {
            if state.busy_job(JobKind::Trial) {
                "Recording…"
            } else {
                "Record 5 seconds"
            }
        } else if state.step == 6 {
            "Finish"
        } else if state.step == 0 {
            "Get started"
        } else {
            "Continue"
        };
        self.primary_button.setTitle(&NSString::from_str(primary));
        accessible_button(&self.primary_button, primary);
        let ollama_error = visible_ollama_error(
            state.step,
            &state.choices.llm_provider,
            state.llm_id.as_deref(),
            state.ollama_error.as_deref(),
        );
        let status = footer_status(
            state.step,
            &state.error_message,
            ollama_error,
            &state.status_message,
        );
        let cancel_visible = !self.cancel_button.isHidden();
        drop(state);
        self.layout_footer(&status.text, cancel_visible);
        self.status
            .setStringValue(&NSString::from_str(&status.text));
        self.status.setTextColor(Some(&color(
            if status.error { "red" } else { "muted" },
            &palette,
        )));
        let tooltip = (!status.text.is_empty()).then(|| NSString::from_str(&status.text));
        self.status.setToolTip(tooltip.as_deref());
    }

    /// Fit the footer buttons to their titles and wrap the status line in the
    /// space left of them, so long titles and errors never overlap or clip.
    fn layout_footer(&self, status_text: &str, cancel_visible: bool) {
        let size = self.root.bounds().size;
        let title_width = |button: &NSButton| {
            let title_font = button
                .font()
                .unwrap_or_else(|| font(13.0, Weight::Semibold));
            measure_text(&button.title().to_string(), &title_font)
        };
        let layout = setup_footer_layout(
            size.width,
            title_width(&self.primary_button),
            cancel_visible,
            title_width(&self.cancel_button),
        );
        let button_y = size.height - 46.0;
        self.primary_button
            .setFrame(rect(layout.primary.x, button_y, layout.primary.width, 34.0));
        self.cancel_button
            .setFrame(rect(layout.cancel.x, button_y, layout.cancel.width, 34.0));
        let status_font = self
            .status
            .font()
            .unwrap_or_else(|| font(TYPE_CAPTION.size, Weight::Regular));
        let (_, y, height) = footer_status_frame(
            status_text,
            layout.status.width,
            size.height,
            &|line: &str| measure_text(line, &status_font),
        );
        self.status
            .setFrame(rect(layout.status.x, y, layout.status.width, height));
    }

    // -------------------------------------------------------------- actions ---

    fn advance(&self) {
        if self.state.borrow().busy_job(JobKind::Trial) {
            self.cancel_download();
        }
        {
            let mut state = self.state.borrow_mut();
            if state.step >= SETUP_STEPS - 1 {
                return;
            }
            state.sync_choices();
            if state.step == 4 {
                drop(state);
                self.reset_monitor();
                state = self.state.borrow_mut();
            }
            state.step += 1;
            state.status_message.clear();
            state.error_message.clear();
            state.scroll_y = 0.0;
        }
        self.rebuild();
    }

    fn back(&self) {
        {
            let mut state = self.state.borrow_mut();
            if state.model_busy() || !valid_setup_step(state.step) || state.step == 0 || state.saved
            {
                return;
            }
            if state.step == 4 {
                drop(state);
                self.reset_monitor();
                state = self.state.borrow_mut();
            }
            state.step -= 1;
            state.status_message.clear();
            state.error_message.clear();
            state.scroll_y = 0.0;
        }
        self.rebuild();
    }

    fn primary(&self) {
        let (step, saved) = {
            let state = self.state.borrow();
            (state.step, state.saved)
        };
        if saved {
            self.close();
            return;
        }
        match step {
            0 | 1 | 4 => self.advance(),
            2 => {
                let ready = {
                    let state = self.state.borrow();
                    state
                        .downloaded
                        .get(&state.stt_id)
                        .copied()
                        .unwrap_or(false)
                };
                if ready {
                    self.advance();
                } else {
                    self.start_speech_download();
                }
            }
            3 => self.continue_cleanup(),
            5 => self.start_trial(),
            6 => self.start_apply(),
            _ => {}
        }
    }

    fn continue_cleanup(&self) {
        let decision = {
            let state = self.state.borrow();
            (
                state.choices.llm_provider.clone(),
                state.llm_id.clone(),
                state.ollama_running,
                state.state_llm_downloaded(),
            )
        };
        let (provider, llm_id, running, downloaded) = decision;
        if provider == "ollama" && llm_id.is_some() && !running {
            self.state.borrow_mut().error_message =
                "Ollama isn't running — start it, or choose No cleanup.".into();
            self.rebuild();
        } else if provider != "ollama" || llm_id.is_none() || downloaded {
            self.advance();
        } else {
            self.start_cleanup_download();
        }
    }

    fn select_stt(&self, index: usize) {
        let mut state = self.state.borrow_mut();
        if state.busy_job(JobKind::SpeechDownload) {
            return;
        }
        if let Some(model) = state.models.get(index) {
            state.stt_id = model.id.clone();
            state.error_message.clear();
            state.scroll_y = 0.0;
        } else {
            return;
        }
        drop(state);
        self.rebuild();
    }

    fn select_llm(&self, index: usize) {
        let mut state = self.state.borrow_mut();
        if state.busy_job(JobKind::CleanupDownload) {
            return;
        }
        if index == 0 {
            state.llm_id = None;
        } else if let Some(model) = LLM_MODELS.get(index - 1) {
            state.llm_id = Some(model.id.to_string());
        } else {
            return;
        }
        state.error_message.clear();
        drop(state);
        self.rebuild();
    }

    fn select_cleanup_level(&self, level: &str) {
        if !matches!(level, "light" | "medium") {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            state.cleanup_level = level.to_string();
            state.choices.cleanup_level = level.to_string();
        }
        self.rebuild();
    }

    fn device_changed(&self, sender: &NSPopUpButton) {
        let index = sender.indexOfSelectedItem().max(0) as usize;
        let device = {
            let state = self.state.borrow();
            if index == 0 {
                String::new()
            } else {
                state
                    .input_devices
                    .as_ref()
                    .ok()
                    .and_then(|devices| devices.get(index - 1))
                    .cloned()
                    .unwrap_or_default()
            }
        };
        self.state.borrow_mut().device = device;
        self.reset_monitor();
        self.start_monitor();
    }

    fn recheck(&self) {
        {
            let mut state = self.state.borrow_mut();
            // Invalidate the previous operation even if it is still running.
            // Only the newest recheck is allowed to populate the list.
            state.operations.remove("checks");
            state.busy.remove("checks");
            state.checks = None;
            state.check_error = None;
            state.error_message.clear();
            state.scroll_y = 0.0;
        }
        self.start_checks();
        self.rebuild();
    }

    fn recheck_ollama(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.operations.remove("ollama-check");
            state.busy.remove("ollama-check");
            state.ollama_checked = false;
            state.ollama_error = None;
            state.error_message.clear();
        }
        self.start_ollama_check();
        self.rebuild();
    }

    fn fix_check(&self, index: usize) {
        let check = self
            .state
            .borrow()
            .checks
            .as_ref()
            .and_then(|checks| checks.get(index))
            .cloned();
        let Some(check) = check else { return };
        let url = check.fix_url.as_str();
        if url.starts_with("https://")
            || url.starts_with("http://")
            || url.starts_with("x-apple.systempreferences:")
        {
            self.open_url(url);
        } else if !url.is_empty() {
            let alert = NSAlert::new(self.mtm);
            alert.setMessageText(&NSString::from_str("Run this command"));
            alert.setInformativeText(&NSString::from_str(url));
            alert.runModal();
        }
    }

    fn open_url(&self, url: &str) {
        let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) else {
            self.state.borrow_mut().error_message = "The settings/download URL is invalid.".into();
            self.rebuild();
            return;
        };
        if !NSWorkspace::sharedWorkspace().openURL(&url) {
            self.state.borrow_mut().error_message =
                "macOS couldn't open the requested link.".into();
            self.rebuild();
        }
    }

    fn open_settings(&self) {
        if let Some(open) = &self.services.hooks.open_settings {
            open();
            return;
        }
        let port = self
            .state
            .borrow()
            .config
            .field_i64("server_port")
            .unwrap_or(8765);
        self.open_url(&format!("http://127.0.0.1:{port}/settings"));
    }

    fn cancel_download(&self) {
        let cancel = self.state.borrow().cancel.clone();
        if let Some((id, cancel)) = cancel {
            cancel.store(true, Ordering::Release);
            let mut state = self.state.borrow_mut();
            state.busy.remove(JobKind::SpeechDownload.as_str());
            state.busy.remove(JobKind::CleanupDownload.as_str());
            state.busy.remove(JobKind::Trial.as_str());
            state.operations.retain(|_, operation| *operation != id);
            state.cancel = None;
            state.last_progress = None;
            state.stalled = false;
            state.error_message.clear();
            state.status_message = "Operation cancelled.".into();
        }
        self.stop_monitor_if_closed();
        self.rebuild();
    }

    // --------------------------------------------------------------- workers ---

    fn begin_job(&self, kind: JobKind) -> Option<u64> {
        let mut state = self.state.borrow_mut();
        if state.closed || state.snapshot_mode || state.busy_job(kind) {
            return None;
        }
        state.next_job = state.next_job.wrapping_add(1).max(1);
        let id = state.next_job;
        state.busy.insert(kind.as_str());
        state.operations.insert(kind.as_str(), id);
        state.error_message.clear();
        if kind.is_download() || kind == JobKind::Trial {
            let cancel = Arc::new(AtomicBool::new(false));
            state.cancel = Some((id, cancel));
            state.last_progress = Some((Instant::now(), 0));
            state.stalled = false;
        }
        Some(id)
    }

    fn spawn_job<F>(&self, kind: JobKind, work: F)
    where
        F: FnOnce(
                u64,
                mpsc::Sender<SetupEvent>,
                Arc<AtomicBool>,
            ) -> std::result::Result<JobOutput, String>
            + Send
            + 'static,
    {
        let Some(id) = self.begin_job(kind) else {
            return;
        };
        let cancel = self
            .state
            .borrow()
            .cancel
            .as_ref()
            .filter(|(operation, _)| *operation == id)
            .map(|(_, cancel)| cancel.clone())
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let worker_sender = self.sender.clone();
        let reply_sender = self.sender.clone();
        let write = (kind == JobKind::Apply).then(crate::shutdown::register_pending_write);
        let spawn = std::thread::Builder::new()
            .name(format!("setup-{}", kind.as_str()))
            .spawn(move || {
                let _write = write;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    work(id, reply_sender, cancel)
                }))
                .unwrap_or_else(|_| Err("Setup worker panicked.".into()));
                let _ = worker_sender.send(SetupEvent::Job(JobEvent { kind, id, result }));
            });
        if let Err(error) = spawn {
            let _ = self.sender.send(SetupEvent::Job(JobEvent {
                kind,
                id,
                result: Err(format!("Couldn't start setup worker: {error}")),
            }));
        }
        self.rebuild();
    }

    fn start_checks(&self) {
        if self.state.borrow().busy_job(JobKind::Checks) {
            return;
        }
        let runner = self.services.checks.clone();
        let config = self.state.borrow().config.clone();
        self.spawn_job(JobKind::Checks, move |_id, _sender, _cancel| {
            runner.run(&config).map(JobOutput::Checks)
        });
    }

    fn start_speech_checks(&self) {
        let backend = self.services.models.clone();
        let models = self.state.borrow().models.clone();
        self.spawn_job(JobKind::SpeechChecks, move |_id, _sender, _cancel| {
            backend
                .speech_downloads(&models)
                .map(JobOutput::SpeechDownloads)
        });
    }

    fn start_ollama_check(&self) {
        if self.state.borrow().choices.llm_provider != "ollama" {
            return;
        }
        {
            self.state.borrow_mut().ollama_checked = true;
        }
        let backend = self.services.models.clone();
        let base_url = self
            .state
            .borrow()
            .config
            .field_str("llm_base_url")
            .unwrap_or("")
            .to_string();
        self.spawn_job(JobKind::OllamaCheck, move |_id, _sender, _cancel| {
            backend
                .ollama(&base_url)
                .map(|(running, models)| JobOutput::Ollama(running, models))
        });
    }

    fn start_speech_download(&self) {
        let model = self.state.borrow().current_stt().cloned();
        let Some(model) = model else {
            self.state.borrow_mut().error_message =
                "Choose an available speech model before continuing.".into();
            self.rebuild();
            return;
        };
        let backend = self.services.models.clone();
        self.spawn_job(JobKind::SpeechDownload, move |id, sender, cancel| {
            let callback: ProgressCallback = Box::new(move |progress| {
                let _ = sender.send(SetupEvent::Progress {
                    kind: JobKind::SpeechDownload,
                    id,
                    progress,
                });
            });
            backend.download_speech(&model, callback, cancel)?;
            Ok(JobOutput::Downloaded)
        });
        self.state.borrow_mut().status_message = "Preparing download…".into();
        self.rebuild();
    }

    fn start_cleanup_download(&self) {
        let model_id = self.state.borrow().llm_id.clone();
        let Some(model_id) = model_id else {
            self.advance();
            return;
        };
        let backend = self.services.models.clone();
        let base_url = self
            .state
            .borrow()
            .config
            .field_str("llm_base_url")
            .unwrap_or("")
            .to_string();
        self.spawn_job(JobKind::CleanupDownload, move |id, sender, cancel| {
            let callback: ProgressCallback = Box::new(move |progress| {
                let _ = sender.send(SetupEvent::Progress {
                    kind: JobKind::CleanupDownload,
                    id,
                    progress,
                });
            });
            backend.pull_cleanup(&base_url, &model_id, callback, cancel)?;
            Ok(JobOutput::Downloaded)
        });
        self.state.borrow_mut().status_message = "Preparing Ollama download…".into();
        self.rebuild();
    }

    fn start_trial(&self) {
        let (model, device, language) = {
            let state = self.state.borrow();
            (
                state.current_stt().cloned(),
                if state.device.is_empty() {
                    None
                } else {
                    Some(state.device.clone())
                },
                state.language.clone(),
            )
        };
        let Some(model) = model else {
            self.state.borrow_mut().error_message = "No speech model is selected.".into();
            self.rebuild();
            return;
        };
        let backend = self.services.models.clone();
        self.spawn_job(JobKind::Trial, move |_id, _sender, cancel| {
            backend
                .try_dictation_cancellable(&model, device.as_deref(), language.as_deref(), cancel)
                .map(JobOutput::Trial)
        });
        self.state.borrow_mut().status_message = "Loading speech model…".into();
        self.rebuild();
    }

    fn start_apply(&self) {
        let choices = {
            let mut state = self.state.borrow_mut();
            state.sync_choices();
            state.choices.clone()
        };
        let runner = self.services.apply.clone();
        self.spawn_job(JobKind::Apply, move |_id, _sender, _cancel| {
            runner.apply(&choices).map(JobOutput::Applied)
        });
    }

    fn start_monitor(&self) {
        let device = self.state.borrow().device.clone();
        if self.state.borrow().step != 4
            || self.state.borrow().snapshot_mode
            || self.active_mic.borrow().is_some()
            || self.state.borrow().busy_job(JobKind::Monitor)
            || self.state.borrow().mic_attempted.as_deref() == Some(device.as_str())
        {
            return;
        }
        self.state.borrow_mut().mic_attempted = Some(device.clone());
        let source = self.services.mic.clone();
        if let Some(source) = source.clone() {
            let cancelled = Arc::new(AtomicBool::new(false));
            *self.pending_mic.borrow_mut() = Some(PendingMic {
                source: source.clone(),
                cancelled: cancelled.clone(),
            });
            self.spawn_job(JobKind::Monitor, move |_id, _sender, _cancel| {
                source.start(
                    (!device.is_empty()).then_some(device.as_str()),
                    cancelled.clone(),
                )?;
                if cancelled.load(Ordering::Acquire) {
                    return Err("Microphone opening was cancelled.".into());
                }
                Ok(JobOutput::MonitorStarted)
            });
        } else {
            self.spawn_job(JobKind::Monitor, move |_id, _sender, _cancel| {
                Err("Microphone level isn't available without an audio service.".into())
            });
        }
    }

    fn stop_monitor(&self) {
        if let Some(pending) = self.pending_mic.borrow_mut().take() {
            pending.cancelled.store(true, Ordering::Release);
            pending.source.stop();
        }
        if let Some(mic) = self.active_mic.borrow_mut().take() {
            mic.source.stop();
        }
        let mut state = self.state.borrow_mut();
        state.operations.remove("monitor");
        state.busy.remove("monitor");
        state.mic_level = None;
        if let Some(meter) = self.meter.borrow().as_ref() {
            meter.setDoubleValue(0.0);
        }
    }

    fn reset_monitor(&self) {
        self.stop_monitor();
        self.state.borrow_mut().mic_attempted = None;
    }

    fn stop_monitor_if_closed(&self) {
        if self.is_closed() {
            self.stop_monitor();
        }
    }

    // -------------------------------------------------------------- event pump ---

    /// Drain worker events and sample the live meter. Normally called by the
    /// run-loop timer; public for deterministic native integration tests.
    pub fn pump(&self) {
        if self.is_closed() {
            return;
        }
        loop {
            let event = self.receiver.borrow().try_recv();
            match event {
                Ok(event) => self.handle_event(event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        self.poll_mic();
        self.check_download_stall();
    }

    pub fn drain_requests(&self) -> usize {
        self.drain_deferred();
        self.pump();
        self.drain_deferred();
        let state = self.state.borrow();
        if state.closed {
            0
        } else {
            state.operations.len()
                + self
                    .deferred
                    .borrow()
                    .iter()
                    .filter(|timer| timer.is_valid())
                    .count()
        }
    }

    fn drain_deferred(&self) {
        let timers = std::mem::take(&mut *self.deferred.borrow_mut());
        for timer in timers {
            if timer.is_valid() {
                timer.fire();
                timer.invalidate();
            }
        }
    }

    fn event_is_current(state: &State, kind: JobKind, id: u64) -> bool {
        !state.closed && state.operations.get(kind.as_str()) == Some(&id)
    }

    fn handle_event(&self, event: SetupEvent) {
        match event {
            SetupEvent::Progress { kind, id, progress } => {
                let changed = {
                    let mut state = self.state.borrow_mut();
                    if !Self::event_is_current(&state, kind, id) || !kind.is_download() {
                        return;
                    }
                    if state
                        .last_progress
                        .as_ref()
                        .is_none_or(|(_, bytes)| *bytes != progress.done)
                    {
                        state.last_progress = Some((Instant::now(), progress.done));
                        state.stalled = false;
                    }
                    let status = if progress.status.is_empty() {
                        String::new()
                    } else {
                        format!("  ·  {}", progress.status)
                    };
                    state.status_message = format!(
                        "{:.0} / {:.0} MB{status}",
                        progress.done as f64 / 1_000_000.0,
                        progress.total as f64 / 1_000_000.0
                    );
                    true
                };
                if changed {
                    self.rebuild();
                }
            }
            SetupEvent::Job(event) => self.handle_job(event),
        }
    }

    fn handle_job(&self, event: JobEvent) {
        let JobEvent { kind, id, result } = event;
        if !Self::event_is_current(&self.state.borrow(), kind, id) {
            return;
        }
        let model_ticket = if kind.is_download() {
            self.state
                .borrow()
                .current_stt()
                .map(|model| model.id.clone())
        } else {
            None
        };
        {
            let mut state = self.state.borrow_mut();
            if !Self::event_is_current(&state, kind, id) {
                return;
            }
            state.busy.remove(kind.as_str());
            state.operations.remove(kind.as_str());
            if kind.is_download() || kind == JobKind::Trial {
                state.cancel = None;
                state.last_progress = None;
                state.stalled = false;
            }
        }
        match result {
            Err(error) => {
                if kind == JobKind::Monitor {
                    if let Some(pending) = self.pending_mic.borrow_mut().take() {
                        pending.source.stop();
                    }
                }
                {
                    let mut state = self.state.borrow_mut();
                    if kind == JobKind::Checks {
                        state.checks = Some(Vec::new());
                        state.check_error = Some(error.clone());
                    } else if kind == JobKind::SpeechChecks {
                        state.downloaded = state
                            .models
                            .iter()
                            .map(|model| (model.id.clone(), false))
                            .collect();
                    } else if kind == JobKind::Monitor {
                        state.mic_level = None;
                    }
                    if kind == JobKind::OllamaCheck {
                        // Shown by the footer only while the selection needs
                        // Ollama (see `visible_ollama_error`).
                        state.ollama_checked = true;
                        state.ollama_running = false;
                        state.ollama_error = Some(error);
                    } else {
                        state.error_message = error;
                    }
                }
                self.rebuild();
            }
            Ok(output) => {
                let mut advance = false;
                let mut open_hub = false;
                {
                    let mut state = self.state.borrow_mut();
                    match (kind, output) {
                        (JobKind::Checks, JobOutput::Checks(checks)) => {
                            state.check_error = None;
                            state.checks = Some(checks);
                        }
                        (JobKind::SpeechChecks, JobOutput::SpeechDownloads(downloaded)) => {
                            state.downloaded.extend(downloaded);
                        }
                        (JobKind::OllamaCheck, JobOutput::Ollama(running, models)) => {
                            state.ollama_running = running;
                            state.ollama_models = models;
                            state.ollama_checked = true;
                            state.ollama_error = None;
                        }
                        (JobKind::SpeechDownload, JobOutput::Downloaded) => {
                            if let Some(model) = model_ticket {
                                state.downloaded.insert(model.clone(), true);
                                state.status_message = "Speech model downloaded.".into();
                                advance = state.step == 2 && state.stt_id == model;
                            }
                        }
                        (JobKind::CleanupDownload, JobOutput::Downloaded) => {
                            if let Some(model) = state.llm_id.clone() {
                                if !state.ollama_models.contains(&model) {
                                    state.ollama_models.push(model.clone());
                                }
                                state.status_message = format!("Downloaded {model}");
                                advance = state.step == 3
                                    && state.llm_id.as_deref() == Some(model.as_str());
                            }
                        }
                        (JobKind::Trial, JobOutput::Trial(transcript)) => {
                            state.status_message = if transcript.is_empty() {
                                "No speech detected. Try again near your microphone.".into()
                            } else {
                                transcript
                            };
                        }
                        (JobKind::Apply, JobOutput::Applied(path)) => {
                            state.saved = true;
                            state.status_message = path.display().to_string();
                            open_hub = true;
                        }
                        (JobKind::Monitor, JobOutput::MonitorStarted) => {
                            if let Some(pending) = self.pending_mic.borrow_mut().take() {
                                *self.active_mic.borrow_mut() = Some(ActiveMic {
                                    source: pending.source,
                                });
                            }
                            state.status_message.clear();
                        }
                        // A backend implementation returning the wrong result
                        // is surfaced rather than silently marked successful.
                        _ => {
                            state.error_message =
                                "Setup worker returned an unexpected result.".into();
                        }
                    }
                }
                if advance {
                    self.advance();
                } else {
                    self.rebuild();
                }
                // Finish opens the Hub in this process; it never quits the app.
                if open_hub {
                    if let Some(launch) = self.services.hooks.launch_runtime.clone() {
                        launch();
                    }
                }
            }
        }
    }

    fn poll_mic(&self) {
        if self.state.borrow().step != 4 {
            return;
        }
        let source = self
            .active_mic
            .borrow()
            .as_ref()
            .map(|mic| mic.source.clone());
        let Some(source) = source else { return };
        match source.level() {
            Ok(level) if level.is_finite() => {
                let level = level.clamp(0.0, 1.0);
                self.state.borrow_mut().mic_level = Some(level);
                if let Some(meter) = self.meter.borrow().as_ref() {
                    meter.setDoubleValue(level);
                }
            }
            Ok(_) => {
                self.state.borrow_mut().error_message =
                    "Microphone returned a non-finite level.".into();
                self.stop_monitor();
                self.rebuild();
            }
            Err(error) => {
                self.state.borrow_mut().error_message = error;
                self.stop_monitor();
                self.rebuild();
            }
        }
    }

    fn check_download_stall(&self) {
        let changed = {
            let mut state = self.state.borrow_mut();
            if state.stalled
                || (!state.busy_job(JobKind::SpeechDownload)
                    && !state.busy_job(JobKind::CleanupDownload))
            {
                return;
            }
            let Some((last, _)) = state.last_progress else {
                return;
            };
            if last.elapsed() < Duration::from_secs(DOWNLOAD_STALL_SECONDS) {
                return;
            }
            state.stalled = true;
            state.status_message = "No progress for 30s — check your connection, or Cancel.".into();
            state.error_message.clear();
            true
        };
        if changed {
            self.rebuild();
        }
    }

    // --------------------------------------------------------------- close ---

    fn teardown(&self) {
        {
            let mut state = self.state.borrow_mut();
            if state.closed {
                return;
            }
            state.closed = true;
            state.next_job = state.next_job.wrapping_add(1);
            state.operations.clear();
            state.busy.clear();
            if let Some((_, cancel)) = state.cancel.take() {
                cancel.store(true, Ordering::Release);
            }
        }
        self.stop_monitor();
        self.page_target.clear();
        self.shell_target.clear();
        if let Some(timer) = self.ticker.borrow_mut().take() {
            timer.invalidate();
        }
        for timer in std::mem::take(&mut *self.deferred.borrow_mut()) {
            timer.invalidate();
        }
    }

    /// Close and stop every timer, audio source and current model operation.
    /// In-flight replies are made stale before their next main-loop delivery.
    pub fn close(&self) {
        self.teardown();
        self.window.close();
    }
}

impl State {
    fn state_llm_downloaded(&self) -> bool {
        self.llm_id
            .as_ref()
            .is_some_and(|id| self.ollama_models.contains(id))
    }
}

impl Drop for SetupWindow {
    fn drop(&mut self) {
        // AppKit delegates/targets are unretained and must not outlive us.
        self.window.setDelegate(None);
        self.page_target.clear();
        self.shell_target.clear();
        if let Some(pending) = self.pending_mic.get_mut().take() {
            pending.cancelled.store(true, Ordering::Release);
            pending.source.stop();
        }
        if let Some(active) = self.active_mic.get_mut().take() {
            active.source.stop();
        }
    }
}

fn or_disabled(value: &str) -> &str {
    if value.is_empty() {
        "disabled"
    } else {
        value
    }
}

// ---------------------------------------------------------- snapshot seeds ---

/// Names match `scripts/mac_ui_snapshots.py`; the renderer adds the same
/// `-light` / `-dark` suffix and `.png` extension.
pub const SETUP_SNAPSHOT_NAMES: [&str; 18] = [
    "setup-step-1",
    "setup-step-2",
    "setup-step-3",
    "setup-step-4",
    "setup-step-5",
    "setup-step-6",
    "setup-step-7",
    "setup-checks-needs-action",
    "setup-checks-all-ready",
    "setup-checks-needs-action-wide",
    "setup-complete",
    "setup-cleanup-level-medium",
    "setup-non-ollama-provider",
    "setup-download-progress",
    "setup-download-error",
    "setup-cleanup-off-ollama-unavailable",
    "setup-cleanup-ollama-unavailable",
    "setup-speech-download-required",
];

/// Long, realistic Ollama check failure used to show the footer wrapping.
const SNAPSHOT_OLLAMA_ERROR: &str = "Ollama isn't reachable at http://127.0.0.1:11434 \
    (connection refused). Start Ollama, or choose No cleanup to continue without it.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupSnapshotState {
    Step(usize),
    ChecksNeedsAction,
    ChecksAllReady,
    ChecksNeedsActionWide,
    Complete,
    CleanupLevelMedium,
    NonOllamaProvider,
    DownloadProgress,
    DownloadError,
    /// "No cleanup" with a failed Ollama check: no error is shown.
    CleanupOffOllamaUnavailable,
    /// An Ollama model selected while Ollama is unavailable: wrapped error.
    CleanupOllamaUnavailable,
    /// Speech model not yet downloaded: full "Download & continue" title.
    SpeechDownloadRequired,
}

impl SetupSnapshotState {
    pub fn file_stem(self) -> &'static str {
        match self {
            Self::Step(step) if valid_setup_step(step) => SETUP_SNAPSHOT_NAMES[step],
            Self::Step(_) => SETUP_SNAPSHOT_NAMES[0],
            Self::ChecksNeedsAction => "setup-checks-needs-action",
            Self::ChecksAllReady => "setup-checks-all-ready",
            Self::ChecksNeedsActionWide => "setup-checks-needs-action-wide",
            Self::Complete => "setup-complete",
            Self::CleanupLevelMedium => "setup-cleanup-level-medium",
            Self::NonOllamaProvider => "setup-non-ollama-provider",
            Self::DownloadProgress => "setup-download-progress",
            Self::DownloadError => "setup-download-error",
            Self::CleanupOffOllamaUnavailable => "setup-cleanup-off-ollama-unavailable",
            Self::CleanupOllamaUnavailable => "setup-cleanup-ollama-unavailable",
            Self::SpeechDownloadRequired => "setup-speech-download-required",
        }
    }
}

const SNAPSHOT_CHECKS: [(&str, &str, &str, &str, &str); 7] = [
    (
        "ffmpeg",
        "ffmpeg",
        "Installed",
        "Install ffmpeg",
        "https://formulae.brew.sh/formula/ffmpeg",
    ),
    (
        "ollama-installed",
        "Ollama installed",
        "Installed",
        "Get Ollama",
        "https://ollama.com/download",
    ),
    (
        "ollama-running",
        "Ollama running",
        "Ready",
        "Start Ollama",
        "https://ollama.com/download",
    ),
    (
        "microphone-device",
        "Input device",
        "1 input device(s) found",
        "Check microphone",
        "",
    ),
    (
        "microphone",
        "Microphone permission",
        "Unknown",
        "Open settings",
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
    ),
    (
        "accessibility",
        "Accessibility permission",
        "Not granted",
        "Open settings",
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
    ),
    (
        "input-monitoring",
        "Input Monitoring permission",
        "Not granted",
        "Open settings",
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent",
    ),
];

/// Seven real check rows matching the Python snapshot fixture.
pub fn snapshot_checks(all_ready: bool) -> Vec<Check> {
    SNAPSHOT_CHECKS
        .iter()
        .map(|(key, label, detail, fix_label, fix_url)| {
            let ok = if all_ready {
                Some(true)
            } else {
                match *key {
                    "microphone" => None,
                    "accessibility" | "input-monitoring" => Some(false),
                    _ => Some(true),
                }
            };
            Check::new(
                key,
                label,
                ok,
                if all_ready && matches!(*key, "microphone" | "accessibility" | "input-monitoring")
                {
                    "Granted"
                } else {
                    detail
                },
                fix_label,
                fix_url,
            )
        })
        .collect()
}

fn snapshot_page_checks() -> Vec<Check> {
    vec![
        Check::new("ffmpeg", "ffmpeg", Some(true), "Installed", "", ""),
        Check::new(
            "ollama-installed",
            "Ollama installed",
            Some(true),
            "Installed",
            "",
            "",
        ),
        Check::new(
            "ollama-running",
            "Ollama running",
            Some(true),
            "Ready",
            "",
            "",
        ),
        Check::new(
            "microphone-device",
            "Input device",
            Some(true),
            "1 input device found",
            "",
            "",
        ),
    ]
}

/// `models.detect_hardware` reports bytes divided by 1024³. HostPlatform has
/// already measured the same bytes, but currently reports decimal GB.
fn python_hardware_units(mut hardware: HardwareInfo) -> HardwareInfo {
    let decimal_gb_to_gib = 1_000_000_000.0 / 1_073_741_824.0;
    hardware.ram_gb *= decimal_gb_to_gib;
    hardware.free_disk_gb *= decimal_gb_to_gib;
    hardware
}

fn snapshot_platform(hardware: HardwareInfo) -> crate::setup_flow::FakeSetupPlatform {
    crate::setup_flow::FakeSetupPlatform {
        system: hardware.system.clone(),
        machine: hardware.machine.clone(),
        commands: vec!["ffmpeg".into()],
        ollama_installed: true,
        ollama_running: Some(true),
        input_devices: Ok(vec!["Built-in Microphone".into()]),
        // The Python snapshot environment ships faster-whisper; optional MLX
        // and Parakeet modules are absent. Do not invent those runtimes here.
        runtimes: vec!["faster-whisper".into()],
        hardware: Ok(hardware),
        ..crate::setup_flow::FakeSetupPlatform::default()
    }
}

/// Python-compatible setup fixture using the actual host hardware, just like
/// `SetupWindowController.init()`. Snapshot mode disables all workers and
/// audio; remaining fake platform data is confined to fixture presentation.
pub fn snapshot_fixture(mtm: MainThreadMarker) -> Rc<SetupWindow> {
    use crate::setup_flow::FakeLaunchAgent;

    let hardware = python_hardware_units(
        HostPlatform
            .detect_hardware()
            .expect("setup snapshots must detect the actual host hardware"),
    );
    let platform = snapshot_platform(hardware.clone());
    let config = Config::defaults_for(hardware.system == "Darwin");
    let context = SetupContext::detect(
        config,
        hardware,
        &platform,
        &FakeLaunchAgent::new("not installed"),
    )
    .expect("the shipped faster-whisper runtime provides a speech catalog");
    let setup = SetupWindow::new(mtm, context, SetupServices::default());
    setup.state.borrow_mut().snapshot_mode = true;
    setup.rebuild();
    setup
}

impl SetupWindow {
    /// Enable/disable all background and audio work. Intended for the snapshot
    /// driver; normal callers leave this false.
    pub fn set_snapshot_mode(&self, enabled: bool) {
        if enabled {
            self.reset_monitor();
        }
        self.state.borrow_mut().snapshot_mode = enabled;
        self.rebuild();
    }

    /// Seed one Python-compatible setup snapshot and relayout. No config,
    /// network, microphone or launch-agent access occurs.
    pub fn seed_snapshot(&self, fixture: SetupSnapshotState) {
        self.set_snapshot_mode(true);
        {
            let mut state = self.state.borrow_mut();
            state.step = 0;
            state.saved = false;
            state.checks = Some(snapshot_page_checks());
            state.check_error = None;
            state.downloaded = state
                .models
                .iter()
                .map(|model| (model.id.clone(), true))
                .collect();
            state.ollama_checked = true;
            state.ollama_running = true;
            state.ollama_error = None;
            state.ollama_models = vec!["llama3.2:1b".into()];
            state.stt_id = state.choices.stt_id.clone();
            state.llm_id = Some("llama3.2:1b".into());
            state.choices.llm_provider = state
                .config
                .field_str("llm_provider")
                .unwrap_or("ollama")
                .to_string();
            state.choices.cleanup_level = state
                .config
                .field_str("cleanup_level")
                .unwrap_or("light")
                .to_string();
            // The script changes this property before the non-Ollama and
            // download fixtures and never restores it between those shots.
            state.cleanup_level = if matches!(
                fixture,
                SetupSnapshotState::CleanupLevelMedium
                    | SetupSnapshotState::NonOllamaProvider
                    | SetupSnapshotState::DownloadProgress
                    | SetupSnapshotState::DownloadError
            ) {
                "medium".into()
            } else {
                state.choices.cleanup_level.clone()
            };
            state.status_message.clear();
            state.error_message.clear();
            state.busy.clear();
            state.operations.clear();
            state.cancel = None;
            state.last_progress = None;
            state.stalled = false;
            state.scroll_y = 0.0;
            state.mic_level = None;
        }
        match fixture {
            SetupSnapshotState::Step(step) => {
                self.state.borrow_mut().step = step.min(SETUP_STEPS - 1);
            }
            SetupSnapshotState::ChecksNeedsAction => {
                let mut state = self.state.borrow_mut();
                state.step = 1;
                state.checks = Some(snapshot_checks(false));
            }
            SetupSnapshotState::ChecksAllReady => {
                let mut state = self.state.borrow_mut();
                state.step = 1;
                state.checks = Some(snapshot_checks(true));
            }
            SetupSnapshotState::ChecksNeedsActionWide => {
                {
                    let mut state = self.state.borrow_mut();
                    state.step = 1;
                    state.checks = Some(snapshot_checks(false));
                }
                self.rebuild();
                self.resize_content(1100.0, f64::from(WINDOW_MIN_HEIGHT));
            }
            SetupSnapshotState::Complete => {
                let mut state = self.state.borrow_mut();
                state.step = 6;
                state.saved = true;
            }
            SetupSnapshotState::CleanupLevelMedium => {
                let mut state = self.state.borrow_mut();
                state.step = 3;
                state.cleanup_level = "medium".into();
            }
            SetupSnapshotState::NonOllamaProvider => {
                let mut state = self.state.borrow_mut();
                state.step = 3;
                state.choices.llm_provider = "openai-compatible".into();
                // A stored Ollama failure must stay hidden for this provider.
                state.ollama_error = Some(SNAPSHOT_OLLAMA_ERROR.into());
            }
            SetupSnapshotState::CleanupOffOllamaUnavailable => {
                let mut state = self.state.borrow_mut();
                state.step = 3;
                state.choices.llm_provider = "ollama".into();
                state.llm_id = None;
                state.ollama_running = false;
                state.ollama_models.clear();
                state.ollama_error = Some(SNAPSHOT_OLLAMA_ERROR.into());
            }
            SetupSnapshotState::CleanupOllamaUnavailable => {
                let mut state = self.state.borrow_mut();
                state.step = 3;
                state.choices.llm_provider = "ollama".into();
                state.ollama_running = false;
                state.ollama_models.clear();
                state.ollama_error = Some(SNAPSHOT_OLLAMA_ERROR.into());
            }
            SetupSnapshotState::SpeechDownloadRequired => {
                let mut state = self.state.borrow_mut();
                state.step = 2;
                state.choices.llm_provider = "ollama".into();
                let stt_id = state.stt_id.clone();
                state.downloaded.insert(stt_id, false);
            }
            SetupSnapshotState::DownloadProgress => {
                let mut state = self.state.borrow_mut();
                state.step = 2;
                state.choices.llm_provider = "ollama".into();
                state.busy.insert("stt-download");
                state.status_message = "43 / 78 MB".into();
            }
            SetupSnapshotState::DownloadError => {
                let mut state = self.state.borrow_mut();
                state.step = 2;
                state.choices.llm_provider = "ollama".into();
                state.status_message = "Download failed.".into();
                state.error_message = "Network connection interrupted.".into();
            }
        }
        self.rebuild();
    }
}

// ------------------------------------------------------------------- tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup_flow::{FakeLaunchAgent, FakeSetupPlatform};

    fn test_context() -> SetupContext {
        let hardware = HardwareInfo::new("Darwin", "arm64", 16.0, true, 256.0);
        let platform = FakeSetupPlatform {
            system: "Darwin".into(),
            machine: "arm64".into(),
            runtimes: vec!["faster-whisper".into()],
            ..FakeSetupPlatform::default()
        };
        SetupContext::detect(
            Config::defaults_for(true),
            hardware,
            &platform,
            &FakeLaunchAgent::new("not installed"),
        )
        .unwrap()
    }

    #[test]
    fn model_sizes_use_python_grouping() {
        assert_eq!(format_model_size_mb(78), "78");
        assert_eq!(format_model_size_mb(1000), "1,000");
        assert_eq!(format_model_size_mb(1321), "1,321");
        assert_eq!(format_model_size_mb(1234567), "1,234,567");
    }

    #[test]
    fn callout_titles_keep_validation_errors_distinct_from_downloads() {
        assert_eq!(callout_title(false, true), "Download progress");
        assert_eq!(callout_title(true, true), "Download error");
        assert_eq!(callout_title(true, false), "Setup error");
        assert_eq!(callout_title(false, false), "Progress");
    }

    #[test]
    fn snapshot_platform_matches_low_memory_python_recommendation() {
        let hardware =
            python_hardware_units(HardwareInfo::new("Darwin", "arm64", 8.0, true, 100.0));
        assert!(hardware.ram_gb < 8.0);
        assert!((hardware.ram_gb - 7.450_580_596_923_828).abs() < 0.000_001);
        let platform = snapshot_platform(hardware.clone());
        let context = SetupContext::detect(
            Config::defaults_for(true),
            hardware,
            &platform,
            &FakeLaunchAgent::new("not installed"),
        )
        .unwrap();
        assert_eq!(context.choices.stt_id, "fw-tiny");
        assert_eq!(context.choices.llm_id, None);
        assert_eq!(context.models.len(), 7);
        assert!(context
            .models
            .iter()
            .all(|model| model.backend == "faster-whisper"));
    }

    #[test]
    fn action_labels_and_order_preserve_original_indices() {
        let mut checks = snapshot_checks(false);
        let order = ordered_check_indexes(&checks);
        assert_eq!(
            order
                .iter()
                .map(|index| checks[*index].key.as_str())
                .collect::<Vec<_>>(),
            vec![
                "microphone",
                "accessibility",
                "input-monitoring",
                "ffmpeg",
                "ollama-installed",
                "ollama-running",
                "microphone-device"
            ]
        );
        assert_eq!(check_action_label(&checks[4]), Some("Allow microphone"));
        checks[4].ok = Some(true);
        assert_eq!(check_action_label(&checks[4]), None);
        let command = Check::new(
            "other",
            "Other",
            None,
            "detail",
            "Other action",
            "not-a-url",
        );
        assert_eq!(check_action_label(&command), Some("Other action"));
    }

    #[test]
    fn status_mapping_does_not_treat_unknown_as_ready() {
        assert_eq!(check_status(None, false).0, "Unknown");
        assert_eq!(check_status(Some(false), false).0, "Needs you");
        assert_eq!(check_status(Some(true), false).0, "Ready");
        assert_eq!(check_status(Some(false), true).0, "Checking…");
    }

    #[test]
    fn empty_check_results_are_not_all_ready() {
        let checks: Vec<Check> = Vec::new();
        assert!(ordered_check_indexes(&checks).is_empty());
        // The renderer has a distinct empty branch before its all-ready branch.
        assert!(!(!checks.is_empty() && checks.iter().all(|check| check.ok == Some(true))));
    }

    #[test]
    fn control_choices_survive_rebuild_state_cloning() {
        let mut state = State::new(test_context());
        state.device = "USB mic".into();
        state.hotkey = "f19".into();
        state.cleanup_level = "medium".into();
        state.pause_media = true;
        state.start_on_login = true;
        state.sync_choices();
        assert_eq!(state.choices.input_device, "USB mic");
        assert_eq!(state.choices.hotkey, "f19");
        assert_eq!(state.choices.cleanup_level, "medium");
        assert!(state.choices.pause_media_while_dictating);
        assert!(state.choices.start_on_login);
    }

    #[test]
    fn snapshot_names_and_seeds_match_the_script() {
        assert_eq!(SETUP_SNAPSHOT_NAMES.len(), 18);
        for state in [
            SetupSnapshotState::CleanupOffOllamaUnavailable,
            SetupSnapshotState::CleanupOllamaUnavailable,
            SetupSnapshotState::SpeechDownloadRequired,
        ] {
            assert!(
                SETUP_SNAPSHOT_NAMES.contains(&state.file_stem()),
                "{state:?}"
            );
        }
        for (index, name) in SETUP_SNAPSHOT_NAMES.iter().take(7).enumerate() {
            assert_eq!(SetupSnapshotState::Step(index).file_stem(), *name);
        }
        assert_eq!(snapshot_page_checks().len(), 4);
        assert!(snapshot_page_checks()
            .iter()
            .all(|check| check.ok == Some(true) && check.fix_label.is_empty()));
        assert_eq!(snapshot_page_checks()[3].detail, "1 input device found");
        assert_eq!(snapshot_checks(false).len(), 7);
        assert!(snapshot_checks(true)
            .iter()
            .all(|check| check.ok == Some(true)));
    }

    #[test]
    fn stale_replies_are_rejected() {
        let mut state = State::new(test_context());
        state.operations.insert("checks", 1);
        assert!(SetupWindow::event_is_current(&state, JobKind::Checks, 1));
        assert!(!SetupWindow::event_is_current(&state, JobKind::Checks, 2));
        state.closed = true;
        assert!(!SetupWindow::event_is_current(&state, JobKind::Checks, 1));
    }
}
