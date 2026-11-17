//! Python-compatible model leases. Keep the returned guard alive for the model's lifetime.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model_storage::{
    faster_whisper_repo, mlx_repo, whisper_cpp_file, PARAKEET_ONNX_REPO, PARAKEET_REPO,
    WHISPER_CPP_REPO,
};

pub const VERSION: u32 = 1;
pub const MAX_LEASES: usize = 64;
pub const MAX_LEASE_BYTES: u64 = 4096;
static LEASE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A(\d{1,10})(?:\.[0-9a-f]{16})?\.json\z").unwrap());

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub pid: u64,
    pub backend: String,
    pub repo: Option<String>,
    pub known: bool,
}

pub fn lease_dir() -> PathBuf {
    lf_core::home::localflow_dir().join("stt-leases")
}

pub fn runtime_repo(backend: &str, model_size: &str) -> (Option<String>, bool) {
    match backend {
        "parakeet" => (Some(PARAKEET_REPO.into()), true),
        "parakeet-onnx" => (Some(PARAKEET_ONNX_REPO.into()), true),
        "whisper-cpp" => (
            whisper_cpp_file(model_size).map(|_| WHISPER_CPP_REPO.into()),
            true,
        ),
        "mlx" => {
            let repo = mlx_repo(model_size).map(str::to_owned);
            let known = repo.is_some();
            (repo, known)
        }
        "faster-whisper" => (faster_whisper_repo(model_size), true),
        _ => (None, false),
    }
}

pub fn selection_backend(value: Option<&str>) -> &'static str {
    match value
        .unwrap_or("")
        .trim_matches(lf_core::symbols::pycompat::is_space)
    {
        "parakeet" => "parakeet",
        "faster-whisper" => "faster-whisper",
        "mlx" | "mlx-whisper" => "mlx",
        "whisper-cpp" => "whisper-cpp",
        "parakeet-onnx" => "parakeet-onnx",
        _ => "unknown",
    }
}

/// Removes its lease on drop. Unlike Python's atexit hook, deliberate process::exit
/// skips Rust destructors; dead-pid leases are ignored by both implementations.
#[derive(Debug)]
pub struct LeaseGuard {
    path: PathBuf,
}
impl LeaseGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for LeaseGuard {
    fn drop(&mut self) {
        release(&self.path);
    }
}

/// Never fails outward. Scoped leases never replace the process's long-lived lease.
pub fn record(
    backend: &str,
    model_size: &str,
    root: Option<&Path>,
    scoped: bool,
) -> Option<LeaseGuard> {
    let result = (|| -> anyhow::Result<LeaseGuard> {
        let folder = root.map(Path::to_path_buf).unwrap_or_else(lease_dir);
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&folder)?;
        let pid = std::process::id();
        let name = if scoped {
            let mut token = [0_u8; 8];
            getrandom::fill(&mut token).map_err(|e| anyhow::anyhow!("{e}"))?;
            format!(
                "{pid}.{}.json",
                token.iter().map(|b| format!("{b:02x}")).collect::<String>()
            )
        } else {
            format!("{pid}.json")
        };
        let path = folder.join(name);
        let (repo, known) = runtime_repo(backend, model_size);
        let data = json!({"version": VERSION, "pid": pid, "backend": backend.chars().take(64).collect::<String>(), "repo": repo, "known": known, "created": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64()});
        let mut temporary = tempfile::Builder::new()
            .prefix(".lease.")
            .suffix(".tmp")
            .tempfile_in(&folder)?;
        serde_json::to_writer(temporary.as_file_mut(), &data)?;
        temporary.persist(&path).map_err(|e| e.error)?;
        Ok(LeaseGuard { path })
    })();
    match result {
        Ok(guard) => Some(guard),
        Err(error) => {
            log::warn!("could not record the speech model in use: {error}");
            None
        }
    }
}

pub fn release(path: &Path) {
    release_for_pid(path, u64::from(std::process::id()));
}

fn release_for_pid(path: &Path, pid: u64) {
    if let Ok(bytes) = fs::read(path) {
        if let Ok(data) = serde_json::from_slice::<Value>(&bytes) {
            if python_integer_eq(&data["pid"], pid) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// Only for this process, or an owned helper whose exit has been observed.
#[cfg(feature = "runtime")]
pub(crate) fn release_process(pid: u32) {
    if let Ok(entries) = fs::read_dir(lease_dir()) {
        for path in entries.flatten().map(|entry| entry.path()) {
            if lease_pid(&path) == Some(u64::from(pid)) {
                release_for_pid(&path, u64::from(pid));
            }
        }
    }
}

fn lease_pid(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let captures = LEASE_NAME.captures(name)?;
    Some(captures[1].chars().fold(0_u64, |n, c| {
        n * 10 + u64::from(lf_core::symbols::pycompat::decimal_value(c))
    }))
}

pub fn pid_alive(pid: u64) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == u64::from(std::process::id()) {
        return true;
    }
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return true;
        };
        // SAFETY: signal zero only checks existence/permissions and does not signal the process.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED},
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };
        let Ok(pid) = u32::try_from(pid) else {
            return false;
        };
        // SAFETY: process query only; the returned handle is closed on every successful open.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code = 0;
            let _ = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            // Match Python: a successful open counts as alive, even if exit code changed.
            true
        }
    }
}

pub fn live_leases(root: Option<&Path>, alive: impl Fn(u64) -> bool) -> Vec<Lease> {
    let folder = root.map(Path::to_path_buf).unwrap_or_else(lease_dir);
    let Ok(entries) = fs::read_dir(folder) else {
        return vec![];
    };
    let Ok(mut entries) = entries
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()
    else {
        return vec![];
    };
    entries.sort();
    entries
        .into_iter()
        .filter_map(|path| {
            let pid = lease_pid(&path)?;
            if alive(pid) {
                Some(read(&path, pid))
            } else {
                let _ = fs::remove_file(path);
                None
            }
        })
        .take(MAX_LEASES)
        .collect()
}

// Python numeric equality accepts 1.0 and True for version/pid, not just JSON integers.
fn python_integer_eq(value: &Value, expected: u64) -> bool {
    value.as_u64() == Some(expected)
        || value.as_f64() == Some(expected as f64)
        || value.as_bool().map(u64::from) == Some(expected)
}

pub fn read(path: &Path, pid: u64) -> Lease {
    let unknown = || Lease {
        pid,
        backend: "unknown".into(),
        repo: None,
        known: false,
    };
    let Ok(info) = path.symlink_metadata() else {
        return unknown();
    };
    if !info.is_file() || info.len() > MAX_LEASE_BYTES {
        return unknown();
    }
    let Ok(bytes) = fs::read(path) else {
        return unknown();
    };
    let Ok(data) = serde_json::from_slice::<Value>(&bytes) else {
        return unknown();
    };
    if !data.is_object()
        || !python_integer_eq(&data["version"], VERSION.into())
        || !python_integer_eq(&data["pid"], pid)
    {
        return unknown();
    }
    let (Some(backend), Some(known)) = (data["backend"].as_str(), data["known"].as_bool()) else {
        return unknown();
    };
    let repo = match data.get("repo") {
        Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        _ => return unknown(),
    };
    Lease {
        pid,
        backend: backend.into(),
        repo,
        known,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_and_selection_mappings() {
        use crate::model_storage::*;
        for &(size, repo) in FASTER_WHISPER_REPOS {
            assert_eq!(
                runtime_repo("faster-whisper", size),
                (Some(repo.into()), true)
            );
        }
        for &(size, repo) in MLX_REPOS {
            assert_eq!(runtime_repo("mlx", size), (Some(repo.into()), true));
        }
        assert_eq!(runtime_repo("mlx", "missing"), (None, false));
        assert_eq!(
            runtime_repo("faster-whisper", "org/custom"),
            (Some("org/custom".into()), true)
        );
        for size in ["/models/local", "./local", "~/local", "unknown", "a/b/c"] {
            assert_eq!(runtime_repo("faster-whisper", size), (None, true));
        }
        assert_eq!(runtime_repo("plugin", "base"), (None, false));
        for &(size, _) in WHISPER_CPP_FILES {
            assert_eq!(
                runtime_repo("whisper-cpp", size),
                (Some(WHISPER_CPP_REPO.into()), true)
            );
        }
        assert_eq!(
            runtime_repo("parakeet-onnx", "anything"),
            (Some(PARAKEET_ONNX_REPO.into()), true)
        );
        for (input, expected) in [
            (None, "unknown"),
            (Some("auto"), "unknown"),
            (Some(""), "unknown"),
            (Some(" mlx-whisper "), "mlx"),
            (Some("faster-whisper"), "faster-whisper"),
            (Some("parakeet"), "parakeet"),
            (Some("plugin"), "unknown"),
        ] {
            assert_eq!(selection_backend(input), expected);
        }
    }

    #[test]
    fn private_atomic_guard_scopes_and_release_checks_pid() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("leases");
        let long = record("faster-whisper", "base", Some(&root), false).unwrap();
        assert_eq!(
            long.path(),
            root.join(format!("{}.json", std::process::id()))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(root.metadata().unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(
                long.path().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let a = record("mlx", "base", Some(&root), true).unwrap();
        let b = record("parakeet", "base", Some(&root), true).unwrap();
        assert_ne!(a.path(), b.path());
        assert!(LEASE_NAME.is_match(a.path().file_name().unwrap().to_str().unwrap()));
        assert_eq!(live_leases(Some(&root), pid_alive).len(), 3);
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists());
        assert!(long.path().exists() && b.path().exists());
        let data: Value = serde_json::from_slice(&fs::read(long.path()).unwrap()).unwrap();
        assert_eq!(data.as_object().unwrap().len(), 6);
        assert!(data["created"].as_f64().is_some());
        fs::write(b.path(), r#"{"pid":0}"#).unwrap();
        let path = b.path().to_path_buf();
        drop(b);
        assert!(path.exists());
        drop(long);
        let not_dir = temp.path().join("file");
        fs::write(&not_dir, "x").unwrap();
        assert!(record("mlx", "base", Some(&not_dir), false).is_none());
        assert!(pid_alive(u64::from(std::process::id())));
        assert!(!pid_alive(0));
    }

    #[test]
    fn malformed_leases_are_unknown_and_names_are_strict() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "12.json.bak",
            "12.ABCDEF0123456789.json",
            "12.short.json",
            "-1.json",
            "12..json",
            "x12.json",
            "12.0123456789abcdef.extra.json",
            "12345678901.json",
        ] {
            fs::write(root.path().join(name), "{}").unwrap();
        }
        assert!(live_leases(Some(root.path()), |_| true).is_empty());
        let path = root.path().join("4242.json");
        let unknown = Lease {
            pid: 4242,
            backend: "unknown".into(),
            repo: None,
            known: false,
        };
        let valid = json!({"version": 1, "pid": 4242, "backend": "mlx", "known": true, "repo": "model/repo"});
        for (field, value) in [
            ("version", json!(2)),
            ("pid", json!(1)),
            ("backend", json!(null)),
            ("known", json!(1)),
            ("repo", json!(1)),
        ] {
            let mut data = valid.clone();
            data[field] = value;
            fs::write(&path, data.to_string()).unwrap();
            assert_eq!(read(&path, 4242), unknown);
        }
        for data in ["{broken".to_string(), "[]".into(), " ".repeat(4097)] {
            fs::write(&path, data).unwrap();
            assert_eq!(read(&path, 4242), unknown);
        }
        fs::write(&path, valid.to_string()).unwrap();
        assert!(read(&path, 4242).known);
        assert!(live_leases(Some(root.path()), |_| false).is_empty());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&path, root.path().join("4243.json")).unwrap();
            assert_eq!(
                read(&root.path().join("4243.json"), 4243).backend,
                "unknown"
            );
        }
        assert!(live_leases(Some(&root.path().join("missing")), |_| true).is_empty());
    }

    #[test]
    fn dead_leases_do_not_hide_a_live_model_after_repeated_helper_eviction() {
        let root = tempfile::tempdir().unwrap();
        for pid in 1000..1200 {
            fs::write(root.path().join(format!("{pid}.json")), "{}").unwrap();
        }
        let live = root.path().join("9999.json");
        fs::write(&live, json!({"version":1,"pid":9999,"backend":"whisper-cpp","repo":WHISPER_CPP_REPO,"known":true}).to_string()).unwrap();
        let leases = live_leases(Some(root.path()), |pid| pid == 9999);
        assert_eq!(leases.len(), 1, "dead helpers hid the active model lease");
        assert_eq!(leases[0].repo.as_deref(), Some(WHISPER_CPP_REPO));
        assert!(live.exists());
        assert!(!root.path().join("1000.json").exists());
    }

    #[test]
    fn only_live_lease_entries_consume_the_enumeration_limit() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..150 {
            fs::write(root.path().join(format!("{i}.json")), "{}").unwrap();
        }
        assert_eq!(live_leases(Some(root.path()), |_| true).len(), MAX_LEASES);
        for i in 0..128 {
            fs::write(root.path().join(format!("!{i}")), "{}").unwrap();
        }
        assert_eq!(live_leases(Some(root.path()), |_| true).len(), MAX_LEASES);
    }
}
