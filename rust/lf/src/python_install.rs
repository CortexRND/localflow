//! Interpreters that may belong to the Python LocalFlow install, best first:
//! the active virtualenv, the one next to the `lf` launcher on `PATH`, then a
//! bare `python3`/`python` (`python.exe`/`python` on Windows).

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PYTHON: &str = if cfg!(windows) {
    "python.exe"
} else {
    "python3"
};
const LAUNCHER: &str = if cfg!(windows) { "lf.exe" } else { "lf" };

pub(crate) fn candidates() -> Vec<PathBuf> {
    let venv = env::var_os("VIRTUAL_ENV").map(|root| {
        Path::new(&root)
            .join(if cfg!(windows) { "Scripts" } else { "bin" })
            .join(PYTHON)
    });
    let path = env::var_os("PATH").unwrap_or_default();
    let installed = env::split_paths(&path).find_map(|dir| {
        let binary = dir.join(LAUNCHER).canonicalize().ok()?;
        let python = binary.with_file_name(PYTHON);
        python.is_file().then_some(python)
    });
    venv.into_iter()
        .chain(installed)
        .chain([PathBuf::from(PYTHON), PathBuf::from("python")])
        .collect()
}

/// The first candidate that can import `module` (found, not executed).
pub(crate) fn with_module(module: &str) -> Option<PathBuf> {
    candidates().into_iter().find(|python| {
        Command::new(python)
            .args([
                "-c",
                "import importlib.util, sys; sys.exit(importlib.util.find_spec(sys.argv[1]) is None)",
                module,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}
