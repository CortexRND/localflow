//! One registry of long-running model jobs (speech downloads, Ollama pulls),
//! shared by setup and Models & keys so a download started in one place is
//! visible (and cancellable) in the other.
//!
//! Each job runs on its own thread with a cancel flag. Progress lands in the
//! registry; views poll it on a timer. One job per key: a second request
//! attaches to the running one. Cancelling sets the flag and the state shows
//! "Cancelling…" until the worker really returns.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use lf_ui::setup_backend::{DownloadProgress, ProgressCallback};

/// No progress for this long: "Still working… check your connection."
pub const STALLED_AFTER: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Running {
        done: u64,
        total: u64,
        status: String,
        last_tick: Instant,
        cancelling: bool,
    },
    Failed(String),
    Cancelled,
    Done,
}

impl JobState {
    pub fn running(&self) -> bool {
        matches!(self, JobState::Running { .. })
    }

    /// Fraction done, `None` while the size is unknown (indeterminate).
    pub fn fraction(&self) -> Option<f32> {
        match self {
            JobState::Running { done, total, .. } if *total > 0 => {
                Some((*done as f64 / *total as f64).clamp(0.0, 1.0) as f32)
            }
            JobState::Done => Some(1.0),
            _ => None,
        }
    }

    /// No progress tick for [`STALLED_AFTER`] at `now`.
    pub fn stalled(&self, now: Instant) -> bool {
        matches!(self, JobState::Running { last_tick, cancelling: false, .. }
            if now.saturating_duration_since(*last_tick) >= STALLED_AFTER)
    }

    /// "312 of 1480 MB" (or the worker's status while the size is unknown).
    pub fn progress_text(&self) -> String {
        match self {
            JobState::Running {
                cancelling: true, ..
            } => "Cancelling…".into(),
            JobState::Running { done, total, .. } if *total > 0 => {
                format!("{} of {} MB", done / 1_000_000, total / 1_000_000)
            }
            JobState::Running { status, .. } if !status.is_empty() => status.clone(),
            JobState::Running { .. } => "Starting…".into(),
            JobState::Failed(error) => error.clone(),
            JobState::Cancelled => "Cancelled.".into(),
            JobState::Done => "Downloaded.".into(),
        }
    }
}

struct Entry {
    state: JobState,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Shared {
    jobs: Mutex<HashMap<String, Entry>>,
    changed: Condvar,
    /// Speech-model deletions in flight (see [`Downloads::begin_delete`]).
    deleting: std::sync::atomic::AtomicUsize,
}

/// Why a speech download didn't start: a model folder is being deleted.
pub const DELETE_IN_PROGRESS: &str =
    "A speech model is being deleted. Try again when that finishes.";

/// Held while a speech-model deletion runs; no speech download can start
/// until it is dropped (and none was running when it was taken).
pub struct DeleteGuard {
    shared: Arc<Shared>,
}

impl Drop for DeleteGuard {
    fn drop(&mut self) {
        let _jobs = self.shared.jobs.lock().unwrap_or_else(|p| p.into_inner());
        self.shared.deleting.fetch_sub(1, Ordering::AcqRel);
        self.shared.changed.notify_all();
    }
}

/// Cheap to clone; every clone sees the same jobs.
#[derive(Clone, Default)]
pub struct Downloads {
    shared: Arc<Shared>,
}

pub type Work = Box<dyn FnOnce(ProgressCallback, Arc<AtomicBool>) -> Result<(), String> + Send>;

/// Turn a long error into one sanitised line (no newlines, bounded).
pub fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(240).collect();
    if flat.chars().count() > 240 {
        out.push('…');
    }
    out
}

impl Downloads {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.shared.jobs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Start `work` under `key` unless it is already running (then the call
    /// attaches and returns `false`).
    pub fn start(&self, key: &str, work: Work) -> bool {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut jobs = self.lock();
            if jobs.get(key).is_some_and(|e| e.state.running()) {
                return false;
            }
            // Decided under the same lock `begin_delete` takes: a download
            // and a model deletion never overlap.
            if key.starts_with("speech:") && self.shared.deleting.load(Ordering::Acquire) > 0 {
                jobs.insert(
                    key.to_string(),
                    Entry {
                        state: JobState::Failed(DELETE_IN_PROGRESS.into()),
                        cancel,
                    },
                );
                return false;
            }
            jobs.insert(
                key.to_string(),
                Entry {
                    state: JobState::Running {
                        done: 0,
                        total: 0,
                        status: String::new(),
                        last_tick: Instant::now(),
                        cancelling: false,
                    },
                    cancel: cancel.clone(),
                },
            );
        }
        let this = self.clone();
        let progress_key = key.to_string();
        let progress_this = self.clone();
        let progress: ProgressCallback = Box::new(move |tick: DownloadProgress| {
            let mut jobs = progress_this.lock();
            if let Some(Entry {
                state:
                    JobState::Running {
                        done,
                        total,
                        status,
                        last_tick,
                        ..
                    },
                ..
            }) = jobs.get_mut(&progress_key)
            {
                *done = tick.done;
                *total = tick.total;
                *status = one_line(&tick.status);
                *last_tick = Instant::now();
            }
        });
        let worker_key = key.to_string();
        let worker_cancel = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("localflow-model-job".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    work(progress, worker_cancel.clone())
                }))
                .unwrap_or_else(|_| Err("The download stopped unexpectedly.".into()));
                this.finish(&worker_key, &worker_cancel, result);
            });
        if spawned.is_err() {
            self.finish(key, &cancel, Err("Couldn't start the download.".into()));
        }
        true
    }

    fn finish(&self, key: &str, cancel: &AtomicBool, result: Result<(), String>) {
        let mut jobs = self.lock();
        if let Some(entry) = jobs.get_mut(key) {
            entry.state = match result {
                Ok(()) => JobState::Done,
                Err(_) if cancel.load(Ordering::Acquire) => JobState::Cancelled,
                Err(error) => JobState::Failed(one_line(&error)),
            };
        }
        drop(jobs);
        self.shared.changed.notify_all();
    }

    pub fn state(&self, key: &str) -> Option<JobState> {
        self.lock().get(key).map(|e| e.state.clone())
    }

    /// Ask a running job to stop; `true` when one was running.
    pub fn cancel(&self, key: &str) -> bool {
        let mut jobs = self.lock();
        match jobs.get_mut(key) {
            Some(Entry {
                state: JobState::Running { cancelling, .. },
                cancel,
            }) => {
                cancel.store(true, Ordering::Release);
                *cancelling = true;
                true
            }
            _ => false,
        }
    }

    /// Cancel every running job; returns how many were running.
    pub fn cancel_all(&self) -> usize {
        let keys = self.running();
        keys.iter().filter(|k| self.cancel(k)).count()
    }

    pub fn running(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .lock()
            .iter()
            .filter(|(_, e)| e.state.running())
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    }

    pub fn any_running(&self) -> bool {
        !self.running().is_empty()
    }

    /// Forget a finished job (its row returns to the idle state).
    pub fn clear(&self, key: &str) {
        let mut jobs = self.lock();
        if jobs.get(key).is_some_and(|e| !e.state.running()) {
            jobs.remove(key);
        }
    }

    /// Start a speech-model deletion: `None` while a speech download runs.
    /// While the guard lives, speech downloads are refused (and shown as
    /// failed with [`DELETE_IN_PROGRESS`]).
    pub fn begin_delete(&self) -> Option<DeleteGuard> {
        let jobs = self.lock();
        if jobs
            .iter()
            .any(|(k, e)| k.starts_with("speech:") && e.state.running())
        {
            return None;
        }
        self.shared.deleting.fetch_add(1, Ordering::AcqRel);
        drop(jobs);
        Some(DeleteGuard {
            shared: self.shared.clone(),
        })
    }

    /// A speech-model deletion is in flight.
    pub fn deleting(&self) -> bool {
        self.shared.deleting.load(Ordering::Acquire) > 0
    }

    /// Wait until nothing runs (downloads and deletions), at most
    /// `timeout`; `true` when idle.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut jobs = self.lock();
        loop {
            if !jobs.values().any(|e| e.state.running())
                && self.shared.deleting.load(Ordering::Acquire) == 0
            {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            jobs = self
                .shared
                .changed
                .wait_timeout(jobs, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
}

/// Registry key of a speech model download.
pub fn speech_key(model_id: &str) -> String {
    format!("speech:{model_id}")
}

/// Registry key of an Ollama pull.
pub fn cleanup_key(model_id: &str) -> String {
    format!("cleanup:{model_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn progress(done: u64, total: u64) -> DownloadProgress {
        DownloadProgress {
            done,
            total,
            status: "Downloading GGML weights".into(),
        }
    }

    #[test]
    fn progress_is_visible_while_running_and_done_at_the_end() {
        let jobs = Downloads::new();
        let (tick, ticked) = mpsc::channel();
        let (go, gate) = mpsc::channel::<()>();
        assert!(jobs.start(
            "speech:fw-base",
            Box::new(move |report, _| {
                report(progress(312_000_000, 1_480_000_000));
                tick.send(()).unwrap();
                gate.recv().unwrap();
                Ok(())
            })
        ));
        ticked.recv_timeout(Duration::from_secs(3)).unwrap();
        let state = jobs.state("speech:fw-base").unwrap();
        assert_eq!(state.progress_text(), "312 of 1480 MB");
        assert!(state.fraction().unwrap() > 0.2);
        assert!(!jobs.start(
            "speech:fw-base",
            Box::new(|_, _| panic!("attach, don't restart"))
        ));
        assert_eq!(jobs.running(), vec!["speech:fw-base".to_string()]);
        go.send(()).unwrap();
        assert!(jobs.wait_idle(Duration::from_secs(3)));
        assert_eq!(jobs.state("speech:fw-base"), Some(JobState::Done));
    }

    #[test]
    fn cancel_shows_cancelling_until_the_worker_returns_then_cancelled() {
        let jobs = Downloads::new();
        let (entered, started) = mpsc::channel();
        assert!(jobs.start(
            "speech:fw-small",
            Box::new(move |_, cancel| {
                entered.send(()).unwrap();
                while !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                std::thread::sleep(Duration::from_millis(30));
                Err("Download cancelled.".into())
            })
        ));
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(jobs.cancel("speech:fw-small"));
        let state = jobs.state("speech:fw-small").unwrap();
        assert!(
            state.running(),
            "not claimed cancelled before the worker stops"
        );
        assert_eq!(state.progress_text(), "Cancelling…");
        assert!(jobs.wait_idle(Duration::from_secs(3)));
        assert_eq!(jobs.state("speech:fw-small"), Some(JobState::Cancelled));
        assert!(!jobs.cancel("speech:fw-small"));
    }

    #[test]
    fn failures_are_one_line_and_retry_starts_again() {
        let jobs = Downloads::new();
        jobs.start(
            "cleanup:x",
            Box::new(|_, _| Err("network\nunreachable  (os error 101)".into())),
        );
        assert!(jobs.wait_idle(Duration::from_secs(3)));
        assert_eq!(
            jobs.state("cleanup:x"),
            Some(JobState::Failed(
                "network unreachable (os error 101)".into()
            ))
        );
        assert!(jobs.start("cleanup:x", Box::new(|_, _| Ok(()))), "retry");
        assert!(jobs.wait_idle(Duration::from_secs(3)));
        jobs.clear("cleanup:x");
        assert_eq!(jobs.state("cleanup:x"), None);
    }

    #[test]
    fn downloads_and_model_deletions_never_overlap() {
        let jobs = Downloads::new();
        let (entered, running) = mpsc::channel();
        let (go, gate) = mpsc::channel::<()>();
        jobs.start(
            &speech_key("fw-small"),
            Box::new(move |_, _| {
                entered.send(()).unwrap();
                gate.recv().unwrap();
                Ok(())
            }),
        );
        running.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(jobs.begin_delete().is_none(), "no delete during a download");
        go.send(()).unwrap();
        assert!(jobs.wait_idle(Duration::from_secs(3)));
        let guard = jobs.begin_delete().unwrap();
        assert!(jobs.deleting());
        // A download requested meanwhile (setup or Models) doesn't run.
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        assert!(!jobs.start(
            &speech_key("fw-base"),
            Box::new(move |_, _| {
                flag.store(true, Ordering::Release);
                Ok(())
            })
        ));
        assert_eq!(
            jobs.state(&speech_key("fw-base")),
            Some(JobState::Failed(DELETE_IN_PROGRESS.into()))
        );
        // Cleanup pulls are unaffected; close waits for the deletion.
        assert!(jobs.start(&cleanup_key("qwen3:4b"), Box::new(|_, _| Ok(()))));
        assert!(
            !jobs.wait_idle(Duration::from_millis(50)),
            "deletion still in flight"
        );
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(guard);
        });
        assert!(
            jobs.wait_idle(Duration::from_secs(3)),
            "woken when it finishes"
        );
        releaser.join().unwrap();
        assert!(!ran.load(Ordering::Acquire));
        assert!(
            jobs.start(&speech_key("fw-base"), Box::new(|_, _| Ok(()))),
            "retry works"
        );
    }

    #[test]
    fn stalled_is_derived_from_the_last_tick() {
        let start = Instant::now();
        let running = JobState::Running {
            done: 1,
            total: 10,
            status: String::new(),
            last_tick: start,
            cancelling: false,
        };
        assert!(!running.stalled(start + Duration::from_secs(29)));
        assert!(running.stalled(start + STALLED_AFTER));
        assert_eq!(
            JobState::Running {
                done: 0,
                total: 0,
                status: String::new(),
                last_tick: start,
                cancelling: false
            }
            .fraction(),
            None,
            "unknown size is indeterminate"
        );
    }

    #[test]
    fn cancel_all_and_a_panicking_worker() {
        let jobs = Downloads::new();
        // The panicking job finishes first (wait_idle is the handshake), then
        // the long job is confirmed running before cancel_all: no timing.
        jobs.start("speech:b", Box::new(|_, _| panic!("boom")));
        assert!(jobs.wait_idle(Duration::from_secs(3)));
        assert!(matches!(jobs.state("speech:b"), Some(JobState::Failed(_))));
        let (entered, running) = mpsc::channel();
        jobs.start(
            "speech:a",
            Box::new(move |_, cancel| {
                entered.send(()).unwrap();
                while !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err("cancelled".into())
            }),
        );
        running.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(jobs.cancel_all(), 1, "only the running job counts");
        assert!(jobs.wait_idle(Duration::from_secs(3)));
        assert_eq!(jobs.state("speech:a"), Some(JobState::Cancelled));
        assert!(matches!(jobs.state("speech:b"), Some(JobState::Failed(_))));
    }
}
