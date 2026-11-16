//! Single-instance guard (`localflow/singleinstance.py`).
//!
//! An OS advisory lock held by an open file handle (flock on Unix,
//! LockFileEx on Windows, both via `File::try_lock`), not a pid check: the
//! kernel releases it when the holder dies, so there is no stale-pid logic.
//! The lock file holds "<pid> <iso-timestamp>\n" for error messages. The
//! Python app takes the same lock on the same file, so the two exclude
//! each other too.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub fn lock_dir() -> PathBuf {
    lf_core::home::localflow_dir()
}

pub fn lock_file_path(name: &str) -> PathBuf {
    lock_dir().join(format!("{name}.lock"))
}

pub struct SingleInstance {
    path: PathBuf,
    file: Option<File>,
}

impl SingleInstance {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Release the lock and close the handle. Idempotent.
    pub fn release(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        self.release();
    }
}

/// Become the single instance `name`; None when another live process holds it.
pub fn acquire(name: &str) -> std::io::Result<Option<SingleInstance>> {
    acquire_at(&lock_file_path(name))
}

pub fn acquire_at(path: &Path) -> std::io::Result<Option<SingleInstance>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6f+00:00");
    // The lock is held either way; a write failure must not drop it.
    let _ = file
        .set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|()| writeln!(file, "{} {stamp}", std::process::id()))
        .and_then(|()| file.flush());
    Ok(Some(SingleInstance {
        path: path.to_path_buf(),
        file: Some(file),
    }))
}

/// (pid, timestamp) of whoever holds (or last held) the lock file.
pub fn read_holder_at(path: &Path) -> (Option<u32>, Option<String>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let content = content.trim();
    let (pid, ts) = content.split_once(' ').unwrap_or((content, ""));
    (pid.parse().ok(), (!ts.is_empty()).then(|| ts.to_string()))
}

pub fn read_holder(name: &str) -> (Option<u32>, Option<String>) {
    read_holder_at(&lock_file_path(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dictate.lock");
        let mut first = acquire_at(&path).unwrap().expect("first acquire");
        assert!(acquire_at(&path).unwrap().is_none());
        first.release();
        first.release();
        // Read after release: Windows locks are mandatory, so another handle
        // cannot read the file while it is held.
        let (pid, ts) = read_holder_at(&path);
        assert_eq!(pid, Some(std::process::id()));
        assert!(ts.unwrap().ends_with("+00:00"));
        assert!(acquire_at(&path).unwrap().is_some());
    }

    #[test]
    fn dropping_releases_and_stale_content_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        // A dead holder's pid left in the file does not block acquisition.
        std::fs::write(&path, "999999 2020-01-01T00:00:00+00:00\n").unwrap();
        {
            let _held = acquire_at(&path).unwrap().expect("acquire over stale file");
        }
        assert!(acquire_at(&path).unwrap().is_some());
        assert_eq!(read_holder_at(&dir.path().join("missing")), (None, None));
    }
}
