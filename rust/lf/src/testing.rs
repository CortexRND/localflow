//! In-process harness for unit tests: buffers instead of stdio and a fake
//! clipboard. Commands that resolve user paths are exercised end to end in
//! `tests/cli.rs` with a per-process environment instead.

use std::io::Cursor;
use std::sync::Mutex;

use crate::{run, Clipboard, Io};

/// Held by unit tests that change the process environment or the
/// process-wide keyring backend.
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default)]
pub struct FakeClipboard {
    pub copied: Vec<String>,
    pub unavailable: bool,
}

impl Clipboard for FakeClipboard {
    fn set_text(&mut self, text: &str) -> anyhow::Result<()> {
        if self.unavailable {
            anyhow::bail!("clipboard unavailable: no display");
        }
        self.copied.push(text.to_string());
        Ok(())
    }
}

pub struct Outcome {
    pub code: i32,
    pub out: String,
    pub err: String,
}

pub fn run_with(args: &[&str], input: &str, clipboard: &mut FakeClipboard) -> Outcome {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut reader = Cursor::new(input.as_bytes().to_vec());
    let mut io = Io {
        out: &mut out,
        err: &mut err,
        input: &mut reader,
        clipboard,
        server_url: None,
        terminal: false,
    };
    let mut argv = vec!["lf-rs"];
    argv.extend_from_slice(args);
    let code = run(argv, &mut io);
    Outcome {
        code,
        out: String::from_utf8_lossy(&out).into_owned(),
        err: String::from_utf8_lossy(&err).into_owned(),
    }
}

pub fn run_cli(args: &[&str]) -> Outcome {
    run_with(args, "", &mut FakeClipboard::default())
}
