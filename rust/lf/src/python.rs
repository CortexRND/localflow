//! The Python installation `lf-rs` runs next to. `stt` asks it about speech
//! runtimes; `agent` and `setup install-app` point launchers at it the way the
//! Python CLI uses `sys.executable`.

use std::env;
use std::path::{Path, PathBuf};

/// Interpreter file name the discovery looks for.
pub(crate) const PYTHON: &str = if cfg!(windows) {
    "python.exe"
} else {
    "python3"
};

/// Interpreters to try, best first: `$VIRTUAL_ENV`'s, the one next to the
/// `lf` launcher found on PATH (symlinks resolved), then bare `python3` and
/// `python` for the OS to look up on PATH. Only the launcher candidate is
/// checked to exist.
pub(crate) fn candidates() -> Vec<PathBuf> {
    let launcher = if cfg!(windows) { "lf.exe" } else { "lf" };
    let venv = env::var_os("VIRTUAL_ENV").map(|root| {
        Path::new(&root)
            .join(if cfg!(windows) { "Scripts" } else { "bin" })
            .join(PYTHON)
    });
    let path = env::var_os("PATH").unwrap_or_default();
    let installed = env::split_paths(&path).find_map(|dir| {
        let binary = dir.join(launcher).canonicalize().ok()?;
        let python = binary.with_file_name(PYTHON);
        python.is_file().then_some(python)
    });
    venv.into_iter()
        .chain(installed)
        .chain([PathBuf::from(PYTHON), PathBuf::from("python")])
        .collect()
}

/// The first of [`candidates`] that exists, without running it; bare names
/// are looked up on PATH like the OS would.
pub(crate) fn find() -> Option<PathBuf> {
    candidates().into_iter().find_map(|candidate| {
        if candidate.components().count() > 1 {
            return candidate.is_file().then_some(candidate);
        }
        let path = env::var_os("PATH")?;
        env::split_paths(&path)
            .map(|dir| dir.join(&candidate))
            .find(|binary| binary.is_file())
    })
}

/// `Path(sys.executable).parent / name`: `name` next to the Python
/// interpreter, or next to `lf-rs` when no interpreter is found.
pub(crate) fn sibling(name: &str) -> PathBuf {
    let dir = find()
        .and_then(|python| python.parent().map(Path::to_path_buf))
        .or_else(|| {
            env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf))
        })
        .unwrap_or_default();
    dir.join(name)
}

#[cfg(test)]
mod tests {
    #[test]
    fn bare_names_come_last() {
        let candidates = super::candidates();
        let names: Vec<_> = candidates[candidates.len() - 2..]
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [super::PYTHON, "python"]);
    }
}
