//! What Home and the sidebar may say about dictation, from runtime signals.
//!
//! Inputs are only what a runtime itself reports: `status.json` (protocol 2
//! adds listener, session, microphone and a heartbeat; see
//! `lf_runtime::status`), whether the PID it names is alive and who holds
//! `localflow.lock`, plus the state of an engine this window started. HTTP
//! `/healthz`, the config's `enabled` flag, a live PID alone or a spawned
//! thread never make the shortcut "listening".
//!
//! Wording: a confirmed keyboard listener is not proof that the microphone
//! or the speech model work (those are checked when dictation starts), so
//! the best state reads "Shortcut listening", never an unqualified "Ready".

use lf_runtime::singleinstance::LockProbe;
use lf_runtime::status::{ListenerFact, StatusFile};

use crate::data::DataRoot;

/// Status dot / banner tone, without GPUI types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Success,
    Info,
    Warning,
    Danger,
    Neutral,
    Recording,
}

/// The engine this window started, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnEngine {
    None,
    Starting,
    Running,
    Stopping,
    /// Another process already owns dictation (pid when known).
    OwnedElsewhere(Option<u32>),
    Failed(String),
}

/// What the status file looked like.
#[derive(Clone, Debug, PartialEq)]
// One short-lived value per 1 s probe; boxing the status would buy nothing.
#[allow(clippy::large_enum_variant)]
pub enum FileObs {
    Absent,
    Unreadable(String),
    Present(StatusFile),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub file: FileObs,
    /// Whether the PID in the status file is a live process.
    pub pid_alive: bool,
    /// Whether `localflow.lock` is actually held, and by whom (read-only
    /// probe; a pid merely recorded in a stale lock file doesn't count).
    pub lock: LockProbe,
    /// The identity line currently in `localflow.lock` (trimmed).
    pub lock_identity: Option<String>,
    /// The status-file `instance` of the runtime this window owns, from its
    /// live `RuntimeHandle` (None unless our engine is running).
    pub own_instance: Option<String>,
    pub now: f64,
    pub own: OwnEngine,
}

/// The desktop session for the shortcut line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    MacOs,
    X11,
    Wayland,
    Unknown,
    Other,
}

impl Session {
    pub fn detect() -> Self {
        if cfg!(target_os = "macos") {
            Session::MacOs
        } else if cfg!(target_os = "linux") {
            match lf_input::linux::session() {
                lf_input::linux::Session::X11 => Session::X11,
                lf_input::linux::Session::Wayland => Session::Wayland,
                lf_input::linux::Session::Unknown => Session::Unknown,
            }
        } else {
            Session::Other
        }
    }
}

/// Facts from the config and the machine (never treated as readiness).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub enabled: bool,
    /// Push-to-talk shortcut, formatted for display.
    pub key: String,
    pub session: Session,
    /// `[dictation] input_device` ("" = system default).
    pub configured_mic: String,
    /// The paste tool this session needs but that isn't installed.
    pub missing_paste_tool: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Stopped,
    /// The status file exists but can't be read: nothing is known.
    Unreadable,
    /// Another process provably holds the dictation lock (or this window's
    /// start was refused because of one) but reports no readable status.
    Elsewhere(Option<u32>),
    Starting,
    Listening,
    Recording(&'static str),
    Processing,
    Failed(String),
    /// Last seen (heartbeat or `since`), when known.
    Stale(Option<i64>),
    Unknown,
    Simulated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeView {
    pub state: State,
    pub lead: String,
    pub support: String,
    pub sidebar: (Signal, String, String),
    pub shortcut: (Signal, String),
    pub microphone: (Signal, String),
    pub warnings: Vec<String>,
    /// The shortcut listener is confirmed (protocol 2, ok, fresh heartbeat).
    pub listening: bool,
    /// Dictation is recording or transcribing right now.
    pub busy: bool,
    /// A runtime process is alive (this window's engine or another).
    pub alive: bool,
    pub pid: Option<u32>,
    /// Another process that *provably* holds the dictation lock right now
    /// (kernel lock table). A pid recorded in a lock or status file, or an
    /// unverifiable status, never fills this: Start then just tries, and
    /// the engine's own lock acquisition decides.
    pub external_owner: Option<u32>,
}

impl RuntimeView {
    /// Why trial/download/delete must wait, when dictation is mid-gesture.
    pub fn busy_reason(&self) -> Option<&'static str> {
        self.busy.then_some(
            "LocalFlow is recording or transcribing right now. Try again when it's idle.",
        )
    }
}

fn recording_mode(state: &str) -> Option<&'static str> {
    match state {
        "recording" => Some(""),
        "locked" => Some("hands-free"),
        "command" => Some("command"),
        _ => None,
    }
}

/// Classify one observation into exactly one [`State`].
pub fn classify(obs: &Observation) -> State {
    let from_file = classify_file(obs);
    match &obs.own {
        // A failed Start here only explains the state while no runtime
        // reports anything better (e.g. `lf run` started afterwards).
        OwnEngine::Failed(reason)
            if matches!(
                from_file,
                State::Stopped
                    | State::Stale(_)
                    | State::Unknown
                    | State::Unreadable
                    | State::Elsewhere(_)
            ) =>
        {
            State::Failed(reason.clone())
        }
        _ => from_file,
    }
}

fn classify_file(obs: &Observation) -> State {
    // Our engine hasn't confirmed a running runtime yet: whatever file is on
    // disk (e.g. a previous start's "ok") says nothing about this start.
    if obs.own == OwnEngine::Starting {
        return State::Starting;
    }
    let file = match &obs.file {
        FileObs::Absent | FileObs::Unreadable(_) => return no_status(obs),
        FileObs::Present(file) => file,
    };
    // Proof 1, in-process: this window owns a live runtime handle and the
    // file carries that runtime's own instance (works on every platform).
    let own_running = obs.own == OwnEngine::Running && obs.own_instance.is_some();
    let ours = own_running
        && file.instance.is_some()
        && file.instance == obs.own_instance
        && file.pid == Some(std::process::id());
    if own_running && !ours {
        // Our runtime runs but this file isn't from it (an earlier start in
        // this process, or someone else's): wait for ours to publish.
        return State::Starting;
    }
    if file.state.as_deref() == Some("stopped") {
        return State::Stopped;
    }
    let last_seen = file.heartbeat.or(file.since).map(|t| t as i64);
    if !obs.pid_alive {
        return State::Stale(last_seen);
    }
    // Proof 2, external: the kernel says the file's PID holds the lock now
    // *and* the file belongs to that lock acquisition (a same-process
    // restart or a recycled PID rewrote the lock identity since).
    if !ours {
        match (obs.lock, file.pid) {
            (LockProbe::Free, _) => return State::Stale(last_seen),
            (LockProbe::Held(holder), pid) if Some(holder) != pid => {
                return State::Stale(last_seen)
            }
            _ => {}
        }
    }
    if file.protocol < lf_runtime::status::PROTOCOL {
        return State::Unknown;
    }
    let same_acquisition = file.lock_identity.is_some() && file.lock_identity == obs.lock_identity;
    if !ours && matches!(obs.lock, LockProbe::Held(_)) && !same_acquisition {
        return State::Stale(last_seen);
    }
    if !file.fresh(obs.now) {
        return State::Stale(last_seen);
    }
    let state = classify_listener(file);
    let verified = ours || (matches!(obs.lock, LockProbe::Held(_)) && same_acquisition);
    // Where ownership can't be verified, never claim a working shortcut.
    if !verified
        && matches!(
            state,
            State::Listening | State::Recording(_) | State::Processing
        )
    {
        return State::Unknown;
    }
    state
}

/// No readable status file: say only what is proven.
fn no_status(obs: &Observation) -> State {
    let own_proven = obs.own == OwnEngine::Running && obs.own_instance.is_some();
    if own_proven {
        return State::Starting; // our runtime runs but hasn't published yet
    }
    if let OwnEngine::OwnedElsewhere(pid) = &obs.own {
        return State::Elsewhere(*pid);
    }
    match obs.lock {
        LockProbe::Held(holder) if holder == std::process::id() => State::Starting,
        LockProbe::Held(holder) => State::Elsewhere(Some(holder)),
        _ if matches!(obs.file, FileObs::Unreadable(_)) => State::Unreadable,
        _ => State::Stopped,
    }
}

fn classify_listener(file: &StatusFile) -> State {
    match file.listener {
        Some(ListenerFact::Starting) | None => State::Starting,
        Some(ListenerFact::Denied) => State::Failed(if file.listener_detail.is_empty() {
            "The keyboard listener was refused permission.".into()
        } else {
            file.listener_detail.clone()
        }),
        Some(ListenerFact::Dead) => State::Failed(if file.listener_detail.is_empty() {
            "The keyboard listener stopped.".into()
        } else {
            file.listener_detail.clone()
        }),
        Some(ListenerFact::Unsupported) => State::Failed(if file.listener_detail.is_empty() {
            "Global shortcuts aren't supported in this session.".into()
        } else {
            file.listener_detail.clone()
        }),
        Some(ListenerFact::Ok) => match file.state.as_deref() {
            Some("idle") => State::Listening,
            Some("processing") => State::Processing,
            Some(other) => match recording_mode(other) {
                Some(mode) => State::Recording(mode),
                None => State::Unknown,
            },
            None => State::Unknown,
        },
    }
}

fn shortcut_line(session: Session, listening: bool, state: &State) -> (Signal, String) {
    match session {
        Session::Wayland => (
            Signal::Warning,
            format!(
                "{} Your shortcut stays saved.",
                lf_input::linux::WAYLAND_UNSUPPORTED
            ),
        ),
        Session::X11 if listening => (
            Signal::Success,
            "Shortcut: listening in this X11 session (hold key and chords).".into(),
        ),
        Session::X11 => (
            Signal::Neutral,
            "Shortcut: supported on X11 — not confirmed until LocalFlow is running.".into(),
        ),
        Session::MacOs if listening => (
            Signal::Success,
            "Shortcut: the runtime's keyboard listener is active.".into(),
        ),
        Session::MacOs => (
            Signal::Neutral,
            "Shortcut: not confirmed until LocalFlow is running (needs Input Monitoring).".into(),
        ),
        Session::Unknown => (
            Signal::Neutral,
            "Shortcut: couldn't detect the desktop session; global shortcuts are unverified."
                .into(),
        ),
        Session::Other => (
            Signal::Neutral,
            if matches!(state, State::Listening) {
                "Shortcut: the runtime reports its listener is active.".into()
            } else {
                "Shortcut: global shortcuts on this platform are handled by the Python app.".into()
            },
        ),
    }
}

fn microphone_line(configured: &str, observed: Option<&str>) -> (Signal, String) {
    let set = if configured.is_empty() {
        "System default".to_string()
    } else {
        format!("“{configured}”")
    };
    let (signal, seen) = match observed {
        Some("available") => (Signal::Success, "available"),
        Some("missing") => (
            Signal::Warning,
            "not found — LocalFlow will fall back to the system default",
        ),
        _ => (Signal::Neutral, "not checked"),
    };
    (signal, format!("Microphone: set to {set} · {seen}"))
}

fn ago(now: f64, then: i64) -> String {
    let secs = (now as i64 - then).max(0);
    match secs {
        s if s < 90 => format!("{s} s ago"),
        s if s < 5400 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

fn warning_text(code: &str) -> Option<&'static str> {
    Some(match code {
        "input-monitoring" => "macOS hasn't granted Input Monitoring to the app running LocalFlow, so the shortcut may not see keys. Grant it in System Settings → Privacy & Security → Input Monitoring, then restart LocalFlow.",
        "dead-tap" => "The keyboard listener hasn't seen any key since it started. If the shortcut doesn't respond, grant Input Monitoring and restart LocalFlow.",
        "speech-model" => "The speech model couldn't be loaded. Check Models & keys.",
        _ => return None,
    })
}

/// Build what Home and the sidebar show.
pub fn derive(obs: &Observation, cx: &Context) -> RuntimeView {
    let state = classify(obs);
    let file = match &obs.file {
        FileObs::Present(file) => Some(file),
        _ => None,
    };
    let listening = matches!(
        state,
        State::Listening | State::Recording(_) | State::Processing
    );
    let busy = matches!(state, State::Recording(_) | State::Processing);
    let external_owner = match obs.lock {
        LockProbe::Held(holder) if holder != std::process::id() => Some(holder),
        _ => None,
    };
    let alive =
        matches!(obs.own, OwnEngine::Starting | OwnEngine::Running) || external_owner.is_some();
    let (lead, support, sidebar) = match &state {
        State::Stopped => (
            "LocalFlow isn't running".to_string(),
            "Start it with `localflow` (or `lf run`), or start dictation in this window if it was launched with `--engine`.".to_string(),
            (Signal::Neutral, "Not running".to_string(), String::new()),
        ),
        State::Starting => (
            "Starting…".into(),
            "Waiting for the keyboard listener to be registered.".into(),
            (Signal::Info, "Starting…".into(), String::new()),
        ),
        State::Listening => (
            "Shortcut listening".into(),
            format!(
                "Use {} to dictate. The microphone and speech model are checked when you start.",
                cx.key
            ),
            (Signal::Success, "Shortcut listening".into(), cx.key.clone()),
        ),
        State::Recording(mode) => (
            if mode.is_empty() {
                "Recording…".into()
            } else {
                format!("Recording… ({mode})")
            },
            "Release the shortcut to transcribe.".into(),
            (Signal::Recording, "Recording".into(), mode.to_string()),
        ),
        State::Processing => (
            "Transcribing…".into(),
            "The text is pasted when it's ready.".into(),
            (Signal::Info, "Transcribing…".into(), String::new()),
        ),
        State::Failed(reason) => (
            "Dictation isn't working".into(),
            reason.clone(),
            (Signal::Danger, "Needs attention".into(), short(reason)),
        ),
        State::Stale(seen) => (
            "LocalFlow stopped responding".into(),
            match seen {
                Some(t) => format!("Last seen {}. Restart it.", ago(obs.now, *t)),
                None => "Its status is out of date. Restart it.".into(),
            },
            (
                Signal::Warning,
                "Not responding".into(),
                seen.map(|t| format!("Last seen {}", ago(obs.now, t)))
                    .unwrap_or_default(),
            ),
        ),
        State::Unreadable => (
            "LocalFlow's status can't be read".into(),
            "Its status file is unreadable, so it's unknown whether LocalFlow is running. Restarting LocalFlow rewrites it.".into(),
            (Signal::Warning, "Status unknown".into(), String::new()),
        ),
        State::Elsewhere(pid) => (
            "Another LocalFlow handles dictation".into(),
            match pid {
                Some(pid) => format!("Process {pid} owns dictation but reports no status this window can read, so its shortcut and microphone are unconfirmed."),
                None => "It reports no status this window can read, so its shortcut and microphone are unconfirmed.".into(),
            },
            (Signal::Neutral, "Running elsewhere".into(), "Status limited".into()),
        ),
        State::Unknown => (
            "LocalFlow status limited".into(),
            "A LocalFlow process reports a status this window can't verify, so it's unconfirmed that it owns dictation or that its shortcut and microphone work.".into(),
            (Signal::Neutral, "Status limited".into(), String::new()),
        ),
        State::Simulated => unreachable!("sandbox views come from `simulated`"),
    };
    let (lead, support, sidebar) = if cx.enabled {
        (lead, support, sidebar)
    } else {
        (
            "Dictation is turned off".to_string(),
            "Turn it on to let LocalFlow listen for your shortcut.".to_string(),
            (Signal::Neutral, "Dictation off".to_string(), String::new()),
        )
    };
    let mut warnings: Vec<String> = file
        .map(|f| {
            f.warnings
                .iter()
                .filter_map(|code| warning_text(code))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(tool) = cx.missing_paste_tool {
        warnings.push(format!(
            "Pasting dictated text needs `{tool}`, which isn't installed. Install it with your package manager."
        ));
    }
    if let OwnEngine::OwnedElsewhere(pid) = &obs.own {
        warnings.push(match pid {
            Some(pid) => format!("Another LocalFlow (pid {pid}) is already handling dictation, so this window didn't start a second one."),
            None => "Another LocalFlow is already handling dictation, so this window didn't start a second one.".into(),
        });
    }
    RuntimeView {
        shortcut: shortcut_line(cx.session, listening, &state),
        microphone: microphone_line(
            file.and_then(|f| f.mic_configured.as_deref())
                .filter(|_| listening)
                .unwrap_or(&cx.configured_mic),
            file.and_then(|f| f.mic_observed.as_deref())
                .filter(|_| listening),
        ),
        pid: file.and_then(|f| f.pid).filter(|_| alive),
        external_owner,
        state,
        lead,
        support,
        sidebar,
        warnings,
        listening,
        busy,
        alive,
    }
}

fn short(reason: &str) -> String {
    let mut text: String = reason.chars().take(40).collect();
    if reason.chars().count() > 40 {
        text.push('…');
    }
    text
}

/// The sandbox view for one simulated state (no probe, no file).
pub fn simulated(state: SimulatedState, cx: &Context) -> RuntimeView {
    let now = 1_000_000.0;
    let file = |listener: ListenerFact, state: &str| StatusFile {
        state: Some(state.into()),
        since: Some(now),
        pid: Some(4242),
        lock_identity: Some("4242 simulated".into()),
        protocol: lf_runtime::status::PROTOCOL,
        heartbeat: Some(now),
        listener: Some(listener),
        listener_detail: if listener == ListenerFact::Unsupported {
            lf_input::linux::WAYLAND_UNSUPPORTED.into()
        } else {
            String::new()
        },
        mic_configured: Some(cx.configured_mic.clone()),
        mic_observed: Some("unknown".into()),
        ..StatusFile::default()
    };
    let obs = |file_obs: FileObs, alive: bool| Observation {
        file: file_obs,
        pid_alive: alive,
        lock: LockProbe::Held(4242),
        lock_identity: Some("4242 simulated".into()),
        own_instance: None,
        now,
        own: OwnEngine::None,
    };
    let observation = match state {
        SimulatedState::Example => {
            let mut view = derive(&obs(FileObs::Absent, false), cx);
            view.state = State::Simulated;
            view.lead = "Exploring example data".into();
            view.support = "Dictation isn't connected in the sandbox. Everything you change here stays in the sandbox folder.".into();
            view.sidebar = (
                Signal::Warning,
                "Sandbox".into(),
                "Example data, fake services".into(),
            );
            view.shortcut = (
                Signal::Neutral,
                "Shortcut: no keyboard listener runs in the sandbox.".into(),
            );
            view.microphone = (
                Signal::Neutral,
                "Microphone: never opened in the sandbox.".into(),
            );
            view.warnings.clear();
            view.external_owner = None;
            return view;
        }
        SimulatedState::Stopped => {
            let mut stopped = obs(FileObs::Absent, false);
            stopped.lock = LockProbe::Free;
            stopped
        }
        SimulatedState::Starting => {
            obs(FileObs::Present(file(ListenerFact::Starting, "idle")), true)
        }
        SimulatedState::Listening => obs(FileObs::Present(file(ListenerFact::Ok, "idle")), true),
        SimulatedState::Recording => {
            obs(FileObs::Present(file(ListenerFact::Ok, "recording")), true)
        }
        SimulatedState::Processing => {
            obs(FileObs::Present(file(ListenerFact::Ok, "processing")), true)
        }
        SimulatedState::Failed => obs(
            FileObs::Present(file(ListenerFact::Unsupported, "idle")),
            true,
        ),
        SimulatedState::Stale => {
            let mut stale = obs(FileObs::Present(file(ListenerFact::Ok, "idle")), false);
            stale.lock = LockProbe::Free;
            stale
        }
        SimulatedState::Unknown => {
            let mut legacy = file(ListenerFact::Ok, "idle");
            legacy.protocol = 1;
            legacy.listener = None;
            obs(FileObs::Present(legacy), true)
        }
    };
    let mut view = derive(&observation, cx);
    view.support = format!("{} (simulated)", view.support);
    view.warnings.clear();
    // No real process stands behind a simulated state.
    view.external_owner = None;
    view
}

/// Sandbox review states (Settings → Advanced, sandbox only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulatedState {
    Example,
    Stopped,
    Starting,
    Listening,
    Recording,
    Processing,
    Failed,
    Stale,
    Unknown,
}

impl SimulatedState {
    pub const ALL: [SimulatedState; 9] = [
        SimulatedState::Example,
        SimulatedState::Stopped,
        SimulatedState::Starting,
        SimulatedState::Listening,
        SimulatedState::Recording,
        SimulatedState::Processing,
        SimulatedState::Failed,
        SimulatedState::Stale,
        SimulatedState::Unknown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SimulatedState::Example => "Example data (default)",
            SimulatedState::Stopped => "Not running",
            SimulatedState::Starting => "Starting",
            SimulatedState::Listening => "Shortcut listening",
            SimulatedState::Recording => "Recording",
            SimulatedState::Processing => "Transcribing",
            SimulatedState::Failed => "Needs attention",
            SimulatedState::Stale => "Not responding",
            SimulatedState::Unknown => "Running (status limited)",
        }
    }
}

// ----------------------------------------------------------------- probe ---

/// Whether `pid` is a live process (signal 0; no signal is sent).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: kill with signal 0 only checks that the process exists and
        // may be signalled; nothing is delivered.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        // Unknown on this platform: the heartbeat decides freshness.
        true
    }
}

/// Read the real signals for `root` (blocking file reads; call off the UI
/// thread). Never used in the sandbox.
pub fn observe(root: &DataRoot, own: OwnEngine, own_instance: Option<String>) -> Observation {
    let file = match StatusFile::read(&root.localflow_dir.join("status.json")) {
        Ok(None) => FileObs::Absent,
        Ok(Some(file)) => FileObs::Present(file),
        Err(error) => FileObs::Unreadable(error.to_string()),
    };
    let pid_alive = match &file {
        FileObs::Present(f) => f.pid.is_some_and(pid_alive),
        _ => false,
    };
    let lock_path = root.localflow_dir.join("localflow.lock");
    let lock = lf_runtime::singleinstance::probe_at(&lock_path);
    let lock_identity = std::fs::read_to_string(&lock_path)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    Observation {
        file,
        pid_alive,
        lock,
        lock_identity,
        own_instance,
        now: lf_ui::hub_route::system_now(),
        own,
    }
}

/// The paste helper this session needs and lacks (`xdotool` on X11,
/// `wtype` on Wayland), using `which`.
pub fn missing_paste_tool(session: Session, which: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    match session {
        Session::X11 if !which("xdotool") => Some("xdotool"),
        Session::Wayland if !which("wtype") => Some("wtype"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 2_000_000_000.0;

    fn cx(session: Session) -> Context {
        Context {
            enabled: true,
            key: "Right Option".into(),
            session,
            configured_mic: String::new(),
            missing_paste_tool: None,
        }
    }

    fn v2(listener: Option<ListenerFact>, state: &str, beat: f64) -> StatusFile {
        StatusFile {
            state: Some(state.into()),
            since: Some(beat - 100.0),
            pid: Some(77),
            instance: Some("inst-77".into()),
            lock_identity: Some("77 t1".into()),
            protocol: 2,
            heartbeat: Some(beat),
            listener,
            mic_configured: Some(String::new()),
            mic_observed: Some("unknown".into()),
            ..StatusFile::default()
        }
    }

    fn obs(file: FileObs, alive: bool) -> Observation {
        Observation {
            file,
            pid_alive: alive,
            lock: LockProbe::Held(77),
            lock_identity: Some("77 t1".into()),
            own_instance: None,
            now: NOW,
            own: OwnEngine::None,
        }
    }

    fn all_observations() -> Vec<Observation> {
        let mut out = vec![
            obs(FileObs::Absent, false),
            obs(FileObs::Unreadable("bad".into()), false),
        ];
        let listeners = [
            None,
            Some(ListenerFact::Starting),
            Some(ListenerFact::Ok),
            Some(ListenerFact::Denied),
            Some(ListenerFact::Dead),
            Some(ListenerFact::Unsupported),
        ];
        for listener in listeners {
            for state in [
                "idle",
                "recording",
                "locked",
                "command",
                "processing",
                "stopped",
                "weird",
            ] {
                for beat in [NOW, NOW - 60.0, NOW + 600.0] {
                    for alive in [true, false] {
                        for protocol in [1, 2] {
                            for holder in [
                                LockProbe::Held(77),
                                LockProbe::Held(88),
                                LockProbe::Free,
                                LockProbe::Unknown,
                            ] {
                                let mut file = v2(listener, state, beat);
                                file.protocol = protocol;
                                if protocol == 1 {
                                    file.heartbeat = None;
                                    file.listener = None;
                                }
                                let mut o = obs(FileObs::Present(file), alive);
                                o.lock = holder;
                                out.push(o);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn every_observation_maps_to_one_state_and_ready_wording_is_earned() {
        for session in [
            Session::X11,
            Session::Wayland,
            Session::MacOs,
            Session::Unknown,
        ] {
            for o in all_observations() {
                let view = derive(&o, &cx(session));
                let text = format!(
                    "{} {} {} {} {} {}",
                    view.lead,
                    view.support,
                    view.sidebar.1,
                    view.sidebar.2,
                    view.shortcut.1,
                    view.microphone.1
                );
                assert!(
                    !text.contains("Ready"),
                    "never an unqualified Ready: {text}"
                );
                let earned = match &o.file {
                    FileObs::Present(f) => {
                        f.protocol >= 2
                            && f.listener == Some(ListenerFact::Ok)
                            && f.fresh(NOW)
                            && o.pid_alive
                            && o.lock == LockProbe::Held(77)
                            && f.lock_identity == o.lock_identity
                            && f.state.as_deref() != Some("stopped")
                    }
                    _ => false,
                };
                if text.contains("listening") || text.contains("Listening") {
                    assert!(earned, "listening claimed without proof: {o:?} -> {text}");
                }
                if view.listening {
                    assert!(earned);
                }
                if session == Session::Wayland {
                    assert_eq!(view.shortcut.0, Signal::Warning);
                }
                if matches!(view.state, State::Listening) {
                    assert_eq!(view.sidebar.0, Signal::Success);
                } else {
                    assert_ne!(view.sidebar.0, Signal::Success);
                }
            }
        }
    }

    #[test]
    fn stale_unknown_and_failed_cases() {
        let fresh = |l, s| FileObs::Present(v2(Some(l), s, NOW));
        assert_eq!(
            classify(&obs(fresh(ListenerFact::Ok, "idle"), true)),
            State::Listening
        );
        assert_eq!(
            classify(&obs(fresh(ListenerFact::Ok, "idle"), false)),
            State::Stale(Some(NOW as i64)),
            "dead pid"
        );
        let mut recycled = obs(fresh(ListenerFact::Ok, "idle"), true);
        recycled.lock = LockProbe::Held(99);
        assert!(
            matches!(classify(&recycled), State::Stale(_)),
            "a recycled pid isn't the lock owner"
        );
        // A fresh leftover file + a live (recycled) pid + an unlocked lock
        // file still naming that pid: nobody owns dictation.
        let mut leftover = obs(fresh(ListenerFact::Ok, "idle"), true);
        leftover.lock = LockProbe::Free;
        assert!(matches!(classify(&leftover), State::Stale(_)));
        // Ownership that can't be verified never reads as listening.
        let mut unverified = obs(fresh(ListenerFact::Ok, "idle"), true);
        unverified.lock = LockProbe::Unknown;
        assert_eq!(classify(&unverified), State::Unknown);
        unverified.file = fresh(ListenerFact::Denied, "idle");
        assert!(
            matches!(classify(&unverified), State::Failed(_)),
            "failures still show"
        );
        let old = FileObs::Present(v2(Some(ListenerFact::Ok), "idle", NOW - 60.0));
        assert!(
            matches!(classify(&obs(old, true)), State::Stale(_)),
            "old heartbeat"
        );
        let mut legacy = v2(None, "idle", NOW);
        legacy.protocol = 1;
        legacy.heartbeat = None;
        assert_eq!(
            classify(&obs(FileObs::Present(legacy), true)),
            State::Unknown
        );
        assert_eq!(
            classify(&obs(fresh(ListenerFact::Starting, "idle"), true)),
            State::Starting
        );
        assert!(matches!(
            classify(&obs(fresh(ListenerFact::Denied, "idle"), true)),
            State::Failed(_)
        ));
        assert_eq!(
            classify(&obs(fresh(ListenerFact::Ok, "locked"), true)),
            State::Recording("hands-free")
        );
        assert_eq!(
            classify(&obs(fresh(ListenerFact::Ok, "stopped"), true)),
            State::Stopped
        );
        let mut nobody = obs(FileObs::Absent, false);
        nobody.lock = LockProbe::Free;
        assert_eq!(classify(&nobody), State::Stopped);
    }

    fn own_file(instance: &str) -> StatusFile {
        let mut file = v2(Some(ListenerFact::Ok), "idle", NOW);
        file.pid = Some(std::process::id());
        file.instance = Some(instance.into());
        file
    }

    #[test]
    fn own_starting_ignores_a_previous_fresh_ok_file() {
        let mut o = obs(FileObs::Present(own_file("previous")), true);
        o.lock = LockProbe::Held(std::process::id());
        o.lock_identity = Some("77 t1".into());
        o.own = OwnEngine::Starting;
        assert_eq!(classify(&o), State::Starting);
    }

    #[test]
    fn same_process_restart_waits_for_the_current_instance() {
        let mut o = obs(FileObs::Present(own_file("first-start")), true);
        o.lock = LockProbe::Held(std::process::id());
        o.own = OwnEngine::Running;
        o.own_instance = Some("second-start".into());
        assert_eq!(
            classify(&o),
            State::Starting,
            "an earlier start's ok doesn't count"
        );
        let mut stopped = own_file("first-start");
        stopped.state = Some("stopped".into());
        o.file = FileObs::Present(stopped);
        assert_eq!(
            classify(&o),
            State::Starting,
            "an earlier start's stop doesn't count"
        );
        o.file = FileObs::Present(own_file("second-start"));
        assert_eq!(classify(&o), State::Listening);
    }

    #[test]
    fn current_owned_instance_is_ready_where_the_lock_cant_be_probed() {
        let mut o = obs(FileObs::Present(own_file("live")), true);
        o.lock = LockProbe::Unknown; // macOS: flock owners aren't readable
        o.lock_identity = None;
        o.own = OwnEngine::Running;
        o.own_instance = Some("live".into());
        assert_eq!(classify(&o), State::Listening);
        // Not ours (another window's runtime): ownership stays unverified.
        o.own = OwnEngine::None;
        o.own_instance = None;
        assert_eq!(classify(&o), State::Unknown);
        // A requested start (no live handle yet) proves nothing either.
        o.own = OwnEngine::Running;
        assert_eq!(classify(&o), State::Unknown);
    }

    #[test]
    fn a_mismatching_instance_or_lock_generation_stays_stale_or_limited() {
        // External, Linux: lock held by the file's pid, but re-acquired since.
        let mut o = obs(
            FileObs::Present(v2(Some(ListenerFact::Ok), "idle", NOW)),
            true,
        );
        o.lock_identity = Some("77 t2".into());
        assert!(
            matches!(classify(&o), State::Stale(_)),
            "earlier acquisition"
        );
        o.lock_identity = Some("77 t1".into());
        assert_eq!(classify(&o), State::Listening, "current acquisition");
        let mut no_identity = v2(Some(ListenerFact::Ok), "idle", NOW);
        no_identity.lock_identity = None;
        o.file = FileObs::Present(no_identity);
        assert!(matches!(classify(&o), State::Stale(_)), "unbound file");
        // Our engine runs, but the file is another process's.
        let mut o = obs(
            FileObs::Present(v2(Some(ListenerFact::Ok), "idle", NOW)),
            true,
        );
        o.own = OwnEngine::Running;
        o.own_instance = Some("ours".into());
        assert_eq!(classify(&o), State::Starting);
    }

    #[test]
    fn missing_or_unreadable_status_claims_only_what_is_proven() {
        let base = |file: FileObs, lock: LockProbe| Observation {
            file,
            pid_alive: false,
            lock,
            lock_identity: None,
            own_instance: None,
            now: NOW,
            own: OwnEngine::None,
        };
        let c = cx(Session::X11);
        for lock in [LockProbe::Free, LockProbe::Unknown] {
            // Absent / unreadable with no pid and no provable holder.
            let absent = derive(&base(FileObs::Absent, lock), &c);
            assert_eq!(absent.state, State::Stopped);
            let unreadable = derive(&base(FileObs::Unreadable("bad json".into()), lock), &c);
            assert_eq!(unreadable.state, State::Unreadable);
            assert_eq!(unreadable.lead, "LocalFlow's status can't be read");
            assert_ne!(unreadable.sidebar.1, "Running", "no running claim");
            assert!(!unreadable.alive && unreadable.external_owner.is_none());
        }
        // Our proven runtime hasn't published yet: starting, not "isn't running".
        let mut own = base(FileObs::Absent, LockProbe::Unknown);
        own.own = OwnEngine::Running;
        own.own_instance = Some("live".into());
        assert_eq!(classify(&own), State::Starting);
        own.file = FileObs::Unreadable("partial write".into());
        assert_eq!(classify(&own), State::Starting);
        // A requested start without a live handle proves nothing.
        own.own_instance = None;
        own.file = FileObs::Absent;
        assert_eq!(classify(&own), State::Stopped);
        // Another process provably holds the lock but wrote no status.
        let held = derive(&base(FileObs::Absent, LockProbe::Held(4242)), &c);
        assert_eq!(held.state, State::Elsewhere(Some(4242)));
        assert_eq!(held.external_owner, Some(4242));
        assert!(held.lead.contains("Another LocalFlow"));
        assert!(!held.listening);
        // This window's start was refused by another owner: consistent lead.
        let mut refused = base(FileObs::Absent, LockProbe::Unknown);
        refused.own = OwnEngine::OwnedElsewhere(Some(9));
        let view = derive(&refused, &c);
        assert_eq!(view.state, State::Elsewhere(Some(9)));
        assert!(!view.lead.contains("isn't running"));
    }

    #[test]
    fn unverifiable_status_or_recorded_pids_never_block_start() {
        // Protocol 1 + a live (possibly recycled) pid + an opaque lock.
        let mut legacy = v2(None, "idle", NOW);
        legacy.protocol = 1;
        legacy.heartbeat = None;
        let mut o = obs(FileObs::Present(legacy), true);
        o.lock = LockProbe::Unknown;
        let view = derive(&o, &cx(Session::MacOs));
        assert_eq!(view.state, State::Unknown);
        assert_eq!(view.lead, "LocalFlow status limited");
        assert_eq!(view.external_owner, None, "Start stays possible");
        assert!(!view.alive);
        // A proven holder (Linux kernel lock table) does block it.
        o.lock = LockProbe::Held(77);
        assert_eq!(derive(&o, &cx(Session::X11)).external_owner, Some(77));
        let listening = derive(
            &obs(
                FileObs::Present(v2(Some(ListenerFact::Ok), "idle", NOW)),
                true,
            ),
            &cx(Session::X11),
        );
        assert!(
            listening.support.starts_with("Use Right Option"),
            "{}",
            listening.support
        );
    }

    #[test]
    fn own_engine_starting_and_failure_are_reported_without_a_file() {
        let mut o = obs(FileObs::Absent, false);
        o.own = OwnEngine::Starting;
        assert_eq!(classify(&o), State::Starting);
        o.own = OwnEngine::Failed("model missing".into());
        let view = derive(&o, &cx(Session::X11));
        assert_eq!(view.state, State::Failed("model missing".into()));
        assert_eq!(view.sidebar.0, Signal::Danger);
        // A runtime that reports a fresh listener wins over an old failure.
        let mut later = obs(
            FileObs::Present(v2(Some(ListenerFact::Ok), "idle", NOW)),
            true,
        );
        later.own = OwnEngine::Failed("model missing".into());
        assert_eq!(classify(&later), State::Listening);
        o.own = OwnEngine::OwnedElsewhere(Some(12));
        let view = derive(&o, &cx(Session::X11));
        assert!(view.warnings.iter().any(|w| w.contains("pid 12")));
    }

    #[test]
    fn configured_and_observed_microphone_stay_separate() {
        let mut c = cx(Session::X11);
        c.configured_mic = "USB Mic".into();
        let view = derive(&obs(FileObs::Absent, false), &c);
        assert_eq!(
            view.microphone.1,
            "Microphone: set to “USB Mic” · not checked"
        );
        let mut file = v2(Some(ListenerFact::Ok), "idle", NOW);
        file.mic_configured = Some("USB Mic".into());
        file.mic_observed = Some("missing".into());
        let view = derive(&obs(FileObs::Present(file), true), &c);
        assert!(view.microphone.1.contains("set to “USB Mic”"));
        assert!(view.microphone.1.contains("not found"));
        assert_eq!(view.microphone.0, Signal::Warning);
    }

    #[test]
    fn session_lines_are_honest() {
        let wayland = derive(&obs(FileObs::Absent, false), &cx(Session::Wayland));
        assert_eq!(wayland.shortcut.0, Signal::Warning);
        assert!(wayland.shortcut.1.contains("Wayland"));
        let x11 = derive(&obs(FileObs::Absent, false), &cx(Session::X11));
        assert!(x11.shortcut.1.contains("not confirmed"));
        let listening = derive(
            &obs(
                FileObs::Present(v2(Some(ListenerFact::Ok), "idle", NOW)),
                true,
            ),
            &cx(Session::X11),
        );
        assert_eq!(listening.shortcut.0, Signal::Success);
        let mut off = cx(Session::X11);
        off.enabled = false;
        assert_eq!(
            derive(&obs(FileObs::Absent, false), &off).lead,
            "Dictation is turned off"
        );
        let off_listening = derive(
            &obs(
                FileObs::Present(v2(Some(ListenerFact::Ok), "idle", NOW)),
                true,
            ),
            &off,
        );
        assert_eq!(
            off_listening.sidebar.1, "Dictation off",
            "no listening claim while off"
        );
        assert_ne!(off_listening.sidebar.0, Signal::Success);
        assert_eq!(
            missing_paste_tool(Session::X11, &|_| false),
            Some("xdotool")
        );
        assert_eq!(
            missing_paste_tool(Session::Wayland, &|t| t == "xdotool"),
            Some("wtype")
        );
        assert_eq!(missing_paste_tool(Session::X11, &|t| t == "xdotool"), None);
    }

    #[test]
    fn busy_runtime_blocks_trials_and_downloads() {
        let rec = derive(
            &obs(
                FileObs::Present(v2(Some(ListenerFact::Ok), "recording", NOW)),
                true,
            ),
            &cx(Session::X11),
        );
        assert!(rec.busy && rec.busy_reason().is_some());
        let idle = derive(
            &obs(
                FileObs::Present(v2(Some(ListenerFact::Ok), "idle", NOW)),
                true,
            ),
            &cx(Session::X11),
        );
        assert!(idle.busy_reason().is_none());
    }

    #[test]
    fn simulated_states_cover_every_lead_without_a_probe() {
        let c = cx(Session::X11);
        let leads: Vec<String> = SimulatedState::ALL
            .iter()
            .map(|s| simulated(*s, &c).lead)
            .collect();
        assert_eq!(leads[0], "Exploring example data");
        assert!(leads.contains(&"Shortcut listening".to_string()));
        assert!(leads.contains(&"LocalFlow stopped responding".to_string()));
        assert!(leads.contains(&"Dictation isn't working".to_string()));
    }

    #[test]
    fn pid_liveness() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }
}
