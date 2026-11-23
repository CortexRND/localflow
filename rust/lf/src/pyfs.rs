//! `pathlib` file operations as the Python CLI performs them: the same
//! system calls, text files with the platform's newline translation, and
//! failures that read like Python's `str(OSError)`.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::pyfmt::str_repr;

/// An `OSError` with its `filename`, displayed as Python formats it:
/// `[Errno 17] File exists: '/x'` (`[WinError 5] ...` on Windows).
#[derive(Debug)]
pub(crate) struct PyOsError {
    error: io::Error,
    filename: Option<String>,
}

impl PyOsError {
    pub(crate) fn new(error: io::Error, filename: Option<&Path>) -> Self {
        PyOsError {
            error,
            filename: filename.map(|path| path.to_string_lossy().into_owned()),
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn is_not_found(&self) -> bool {
        self.error.kind() == io::ErrorKind::NotFound
    }
}

impl fmt::Display for PyOsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(code) = self.error.raw_os_error() else {
            return write!(f, "{}", self.error);
        };
        // std appends " (os error N)" to the system message.
        let text = self.error.to_string();
        let message = text
            .strip_suffix(&format!(" (os error {code})"))
            .unwrap_or(&text);
        if cfg!(windows) {
            // FormatMessage text without the trailing period CPython drops.
            write!(f, "[WinError {code}] {}", message.trim_end_matches('.'))?;
        } else {
            write!(f, "[Errno {code}] {message}")?;
        }
        match &self.filename {
            Some(filename) => write!(f, ": {}", str_repr(filename)),
            None => Ok(()),
        }
    }
}

impl std::error::Error for PyOsError {}

fn wrap<T>(result: io::Result<T>, path: &Path) -> Result<T, PyOsError> {
    result.map_err(|error| PyOsError::new(error, Some(path)))
}

/// `Path.mkdir(parents=True, exist_ok=True)`, including which path a
/// failure names.
pub(crate) fn mkdir_parents(path: &Path) -> Result<(), PyOsError> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match path.parent() {
                Some(parent) if parent != path && !parent.as_os_str().is_empty() => {
                    mkdir_parents(parent)?;
                }
                _ => return Err(PyOsError::new(error, Some(path))),
            }
            match fs::create_dir(path) {
                Err(error) if !path.is_dir() => Err(PyOsError::new(error, Some(path))),
                _ => Ok(()),
            }
        }
        Err(error) if !path.is_dir() => Err(PyOsError::new(error, Some(path))),
        Err(_) => Ok(()),
    }
}

/// `Path.write_text(text)` with UTF-8: `\n` becomes `\r\n` on Windows.
pub(crate) fn write_text(path: &Path, text: &str) -> Result<(), PyOsError> {
    let text = if cfg!(windows) {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    };
    write_bytes(path, text.as_bytes())
}

/// `open(path, "wb").write(data)`.
pub(crate) fn write_bytes(path: &Path, data: &[u8]) -> Result<(), PyOsError> {
    let mut file = wrap(fs::File::create(path), path)?;
    wrap(file.write_all(data), path)
}

/// `Path.unlink()`.
pub(crate) fn unlink(path: &Path) -> Result<(), PyOsError> {
    wrap(fs::remove_file(path), path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_errors_read_like_python() {
        let error = PyOsError::new(io::Error::from_raw_os_error(2), Some(Path::new("it's")));
        let text = error.to_string();
        if cfg!(windows) {
            assert!(text.starts_with("[WinError 2] "), "{text}");
        } else {
            assert_eq!(text, "[Errno 2] No such file or directory: \"it's\"");
        }
        assert!(error.is_not_found());
        let plain = PyOsError::new(io::Error::other("boom"), None);
        assert_eq!(plain.to_string(), "boom");
    }

    #[test]
    fn mkdir_parents_names_the_blocking_path() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        fs::write(&file, "").unwrap();
        let nested = root.path().join("a").join("b");
        mkdir_parents(&nested).unwrap();
        mkdir_parents(&nested).unwrap();
        assert!(nested.is_dir());
        // ENOTDIR is not FileNotFoundError, so Python names the leaf too.
        // Windows reports the path as not found, so Python walks up to the file.
        let leaf = file.join("x").join("y");
        let named = if cfg!(windows) { &file } else { &leaf };
        let error = mkdir_parents(&leaf).unwrap_err();
        assert!(
            error
                .to_string()
                .ends_with(&str_repr(&named.to_string_lossy())),
            "{error}"
        );
    }
}
