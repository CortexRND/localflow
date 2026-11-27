//! What the two `/changes` readers share: the Agents page's live list and
//! the shell's needs-input watcher (sidebar badge). The pauses, the
//! failure backoff, running a blocking read (a held long-poll on its own
//! thread), and the [`ChangesGate`] that keeps at most one `/changes`
//! call open between them.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use crate::controllers::agents::AgentsError;

/// Pause between `/changes` reads that return at once (the sandbox, or a
/// server that didn't hold the request).
pub const POLL_PAUSE: Duration = Duration::from_secs(2);
/// The shortest pause even after a held long-poll, so a server that
/// answers `waited: true` at once can't spin the loop.
pub const MIN_PAUSE: Duration = Duration::from_millis(250);
/// Failures back off from [`POLL_PAUSE`], doubling, to this. Short enough
/// that the 20 s grace period still ends in Disconnected on time.
pub const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// Long-poll length asked of a real server.
pub const LONG_POLL: u32 = crate::controllers::agents_client::MAX_WAIT;

/// The pause after `failures` failed reads in a row: 2, 4, 8, then 10 s.
pub fn backoff(failures: u32) -> Duration {
    POLL_PAUSE
        .saturating_mul(1 << failures.saturating_sub(1).min(8))
        .min(MAX_BACKOFF)
}

/// Run a blocking read: on its own thread when it may be held (a
/// long-poll), else on the background executor.
pub async fn blocking<T: Send + 'static>(
    cx: &mut gpui::AsyncApp,
    held: bool,
    f: impl FnOnce() -> Result<T, AgentsError> + Send + 'static,
) -> Result<T, AgentsError> {
    if !held {
        return cx.background_executor().spawn(async move { f() }).await;
    }
    let (tx, rx) = async_channel::bounded(1);
    let spawned = std::thread::Builder::new()
        .name("agents-poll".into())
        .spawn(move || {
            let _ = tx.send_blocking(f());
        });
    match spawned {
        Ok(_) => rx.recv().await.unwrap_or(Err(AgentsError::Offline)),
        Err(_) => Err(AgentsError::Offline),
    }
}

/// At most one `/changes` call open per window. The page and the shell
/// each take a [`ChangesTicket`] before calling and wait while the other
/// holds one. Waiting costs no latency: a held call answers as soon as
/// anything changes. UI-thread only.
#[derive(Clone, Debug, Default)]
pub struct ChangesGate(Rc<Cell<u32>>);

impl ChangesGate {
    /// A ticket while no call is open; `None` while one is.
    pub fn enter(&self) -> Option<ChangesTicket> {
        if self.0.get() > 0 {
            return None;
        }
        self.0.set(1);
        Some(ChangesTicket(self.0.clone()))
    }

    /// `/changes` calls open now (0 or 1).
    pub fn open(&self) -> u32 {
        self.0.get()
    }
}

/// One open `/changes` call; dropping it (the call returned, or its loop
/// went away mid-call) lets the next one start.
#[derive(Debug)]
pub struct ChangesTicket(Rc<Cell<u32>>);

impl Drop for ChangesTicket {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_its_cap() {
        let secs: Vec<u64> = (1..=6).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 10, 10, 10]);
    }

    #[test]
    fn the_gate_admits_one_call_at_a_time() {
        let gate = ChangesGate::default();
        let shared = gate.clone();
        let ticket = gate.enter().expect("idle");
        assert_eq!(shared.open(), 1);
        assert!(shared.enter().is_none(), "the other loop waits");
        drop(ticket);
        assert_eq!(gate.open(), 0);
        assert!(shared.enter().is_some());
    }
}
