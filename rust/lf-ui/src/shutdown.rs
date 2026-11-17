//! Process-wide bounded shutdown flushing. Register before spawning a writer.

use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct PendingWrites {
    pending: Mutex<usize>,
    idle: Condvar,
}

/// Releases one registered write when dropped, including failed spawns.
pub struct PendingWrite {
    tracker: Arc<PendingWrites>,
}

impl PendingWrites {
    fn register(self: &Arc<Self>) -> PendingWrite {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *pending += 1;
        PendingWrite {
            tracker: self.clone(),
        }
    }

    fn wait(&self, timeout: Duration) -> bool {
        let started = Instant::now();
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while *pending != 0 {
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
                return *pending == 0;
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
        let idle = *pending == 0;
        drop(pending);
        if idle {
            self.tracker.idle.notify_all();
        }
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

/// Register a write before starting its worker; retain the guard until it ends.
pub fn register_pending_write() -> PendingWrite {
    pending_writes().register()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn registration_outlives_individual_owners() {
        let old_window_write = register_pending_write();
        let new_window_write = register_pending_write();
        assert!(!wait_for_pending_writes(Duration::ZERO));
        drop(old_window_write);
        assert!(!wait_for_pending_writes(Duration::ZERO));
        drop(new_window_write);
        assert!(wait_for_pending_writes(Duration::ZERO));
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
