//! History → Retry with the Python app's speech model.
//!
//! Users whose dictation still runs on the Python runtime have its model
//! (faster-whisper in the Hugging Face cache), not the native one. When the
//! native model isn't downloaded, the Python `lf` that owns this install
//! transcribes the saved clip with its own pipeline (`lf history retry <id>
//! --json`): offline, with this window's config and data directory, printing
//! the result as one JSON line and changing nothing. The caller commits it
//! exactly like a native result.
//!
//! The Python `lf` is the interpreter the `lf` launcher exports in
//! [`PYTHON_ENV`] (it is the install that opened this window), else an
//! absolute `lf` on `PATH`, else (Unix) `~/.local/bin/lf`. The child is killed and reaped on timeout, on
//! [`PythonRetry::stop`] (window close) and if the worker unwinds.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::data::DataRoot;

/// Set by the Python `lf` launcher to its own interpreter.
pub const PYTHON_ENV: &str = "LOCALFLOW_PYTHON";
/// Model load plus inference of one saved clip, generously.
pub const TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// `lf history retry --json`: the Python app's model isn't downloaded.
const NO_MODEL_EXIT: i32 = 3;
/// Click's (and `lf-rs`'s) usage error: an `lf` without `--json`.
const USAGE_EXIT: i32 = 2;
const MAX_OUTPUT: u64 = 1 << 20;
const MAX_MESSAGE_CHARS: usize = 160;
const POLL: Duration = Duration::from_millis(25);

#[cfg(windows)]
const LF_NAME: &str = "lf.exe";
#[cfg(not(windows))]
const LF_NAME: &str = "lf";

/// Why the Python app gave no result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PythonError {
    /// No Python `lf` was found.
    Missing,
    /// The `lf` found can't do this (`lf-rs`, an older Python app, …).
    Unsupported,
    /// The Python app's configured model isn't downloaded either.
    NoModel,
    /// It ran and failed; one short sentence.
    Failed(String),
}

/// What the Python app made of the row it read.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub id: i64,
    pub ts: f64,
    pub raw_text: String,
    pub final_text: String,
}

pub type Env = dyn Fn(&str) -> Option<OsString> + Send + Sync;

pub struct PythonRetry {
    env: Box<Env>,
    timeout: Duration,
    /// The running child's stop flag.
    running: Mutex<Option<Arc<AtomicBool>>>,
}

impl PythonRetry {
    /// This process's environment and [`TIMEOUT`].
    pub fn system() -> Self {
        Self::with(Box::new(|name| std::env::var_os(name)), TIMEOUT)
    }

    pub fn with(env: Box<Env>, timeout: Duration) -> Self {
        PythonRetry {
            env,
            timeout,
            running: Mutex::new(None),
        }
    }

    /// Kill a running child (it has written nothing). Safe from any thread.
    pub fn stop(&self) {
        if let Some(flag) = self
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// The argv prefix that runs the Python `lf`, if one is found.
    pub fn locate(&self) -> Option<Vec<OsString>> {
        let env = &self.env;
        if let Some(python) = env(PYTHON_ENV)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_file())
        {
            return Some(vec![python.into(), "-m".into(), "localflow.cli".into()]);
        }
        let path = env("PATH").unwrap_or_default();
        let on_path = std::env::split_paths(&path).map(|dir| dir.join(LF_NAME));
        // install.sh links the Python CLI here; a desktop-entry launch's
        // session PATH often lacks it.
        #[cfg(unix)]
        let on_path = on_path.chain(
            env("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/bin").join(LF_NAME)),
        );
        on_path
            .filter(|lf| lf.is_absolute())
            .find(|lf| executable(lf) && !is_lf_rs(lf))
            .map(|lf| vec![lf.into()])
    }

    /// Transcribe row `id` of `root` with the Python app. Blocking (model
    /// load and inference): worker thread only.
    pub fn transcribe(&self, root: &DataRoot, id: i64) -> Result<Transcript, PythonError> {
        let prefix = self.locate().ok_or(PythonError::Missing)?;
        let mut command = Command::new(&prefix[0]);
        command
            .args(&prefix[1..])
            .args(["history", "retry", &id.to_string(), "--json", "--data-dir"])
            .arg(&root.localflow_dir)
            .env("LOCALFLOW_CONFIG", &root.config_path)
            .env("HF_HUB_OFFLINE", "1")
            .env("TRANSFORMERS_OFFLINE", "1")
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .current_dir(if root.localflow_dir.is_dir() {
                root.localflow_dir.clone()
            } else {
                std::env::temp_dir()
            })
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let mut run = Running {
            slot: &self.running,
            child: None,
        };
        *run.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(stop.clone());
        let child = run.child.insert(match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(PythonError::Missing),
            Err(e) => {
                return Err(PythonError::Failed(format!(
                    "the Python app couldn't start: {e}"
                )))
            }
        });
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => return Err(PythonError::Failed(format!("the Python app was lost: {e}"))),
            }
            if stop.load(Ordering::SeqCst) {
                return Err(PythonError::Failed(
                    "it was stopped because LocalFlow is closing".into(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(PythonError::Failed(format!(
                    "the Python app didn't finish within {}",
                    duration_words(self.timeout)
                )));
            }
            std::thread::sleep(POLL);
        };
        run.child = None;
        outcome(status, &collect(stdout), &collect(stderr))
    }
}

/// Clears the stop slot and kills and reaps an unfinished child on every
/// exit, unwinding too.
struct Running<'a> {
    slot: &'a Mutex<Option<Arc<AtomicBool>>>,
    child: Option<Child>,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Read a pipe to its end on a thread (keeping at most [`MAX_OUTPUT`]) so a
/// chatty child never blocks on a full pipe.
fn drain(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let _ = (&mut pipe).take(MAX_OUTPUT).read_to_end(&mut kept);
            let _ = std::io::copy(&mut pipe, &mut std::io::sink());
            let _ = tx.send(kept);
        });
    }
    rx
}

fn collect(output: mpsc::Receiver<Vec<u8>>) -> String {
    let bytes = output
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn outcome(status: ExitStatus, stdout: &str, stderr: &str) -> Result<Transcript, PythonError> {
    match status.code() {
        Some(0) => last_line(stdout)
            .and_then(parse)
            .ok_or(PythonError::Unsupported),
        Some(NO_MODEL_EXIT) => Err(PythonError::NoModel),
        Some(USAGE_EXIT) => Err(PythonError::Unsupported),
        code => Err(PythonError::Failed(short_error(stderr, code))),
    }
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|line| !line.is_empty())
}

/// `ts` arrives as Python's `repr`, parsed exactly here (serde_json's own
/// float parsing can be one ulp off, and the row key compares it exactly).
fn parse(line: &str) -> Option<Transcript> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    Some(Transcript {
        id: value.get("id")?.as_i64()?,
        ts: value.get("ts")?.as_str()?.parse().ok()?,
        raw_text: value.get("raw_text")?.as_str()?.to_owned(),
        final_text: value.get("final_text")?.as_str()?.to_owned(),
    })
}

/// The Python app's own one-line error (`Error: …`), else the last line it
/// wrote (a traceback's exception), else the exit code. Never a dump.
fn short_error(stderr: &str, code: Option<i32>) -> String {
    let line = stderr
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("Error: "))
        .or_else(|| last_line(stderr));
    let Some(line) = line else {
        return match code {
            Some(code) => format!("the Python app exited with code {code}."),
            None => "the Python app was stopped by a signal.".into(),
        };
    };
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut message: String = line.chars().take(MAX_MESSAGE_CHARS).collect();
    if message.len() < line.len() {
        message.push('…');
    } else if !message.ends_with(['.', '!', '?']) {
        message.push('.');
    }
    format!("the Python app reported: {message}")
}

fn duration_words(duration: Duration) -> String {
    match duration.as_secs() {
        secs if secs >= 120 => format!("{} minutes", secs / 60),
        1 => "1 second".into(),
        secs if secs > 0 => format!("{secs} seconds"),
        _ => format!("{} ms", duration.as_millis()),
    }
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

/// An `lf` that is really the Rust CLI (`lf-rs`, which has no faster-whisper)
/// under another name.
fn is_lf_rs(path: &Path) -> bool {
    std::fs::canonicalize(path).is_ok_and(|real| {
        real.file_stem()
            .is_some_and(|stem| stem.eq_ignore_ascii_case("lf-rs"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real row's timestamp that serde_json's float parsing reads one ulp
    /// off, which made a correct reply look like another history's.
    #[test]
    fn the_row_timestamp_is_read_exactly() {
        let line = r#"{"id": 2, "ts": "1795516579.5272841", "raw_text": "a", "final_text": "A."}"#;
        let reply = parse(line).unwrap();
        assert_eq!(reply.ts.to_bits(), 1795516579.5272841_f64.to_bits());
        assert_eq!((reply.id, reply.raw_text.as_str()), (2, "a"));
        // A bare number (an older reply format) isn't trusted.
        assert!(parse(r#"{"id": 2, "ts": 1.5, "raw_text": "", "final_text": ""}"#).is_none());
    }
}
