//! The optional in-window dictation engine (`--features runtime` +
//! `--engine` + an explicit Start on Home).
//!
//! The engine is `lf_ui::embedded::EmbeddedRuntime` (server + dictation on
//! one shared speech engine), not a parallel implementation. It lives on one
//! owner thread; the UI sends Start/Stop and reads the state. Before
//! starting, the dictation lock (`localflow.lock`) is checked: when another
//! LocalFlow owns dictation nothing is started, nothing is stolen, and that
//! runtime is never stopped by this window. Only what this window started is
//! shut down, with a bounded wait on close.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::controllers::runtime_state::OwnEngine;

/// How long closing the window waits for the engine to stop.
pub const STOP_WAIT: Duration = Duration::from_secs(5);

pub enum Started {
    /// A runtime is running in this process; `instance` is the status-file
    /// instance its live `RuntimeHandle` reports (the ownership proof).
    Running { instance: String },
    /// Another process holds the dictation lock (pid when known).
    OwnedElsewhere(Option<u32>),
}

/// What the owner thread drives.
pub trait EngineBackend: Send + 'static {
    fn start(&mut self) -> Result<Started, String>;
    fn stop(&mut self);
}

enum Command {
    Start,
    Stop,
    Shutdown,
}

pub struct Engine {
    commands: Sender<Command>,
    state: Arc<Mutex<OwnEngine>>,
    /// The running runtime's status instance (None unless running).
    instance: Arc<Mutex<Option<String>>>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    /// Set once the owner thread is gone (joined), with how it ended.
    ended: Option<Stopped>,
}

/// How a shutdown request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// The owner thread confirmed it stopped everything and was joined.
    Done,
    /// Still stopping after the wait; the engine keeps its thread and can
    /// be waited on again. Nothing was released on its behalf.
    Pending,
    /// The owner thread died without confirming (a panic while stopping):
    /// cleanup is NOT confirmed.
    Crashed,
}

impl Engine {
    pub fn spawn(backend: impl EngineBackend) -> std::io::Result<Self> {
        let (commands, inbox) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let state = Arc::new(Mutex::new(OwnEngine::None));
        let shared = state.clone();
        let instance = Arc::new(Mutex::new(None::<String>));
        let shared_instance = instance.clone();
        let thread = std::thread::Builder::new()
            .name("localflow-desktop-engine".into())
            .spawn(move || {
                let mut backend = backend;
                let mut running = false;
                let set = |s: OwnEngine| *shared.lock().unwrap_or_else(|p| p.into_inner()) = s;
                let set_instance = |i: Option<String>| {
                    *shared_instance.lock().unwrap_or_else(|p| p.into_inner()) = i
                };
                while let Ok(command) = inbox.recv() {
                    match command {
                        Command::Start if !running => {
                            set(OwnEngine::Starting);
                            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                backend.start()
                            })) {
                                Ok(Ok(Started::Running { instance })) => {
                                    running = true;
                                    set_instance(Some(instance));
                                    set(OwnEngine::Running);
                                }
                                Ok(Ok(Started::OwnedElsewhere(pid))) => {
                                    set(OwnEngine::OwnedElsewhere(pid))
                                }
                                Ok(Err(error)) => set(OwnEngine::Failed(
                                    crate::controllers::jobs::one_line(&error),
                                )),
                                Err(_) => set(OwnEngine::Failed(
                                    "The dictation engine crashed while starting.".into(),
                                )),
                            }
                        }
                        Command::Start => {}
                        Command::Stop => {
                            if running {
                                set_instance(None);
                                set(OwnEngine::Stopping);
                                backend.stop();
                                running = false;
                            }
                            set(OwnEngine::None);
                        }
                        Command::Shutdown => break,
                    }
                }
                if running {
                    set_instance(None);
                    set(OwnEngine::Stopping);
                    backend.stop();
                }
                set(OwnEngine::None);
                let _ = finished.send(());
            })?;
        Ok(Engine {
            commands,
            state,
            instance,
            done,
            thread: Some(thread),
            ended: None,
        })
    }

    /// The running runtime's status-file instance (proof that a status file
    /// is this window's current runtime). None while starting or stopped.
    pub fn instance(&self) -> Option<String> {
        if self.ended.is_some() {
            return None;
        }
        self.instance
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn state(&self) -> OwnEngine {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Start pressed (idempotent while running).
    pub fn start(&self) {
        let _ = self.commands.send(Command::Start);
    }

    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    /// Whether this window's engine is doing anything a close would stop.
    /// An engine whose stop crashed counts too: its cleanup is unconfirmed.
    pub fn active(&self) -> bool {
        self.ended == Some(Stopped::Crashed)
            || (self.ended.is_none()
                && matches!(
                    self.state(),
                    OwnEngine::Starting | OwnEngine::Running | OwnEngine::Stopping
                ))
    }

    /// Stop what this window started and wait at most `timeout` (call it
    /// off the UI thread). Only a confirmation from the owner thread counts
    /// as [`Stopped::Done`]; a timeout keeps the thread for another wait.
    pub fn shutdown(&mut self, timeout: Duration) -> Stopped {
        if let Some(ended) = self.ended {
            return ended;
        }
        let _ = self.commands.send(Command::Shutdown);
        let outcome = match self.done.recv_timeout(timeout) {
            Ok(()) => Stopped::Done,
            Err(mpsc::RecvTimeoutError::Timeout) => return Stopped::Pending,
            Err(mpsc::RecvTimeoutError::Disconnected) => Stopped::Crashed,
        };
        let joined = self.thread.take().map(JoinHandle::join);
        let outcome = match (outcome, joined) {
            (Stopped::Done, Some(Ok(()))) | (Stopped::Done, None) => Stopped::Done,
            _ => Stopped::Crashed,
        };
        self.ended = Some(outcome);
        outcome
    }
}

impl Drop for Engine {
    /// Never blocks: asks the owner thread to stop. Closing the window
    /// waits (bounded, off the UI thread) through [`Engine::shutdown`]
    /// before it gets here; a forced quit doesn't wait at all.
    fn drop(&mut self) {
        if self.ended.is_none() {
            let _ = self.commands.send(Command::Shutdown);
        }
    }
}

/// `Some(pid)` when the dictation lock file at `path` names another live
/// process. Read-only: the lock is never taken here, so a runtime starting
/// at the same moment can't be refused because of this probe. The
/// authoritative check is the engine's own lock acquisition (activation
/// then reports "owned by another process").
pub fn lock_owner(path: &std::path::Path) -> Option<u32> {
    owner_from_probe(lf_runtime::singleinstance::probe_at(path))
}

/// Only a kernel-proven holder other than this process counts. Where the
/// platform can't tell (macOS), a pid merely recorded in the lock file —
/// possibly a recycled one — must not block Start forever: the canonical
/// activation then takes the free lock, or reports the real owner.
pub fn owner_from_probe(probe: lf_runtime::singleinstance::LockProbe) -> Option<u32> {
    use lf_runtime::singleinstance::LockProbe;
    match probe {
        LockProbe::Held(pid) => (pid != std::process::id()).then_some(pid),
        LockProbe::Free | LockProbe::Unknown => None,
    }
}

/// The real engine (`lf_ui::embedded::EmbeddedRuntime`) for the user's own
/// LocalFlow data. Never constructed in the sandbox.
#[cfg(feature = "runtime")]
pub struct EmbeddedBackend {
    runtime: Option<lf_ui::embedded::EmbeddedRuntime>,
    lock_path: std::path::PathBuf,
    config_path: std::path::PathBuf,
}

#[cfg(feature = "runtime")]
impl EmbeddedBackend {
    pub fn new(root: &crate::data::DataRoot) -> Result<Self, String> {
        if root.is_sandbox() {
            return Err("The sandbox never starts a dictation engine.".into());
        }
        // EmbeddedRuntime works on LocalFlow's own locations; refuse a root
        // that isn't them instead of mixing data sets.
        let expected = lf_core::config::try_config_path().map_err(|e| e.to_string())?;
        if expected != root.config_path || lf_core::home::localflow_dir() != root.localflow_dir {
            return Err("The engine only runs on LocalFlow's own data folders.".into());
        }
        Ok(EmbeddedBackend {
            runtime: None,
            lock_path: lf_runtime::singleinstance::lock_file_path("localflow"),
            config_path: root.config_path.clone(),
        })
    }
}

#[cfg(feature = "runtime")]
impl EngineBackend for EmbeddedBackend {
    fn start(&mut self) -> Result<Started, String> {
        if let Some(pid) = lock_owner(&self.lock_path) {
            return Ok(Started::OwnedElsewhere(Some(pid)));
        }
        let config = lf_core::config::load_config().map_err(|e| format!("{e:#}"))?;
        let enabled = config.field_bool("dictation_enabled").unwrap_or(true);
        let menu = lf_runtime::menubar::MenuState::new(None, enabled);
        let mut runtime =
            lf_ui::embedded::EmbeddedRuntime::start_server(config.clone(), menu.clone());
        let path = self.config_path.clone();
        let toggle: lf_runtime::run::ToggleStore = Arc::new(move || {
            lf_runtime::settings::toggle_dictation(&path).map_err(|e| e.to_string())
        });
        if !runtime.activate(config, toggle) {
            let reason = menu
                .snapshot()
                .input_warning
                .unwrap_or_else(|| "Dictation couldn't start.".into());
            runtime.shutdown();
            return Err(reason);
        }
        if runtime.runtime.is_none() {
            // Someone else took the lock between our check and activation.
            runtime.shutdown();
            return Ok(Started::OwnedElsewhere(
                lf_runtime::singleinstance::read_holder_at(&self.lock_path).0,
            ));
        }
        // The live handle's own reporter instance is the proof; without it
        // (shut down underneath us) this is not a running runtime.
        let Some(instance) = runtime.runtime.as_ref().and_then(|r| r.status_instance()) else {
            runtime.shutdown();
            return Err("Dictation stopped while starting.".into());
        };
        self.runtime = Some(runtime);
        Ok(Started::Running { instance })
    }

    fn stop(&mut self) {
        if let Some(mut runtime) = self.runtime.take() {
            runtime.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    struct Counts(Arc<(AtomicUsize, AtomicUsize)>);

    struct Fake {
        counts: Counts,
        result: fn() -> Result<Started, String>,
        stop_delay: Duration,
    }

    impl EngineBackend for Fake {
        fn start(&mut self) -> Result<Started, String> {
            self.counts.0 .0.fetch_add(1, Ordering::SeqCst);
            (self.result)()
        }
        fn stop(&mut self) {
            std::thread::sleep(self.stop_delay);
            self.counts.0 .1.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn wait_for(engine: &Engine, want: fn(&OwnEngine) -> bool) -> OwnEngine {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let state = engine.state();
            if want(&state) {
                return state;
            }
            assert!(std::time::Instant::now() < deadline, "stuck at {state:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn nothing_starts_until_asked_and_only_what_started_is_stopped() {
        let counts = Counts::default();
        let mut engine = Engine::spawn(Fake {
            counts: counts.clone(),
            result: || {
                Ok(Started::Running {
                    instance: "fake-instance".into(),
                })
            },
            stop_delay: Duration::ZERO,
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(engine.state(), OwnEngine::None);
        assert_eq!(counts.0 .0.load(Ordering::SeqCst), 0, "no implicit start");
        engine.start();
        engine.start();
        wait_for(&engine, |s| *s == OwnEngine::Running);
        assert_eq!(counts.0 .0.load(Ordering::SeqCst), 1, "idempotent");
        assert_eq!(engine.instance().as_deref(), Some("fake-instance"));
        engine.stop();
        wait_for(&engine, |s| *s == OwnEngine::None);
        assert_eq!(engine.instance(), None, "no proof once stopped");
        assert_eq!(engine.shutdown(Duration::from_secs(3)), Stopped::Done);
        assert_eq!(counts.0 .1.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn another_owner_is_left_alone() {
        let counts = Counts::default();
        let mut engine = Engine::spawn(Fake {
            counts: counts.clone(),
            result: || Ok(Started::OwnedElsewhere(Some(4321))),
            stop_delay: Duration::ZERO,
        })
        .unwrap();
        engine.start();
        assert_eq!(
            wait_for(&engine, |s| matches!(s, OwnEngine::OwnedElsewhere(_))),
            OwnEngine::OwnedElsewhere(Some(4321))
        );
        assert!(!engine.active());
        assert_eq!(engine.shutdown(Duration::from_secs(3)), Stopped::Done);
        assert_eq!(
            counts.0 .1.load(Ordering::SeqCst),
            0,
            "never stops a runtime it didn't start"
        );
    }

    #[test]
    fn start_failure_is_visible_and_retryable() {
        let counts = Counts::default();
        let engine = Engine::spawn(Fake {
            counts: counts.clone(),
            result: || Err("Speech model unavailable".into()),
            stop_delay: Duration::ZERO,
        })
        .unwrap();
        engine.start();
        assert_eq!(
            wait_for(&engine, |s| matches!(s, OwnEngine::Failed(_))),
            OwnEngine::Failed("Speech model unavailable".into())
        );
        engine.start();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(counts.0 .0.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn close_waits_a_bounded_time_and_reports_a_slow_stop() {
        let mut engine = Engine::spawn(Fake {
            counts: Counts::default(),
            result: || {
                Ok(Started::Running {
                    instance: "fake-instance".into(),
                })
            },
            stop_delay: Duration::from_millis(400),
        })
        .unwrap();
        engine.start();
        wait_for(&engine, |s| *s == OwnEngine::Running);
        let before = std::time::Instant::now();
        assert_eq!(
            engine.shutdown(Duration::from_millis(50)),
            Stopped::Pending,
            "reported as unfinished"
        );
        assert!(before.elapsed() < Duration::from_millis(300));
        assert!(engine.active(), "still owned and still stopping");
        // The caller retries; the retained thread confirms later.
        assert_eq!(engine.shutdown(Duration::from_secs(3)), Stopped::Done);
        assert_eq!(engine.shutdown(Duration::ZERO), Stopped::Done, "idempotent");
    }

    #[test]
    fn dropping_never_blocks_and_a_panic_while_stopping_is_not_success() {
        let slow = Engine::spawn(Fake {
            counts: Counts::default(),
            result: || {
                Ok(Started::Running {
                    instance: "fake-instance".into(),
                })
            },
            stop_delay: Duration::from_millis(400),
        })
        .unwrap();
        slow.start();
        wait_for(&slow, |s| *s == OwnEngine::Running);
        let dropped = std::time::Instant::now();
        drop(slow);
        assert!(
            dropped.elapsed() < Duration::from_millis(200),
            "drop doesn't wait"
        );

        struct PanicOnStop;
        impl EngineBackend for PanicOnStop {
            fn start(&mut self) -> Result<Started, String> {
                Ok(Started::Running {
                    instance: "fake-instance".into(),
                })
            }
            fn stop(&mut self) {
                panic!("engine stop failed");
            }
        }
        let mut engine = Engine::spawn(PanicOnStop).unwrap();
        engine.start();
        wait_for(&engine, |s| *s == OwnEngine::Running);
        assert_eq!(engine.shutdown(Duration::from_secs(3)), Stopped::Crashed);
        assert_eq!(
            engine.shutdown(Duration::ZERO),
            Stopped::Crashed,
            "stays unconfirmed"
        );
        assert!(engine.active(), "a later close still warns about it");
    }

    #[test]
    fn only_a_proven_other_holder_blocks_start() {
        use lf_runtime::singleinstance::LockProbe;
        assert_eq!(owner_from_probe(LockProbe::Held(4242)), Some(4242));
        assert_eq!(owner_from_probe(LockProbe::Held(std::process::id())), None);
        assert_eq!(owner_from_probe(LockProbe::Free), None);
        assert_eq!(
            owner_from_probe(LockProbe::Unknown),
            None,
            "an opaque lock with a stale/recycled recorded pid lets activation decide"
        );
    }

    #[test]
    fn lock_owner_is_read_only_and_ignores_dead_and_own_pids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("localflow.lock");
        assert_eq!(lock_owner(&path), None, "no lock file");
        std::fs::write(&path, "999999999 2020-01-01T00:00:00+00:00\n").unwrap();
        assert_eq!(lock_owner(&path), None, "a dead holder doesn't count");
        std::fs::write(&path, format!("{} now\n", std::process::id())).unwrap();
        assert_eq!(lock_owner(&path), None, "this process isn't another owner");
        std::fs::write(&path, "1 now\n").unwrap();
        if cfg!(target_os = "linux") {
            // The kernel says nobody holds it: a live pid merely recorded
            // in a stale file isn't an owner.
            assert_eq!(lock_owner(&path), None);
        } else {
            // Unverifiable (macOS): a recorded live pid doesn't block Start.
            assert_eq!(lock_owner(&path), None);
        }
        // The probe never takes the lock; our own hold isn't "another owner".
        let _held = lf_runtime::singleinstance::acquire_at(&path)
            .unwrap()
            .unwrap();
        assert_eq!(lock_owner(&path), None);
    }
}
