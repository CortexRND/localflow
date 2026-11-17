//! Process-wide bounded shutdown flushing. Register before spawning a writer.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct PendingWrites {
    pending: Mutex<usize>,
    meetings: AtomicUsize,
    idle: Condvar,
}

/// Releases one registered write when dropped, including failed spawns.
pub struct PendingWrite {
    tracker: Arc<PendingWrites>,
    meeting: bool,
}

impl PendingWrites {
    fn register(self: &Arc<Self>) -> PendingWrite {
        self.register_kind(false)
    }

    fn register_kind(self: &Arc<Self>, meeting: bool) -> PendingWrite {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *pending += 1;
        if meeting {
            self.meetings.fetch_add(1, Ordering::AcqRel);
        }
        PendingWrite {
            tracker: self.clone(),
            meeting,
        }
    }

    fn wait(&self, timeout: Duration) -> bool {
        self.wait_until(timeout, 0)
    }

    fn wait_for_quit(&self, normal: Duration, cap: Duration) -> bool {
        let started = Instant::now();
        if self.wait(normal.min(cap)) {
            return true;
        }
        let remaining = if self.meetings.load(Ordering::Acquire) > 0 {
            cap.saturating_sub(started.elapsed())
        } else {
            Duration::ZERO
        };
        self.wait(remaining)
    }

    fn wait_until(&self, timeout: Duration, allowed: usize) -> bool {
        let started = Instant::now();
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while *pending > allowed {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return false;
            }
            let (next, result) = self
                .idle
                .wait_timeout(pending, remaining)
                .unwrap_or_else(|error| error.into_inner());
            pending = next;
            if result.timed_out() {
                return *pending <= allowed;
            }
        }
        true
    }
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        let mut pending = self
            .tracker
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *pending -= 1;
        if self.meeting {
            self.tracker.meetings.fetch_sub(1, Ordering::AcqRel);
        }
        drop(pending);
        self.tracker.idle.notify_all();
    }
}

#[cfg(feature = "runtime")]
impl PendingWrite {
    pub(crate) fn wait_for_others(&self, timeout: Duration) -> bool {
        self.tracker.wait_until(timeout, 1)
    }
}

fn pending_writes() -> Arc<PendingWrites> {
    static WRITES: OnceLock<Arc<PendingWrites>> = OnceLock::new();
    WRITES
        .get_or_init(|| Arc::new(PendingWrites::default()))
        .clone()
}

/// Wait for all writes registered so far, never longer than `timeout`.
pub fn wait_for_pending_writes(timeout: Duration) -> bool {
    pending_writes().wait(timeout)
}

pub fn wait_for_quit(normal: Duration, cap: Duration) -> bool {
    pending_writes().wait_for_quit(normal, cap)
}

#[cfg(feature = "runtime")]
pub(crate) fn register_pending_meeting_write() -> PendingWrite {
    pending_writes().register_kind(true)
}

/// Register a write before starting its worker; retain the guard until it ends.
pub fn register_pending_write() -> PendingWrite {
    pending_writes().register()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn meeting_flush_extends_normal_budget_but_never_exceeds_hard_cap() {
        let writes = Arc::new(PendingWrites::default());
        let meeting = writes.register_kind(true);
        let waiter = writes.clone();
        let (ready, waiting) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            ready.send(()).unwrap();
            finished
                .send(waiter.wait_for_quit(Duration::ZERO, Duration::from_secs(5)))
                .unwrap();
        });
        waiting.recv().unwrap();
        assert!(done.recv_timeout(Duration::from_millis(20)).is_err());
        drop(meeting);
        assert!(done.recv_timeout(Duration::from_secs(5)).unwrap());
        thread.join().unwrap();
        let _meeting = writes.register_kind(true);
        let started = Instant::now();
        assert!(!writes.wait_for_quit(Duration::ZERO, Duration::from_millis(20)));
        assert!(started.elapsed() < Duration::from_secs(1));
        let ordinary = Arc::new(PendingWrites::default());
        let _write = ordinary.register();
        assert!(!ordinary.wait_for_quit(Duration::ZERO, Duration::from_secs(5)));
    }

    #[test]
    fn registration_outlives_individual_owners() {
        let writes = Arc::new(PendingWrites::default());
        let old_window_write = writes.register();
        let new_window_write = writes.register();
        assert!(!writes.wait(Duration::ZERO));
        drop(old_window_write);
        assert!(!writes.wait(Duration::ZERO));
        drop(new_window_write);
        assert!(writes.wait(Duration::ZERO));
    }

    #[test]
    fn empty_and_completed_writes_are_idle() {
        let writes = Arc::new(PendingWrites::default());
        assert!(writes.wait(Duration::ZERO));
        let guard = writes.register();
        assert!(!writes.wait(Duration::ZERO));
        drop(guard);
        assert!(writes.wait(Duration::ZERO));
    }

    #[test]
    fn registered_workers_are_waited_on_and_timeout_is_bounded() {
        let writes = Arc::new(PendingWrites::default());
        let guard = writes.register();
        let start = Instant::now();
        assert!(!writes.wait(Duration::from_millis(20)));
        assert!(start.elapsed() < Duration::from_secs(1));
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            wait.recv().unwrap();
            drop(guard);
        });
        release.send(()).unwrap();
        assert!(writes.wait(Duration::from_secs(1)));
        worker.join().unwrap();
    }

    #[test]
    fn failed_worker_unwinds_release_the_guard() {
        let writes = Arc::new(PendingWrites::default());
        let guard = writes.register();
        assert!(std::thread::spawn(move || {
            let _guard = guard;
            panic!("write failed");
        })
        .join()
        .is_err());
        assert!(writes.wait(Duration::ZERO));
    }
}
