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
    pub fn identity(&mut self) -> std::io::Result<String> {
        use std::io::Read;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("lock was released"))?;
        file.seek(SeekFrom::Start(0))?;
        let mut identity = String::new();
        file.read_to_string(&mut identity)?;
        Ok(identity)
    }

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

/// Whether a lock file is actually held right now, found without taking it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockProbe {
    /// A live process holds it (its pid, as the kernel reports it).
    Held(u32),
    /// Nobody holds it: any pid recorded inside is only the last holder.
    Free,
    /// This platform can't tell without taking the lock.
    Unknown,
}

/// Read-only probe: Linux reads the kernel's `/proc/locks` table for the
/// file's inode (FLOCK entries; blocked waiters are ignored). The lock is
/// never acquired, so a runtime starting at the same moment can't lose it.
/// Other platforms report [`LockProbe::Unknown`].
pub fn probe_at(path: &Path) -> LockProbe {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::metadata(path) else {
            return LockProbe::Free;
        };
        match std::fs::read_to_string("/proc/locks") {
            Ok(table) => {
                let (major, minor) = dev_major_minor(meta.dev());
                flock_holder(&table, major, minor, meta.ino())
                    .map_or(LockProbe::Free, LockProbe::Held)
            }
            Err(_) => LockProbe::Unknown,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        LockProbe::Unknown
    }
}

/// glibc `major()`/`minor()` of a userspace `st_dev`.
pub fn dev_major_minor(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xffff_ff00);
    (major, minor)
}

/// The pid holding an FLOCK on the file `major:minor:ino` in a `/proc/locks`
/// table. Lines look like `3: FLOCK  ADVISORY  WRITE 4242 08:01:1234567 0 EOF`
/// (device numbers in hex, inode in decimal); `->` lines are waiters. The
/// whole device+inode must match: different filesystems reuse inode numbers.
/// (On btrfs subvolumes `stat` reports an anonymous device that differs from
/// the one the kernel prints here; the probe then finds no holder and the
/// reader stays conservative — "not responding", never "ready".)
pub fn flock_holder(table: &str, major: u64, minor: u64, ino: u64) -> Option<u32> {
    table.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let _id = fields.next()?;
        if fields.next()? != "FLOCK" {
            return None; // POSIX record locks, OFD locks, or a `->` waiter
        }
        let _advisory = fields.next()?;
        let _mode = fields.next()?;
        let pid = fields.next()?.parse::<u32>().ok()?;
        let mut file = fields.next()?.split(':');
        let file_major = u64::from_str_radix(file.next()?, 16).ok()?;
        let file_minor = u64::from_str_radix(file.next()?, 16).ok()?;
        let file_ino = file.next()?.parse::<u64>().ok()?;
        (file_major == major && file_minor == minor && file_ino == ino).then_some(pid)
    })
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
    fn proc_locks_parsing_finds_holders_not_waiters() {
        let table = "1: POSIX  ADVISORY  WRITE 100 08:01:55 0 EOF\n\
                     2: FLOCK  ADVISORY  WRITE 4242 08:01:777 0 EOF\n\
                     2: -> FLOCK  ADVISORY  WRITE 5151 08:01:888 0 EOF\n";
        assert_eq!(flock_holder(table, 8, 1, 777), Some(4242));
        assert_eq!(
            flock_holder(table, 8, 1, 888),
            None,
            "a waiter doesn't hold"
        );
        assert_eq!(
            flock_holder(table, 8, 1, 55),
            None,
            "POSIX record locks aren't ours"
        );
        assert_eq!(flock_holder("garbage", 0, 0, 1), None);
        // Same inode on another filesystem: not our lock file.
        assert_eq!(flock_holder(table, 8, 2, 777), None);
        assert_eq!(flock_holder(table, 0x103, 1, 777), None);
        let other = "7: FLOCK  ADVISORY  WRITE 99 103:0a:777 0 EOF\n";
        assert_eq!(
            flock_holder(other, 0x103, 0x0a, 777),
            Some(99),
            "hex device fields"
        );
        assert_eq!(flock_holder(other, 8, 1, 777), None);
        // glibc's st_dev encoding (makedev(8, 1) and makedev(259, 10)).
        assert_eq!(dev_major_minor(0x801), (8, 1));
        assert_eq!(dev_major_minor((0x103 << 8) | 0x0a), (0x103, 0x0a));
        // makedev(0x1000, 0x12_345): major's high bits at 44+, minor's at 20+.
        assert_eq!(
            dev_major_minor((0x1000u64 << 32) | (0x12_300u64 << 12) | 0x45),
            (0x1000, 0x12_345)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_sees_a_held_lock_and_a_stale_file_without_taking_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("localflow.lock");
        assert_eq!(probe_at(&path), LockProbe::Free, "no file");
        // A leftover file naming a live pid (pid 1) is not held.
        std::fs::write(&path, "1 2020-01-01T00:00:00+00:00\n").unwrap();
        assert_eq!(probe_at(&path), LockProbe::Free);
        let held = acquire_at(&path).unwrap().unwrap();
        assert_eq!(probe_at(&path), LockProbe::Held(std::process::id()));
        drop(held);
        assert_eq!(probe_at(&path), LockProbe::Free);
        // The probe never holds it: acquiring right after succeeds.
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
