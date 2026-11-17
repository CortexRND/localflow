use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn request_quit(_signal: libc::c_int) {
    REQUESTED.store(true, Ordering::Relaxed);
}

pub(crate) struct SignalHandlers {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl SignalHandlers {
    pub(crate) fn install() -> io::Result<Self> {
        let mut handlers = Self { previous: vec![] };
        for signal in [libc::SIGTERM, libc::SIGINT] {
            // SAFETY: zeroed sigactions are initialized before use; the handler
            // only stores to a lock-free atomic, never calling AppKit here.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = request_quit as *const () as usize;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                let mut previous = std::mem::zeroed();
                if libc::sigaction(signal, &action, &mut previous) != 0 {
                    return Err(io::Error::last_os_error());
                }
                handlers.previous.push((signal, previous));
            }
        }
        Ok(handlers)
    }
}

impl Drop for SignalHandlers {
    fn drop(&mut self) {
        for (signal, previous) in self.previous.iter().rev() {
            // SAFETY: restore the handlers saved by successful sigaction calls.
            unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) };
        }
    }
}

pub(crate) fn poll(terminate: impl FnOnce()) {
    if REQUESTED.swap(false, Ordering::Relaxed) {
        terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_request_graceful_quit_once() {
        if std::env::var_os("LF_TEST_TERMINATION_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "termination::tests::signals_request_graceful_quit_once",
                ])
                .env("LF_TEST_TERMINATION_CHILD", "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        let _handlers = SignalHandlers::install().unwrap();
        for signal in [libc::SIGTERM, libc::SIGINT] {
            let mut quits = 0;
            poll(|| quits += 1);
            assert_eq!(quits, 0);
            // SAFETY: the child process installed its own handlers above.
            assert_eq!(unsafe { libc::raise(signal) }, 0);
            assert_eq!(quits, 0);
            poll(|| quits += 1);
            poll(|| quits += 1);
            assert_eq!(quits, 1);
        }
    }
}
