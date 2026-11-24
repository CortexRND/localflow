//! Catalog-only Hugging Face cache accounting and conservative deletion guards.
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use lf_core::config::Config;
use serde::{Deserialize, Serialize};

use crate::stt_lease::{self, Lease};

pub const FASTER_WHISPER_REPOS: &[(&str, &str)] = &[
    ("tiny.en", "Systran/faster-whisper-tiny.en"),
    ("tiny", "Systran/faster-whisper-tiny"),
    ("base.en", "Systran/faster-whisper-base.en"),
    ("base", "Systran/faster-whisper-base"),
    ("small.en", "Systran/faster-whisper-small.en"),
    ("small", "Systran/faster-whisper-small"),
    ("medium.en", "Systran/faster-whisper-medium.en"),
    ("medium", "Systran/faster-whisper-medium"),
    ("large-v1", "Systran/faster-whisper-large-v1"),
    ("large-v2", "Systran/faster-whisper-large-v2"),
    ("large-v3", "Systran/faster-whisper-large-v3"),
    ("large", "Systran/faster-whisper-large-v3"),
    ("distil-large-v2", "Systran/faster-distil-whisper-large-v2"),
    (
        "distil-medium.en",
        "Systran/faster-distil-whisper-medium.en",
    ),
    ("distil-small.en", "Systran/faster-distil-whisper-small.en"),
    ("distil-large-v3", "Systran/faster-distil-whisper-large-v3"),
    ("distil-large-v3.5", "distil-whisper/distil-large-v3.5-ct2"),
    (
        "large-v3-turbo",
        "mobiuslabsgmbh/faster-whisper-large-v3-turbo",
    ),
    ("turbo", "mobiuslabsgmbh/faster-whisper-large-v3-turbo"),
];
pub const MLX_REPOS: &[(&str, &str)] = &[
    ("tiny", "mlx-community/whisper-tiny-mlx"),
    ("base", "mlx-community/whisper-base-mlx"),
    ("small", "mlx-community/whisper-small-mlx"),
    ("medium", "mlx-community/whisper-medium-mlx"),
    ("large-v3", "mlx-community/whisper-large-v3-mlx"),
    ("large-v3-turbo", "mlx-community/whisper-large-v3-turbo"),
];
pub const PARAKEET_REPO: &str = "mlx-community/parakeet-tdt-0.6b-v2";
/// All ggml files share a repository: selecting or leasing any size protects them all.
pub const WHISPER_CPP_REPO: &str = "ggerganov/whisper.cpp";
pub const WHISPER_CPP_FILES: &[(&str, &str)] = &[
    ("tiny", "ggml-tiny.bin"),
    ("tiny.en", "ggml-tiny.en.bin"),
    ("base", "ggml-base.bin"),
    ("base.en", "ggml-base.en.bin"),
    ("small", "ggml-small.bin"),
    ("small.en", "ggml-small.en.bin"),
    ("medium", "ggml-medium.bin"),
    ("medium.en", "ggml-medium.en.bin"),
    ("large-v3", "ggml-large-v3.bin"),
    ("large-v3-turbo", "ggml-large-v3-turbo.bin"),
];
pub const PARAKEET_ONNX_REPO: &str = "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8";
pub const PARAKEET_ONNX_FILES: &[&str] = &[
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

fn lookup<'a>(mapping: &'a [(&str, &str)], size: &str) -> Option<&'a str> {
    mapping.iter().find(|(s, _)| *s == size).map(|(_, r)| *r)
}
pub fn mlx_repo(size: &str) -> Option<&'static str> {
    lookup(MLX_REPOS, size)
}
pub fn whisper_cpp_file(size: &str) -> Option<&'static str> {
    lookup(WHISPER_CPP_FILES, size)
}
pub fn faster_whisper_repo(size: &str) -> Option<String> {
    lookup(FASTER_WHISPER_REPOS, size)
        .map(str::to_owned)
        .or_else(|| {
            (size.matches('/').count() == 1 && !size.starts_with(['/', '.', '~']))
                .then(|| size.into())
        })
}

/// Size-neutral: inventory names the ggml files actually stored.
const WHISPER_CPP_LABEL: &str = "Whisper (whisper.cpp)";
const WHISPER_CATALOG_LABELS: &[(&str, &str)] = &[
    ("tiny", "Tiny"),
    ("base", "Base"),
    ("small", "Small"),
    ("medium", "Medium"),
    ("large-v3-turbo", "Large v3 Turbo"),
    ("large-v3", "Large v3"),
    ("distil-large-v3", "Distil Large v3"),
];

/// Repository for a Python STT catalog ID, not an arbitrary model size or path.
pub fn python_catalog_repo(id: &str) -> Option<String> {
    if let Some(size) = id.strip_prefix("fw-") {
        lookup(WHISPER_CATALOG_LABELS, size)?;
        faster_whisper_repo(size)
    } else if let Some(size) = id.strip_prefix("mlx-") {
        mlx_repo(size).map(str::to_owned)
    } else if id == "parakeet-v2" {
        Some(PARAKEET_REPO.into())
    } else {
        None
    }
}

/// In Python insertion order; Rust-only repositories follow the Python catalog.
pub fn catalog() -> Vec<(String, String)> {
    let mut result: Vec<_> = WHISPER_CATALOG_LABELS
        .iter()
        .map(|(size, label)| {
            (
                faster_whisper_repo(size).unwrap(),
                format!("Whisper {label}"),
            )
        })
        .collect();
    result.extend(MLX_REPOS.iter().map(|(size, repo)| {
        let label = lookup(WHISPER_CATALOG_LABELS, size).unwrap();
        (repo.to_string(), format!("Whisper {label} (Apple Silicon)"))
    }));
    result.push((PARAKEET_REPO.into(), "Parakeet v2".into()));
    result.push((WHISPER_CPP_REPO.into(), WHISPER_CPP_LABEL.into()));
    result.push((PARAKEET_ONNX_REPO.into(), "Parakeet v2 (ONNX)".into()));
    result
}

/// The optional mapping controls cache-variable precedence; expansions use the
/// process environment, just like os.path.expanduser/expandvars in Python.
pub fn hf_hub_cache(env: Option<&BTreeMap<String, String>>) -> PathBuf {
    let actual;
    let env = match env {
        Some(env) => env,
        None => {
            actual = std::env::vars().collect();
            &actual
        }
    };
    let get = |key| env.get(key).filter(|s| !s.is_empty());
    for key in ["HF_HUB_CACHE", "HUGGINGFACE_HUB_CACHE"] {
        if let Some(value) = get(key) {
            return expand(value);
        }
    }
    if let Some(value) = get("HF_HOME") {
        return expand(value).join("hub");
    }
    let xdg = get("XDG_CACHE_HOME").cloned().unwrap_or_else(|| {
        lf_core::home::python_home()
            .unwrap_or_else(|_| PathBuf::from("~"))
            .join(".cache")
            .to_string_lossy()
            .into_owned()
    });
    expand(&xdg).join("huggingface").join("hub")
}

fn expand(value: &str) -> PathBuf {
    let user =
        lf_core::home::expand_user(Path::new(value)).unwrap_or_else(|_| PathBuf::from(value));
    let value = user.to_string_lossy();
    #[cfg(not(windows))]
    {
        let re = regex::Regex::new(r"\$(?:\{([^}]*)\}|([A-Za-z0-9_]+))").unwrap();
        PathBuf::from(
            re.replace_all(&value, |caps: &regex::Captures<'_>| {
                let key = caps.get(1).or_else(|| caps.get(2)).unwrap().as_str();
                std::env::var(key).unwrap_or_else(|_| caps[0].into())
            })
            .into_owned(),
        )
    }
    #[cfg(windows)]
    {
        // ntpath.expandvars suppresses expansion inside single-quoted sections.
        let re = regex::Regex::new(r"'[^']*'|\$\$|%%|%([^%]*)%|\$\{([^}]*)\}|\$([A-Za-z0-9_-]+)")
            .unwrap();
        PathBuf::from(
            re.replace_all(&value, |caps: &regex::Captures<'_>| match &caps[0] {
                "$$" => "$".into(),
                "%%" => "%".into(),
                s if s.starts_with('\'') => s.into(),
                _ => {
                    let key = caps
                        .get(1)
                        .or_else(|| caps.get(2))
                        .or_else(|| caps.get(3))
                        .unwrap()
                        .as_str();
                    std::env::var(key).unwrap_or_else(|_| caps[0].into())
                }
            })
            .into_owned(),
        )
    }
}

pub fn repo_folder_name(repo: &str) -> String {
    format!("models--{}", repo.replace('/', "--"))
}

/// Cannot probe Python's mlx_whisper import. On Apple Silicon protect both
/// possible auto backends conservatively; elsewhere assume MLX unavailable.
pub fn default_mlx_available() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectedRepos {
    Known(BTreeSet<String>),
    Unknown,
}

pub fn selected_repos(config: &Config, mlx_available: impl Fn() -> bool) -> SelectedRepos {
    let backend = config.field_str("stt_backend").unwrap_or("");
    let size = config.field_str("model_size").unwrap_or("");
    let device = config.field_str("stt_device").unwrap_or("");
    let mut repos = BTreeSet::new();
    match backend {
        "parakeet" => {
            repos.insert(PARAKEET_REPO.into());
            repos.insert(PARAKEET_ONNX_REPO.into());
        }
        "parakeet-onnx" => {
            repos.insert(PARAKEET_ONNX_REPO.into());
        }
        "whisper-cpp" => {
            repos.insert(WHISPER_CPP_REPO.into());
        }
        "auto" | "mlx" | "mlx-whisper" | "faster-whisper" => {
            if whisper_cpp_file(size).is_some() {
                repos.insert(WHISPER_CPP_REPO.into());
            }
            if backend != "faster-whisper" && device == "auto" {
                if let Some(repo) = mlx_repo(size) {
                    if mlx_available() {
                        repos.insert(repo.into());
                    }
                }
            }
            if let Some(repo) = faster_whisper_repo(size) {
                repos.insert(repo);
            }
        }
        _ => return SelectedRepos::Unknown,
    }
    SelectedRepos::Known(repos)
}

pub const UNKNOWN_RUNNING: &str = "A running LocalFlow process uses a speech model LocalFlow can't identify, so nothing was deleted. Quit it or restart LocalFlow first.";
pub const UNKNOWN_SELECTED: &str = "Your speech provider is a plug-in, so LocalFlow can't tell which model it uses. Nothing was deleted.";

#[derive(Clone, Debug, Serialize)]
pub struct Protection {
    pub running: BTreeSet<String>,
    pub selected: BTreeSet<String>,
    pub unknown: String,
}
impl Protection {
    pub fn status(&self, repo: &str) -> &'static str {
        if self.running.contains(repo) {
            "In use"
        } else if self.selected.contains(repo) {
            "Selected"
        } else {
            ""
        }
    }
}
pub fn protection(
    config: &Config,
    mlx_available: impl Fn() -> bool,
    leases: Option<&[Lease]>,
) -> Protection {
    let live;
    let leases = match leases {
        Some(l) => l,
        None => {
            live = stt_lease::live_leases(None, stt_lease::pid_alive);
            &live
        }
    };
    let mut guard = Protection {
        running: BTreeSet::new(),
        selected: BTreeSet::new(),
        unknown: String::new(),
    };
    for lease in leases {
        if !lease.known {
            guard.unknown = UNKNOWN_RUNNING.into();
        } else if let Some(repo) = &lease.repo {
            if !repo.is_empty() {
                guard.running.insert(repo.clone());
            }
        }
    }
    match selected_repos(config, mlx_available) {
        SelectedRepos::Known(repos) => guard.selected = repos,
        SelectedRepos::Unknown => {
            if guard.unknown.is_empty() {
                guard.unknown = UNKNOWN_SELECTED.into();
            }
        }
    }
    guard
}

/// Visits regular files under `folder` as (entry path, resolved path, metadata),
/// following symlinks only to files inside `folder`.
fn walk_files(folder: &Path, mut visit: impl FnMut(&Path, &Path, &fs::Metadata)) {
    let Ok(root) = folder.canonicalize() else {
        return;
    };
    let mut stack = vec![folder.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(info) = path.symlink_metadata() else {
                continue;
            };
            let (resolved, info) = if info.is_symlink() {
                let Ok(target) = path.canonicalize() else {
                    continue;
                };
                if !target.starts_with(&root) {
                    continue;
                }
                let Ok(info) = target.metadata() else {
                    continue;
                };
                (target, info)
            } else {
                if info.is_dir() {
                    stack.push(path);
                    continue;
                }
                (path.clone(), info)
            };
            if info.is_file() {
                visit(&path, &resolved, &info);
            }
        }
    }
}

pub fn folder_size(folder: &Path) -> u64 {
    let mut seen = HashSet::new();
    let mut total = 0;
    walk_files(folder, |_, path, info| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let _ = path;
            if seen.insert((info.dev(), info.ino())) {
                total += info.len();
            }
        }
        #[cfg(windows)]
        {
            if let Ok(handle) = same_file::Handle::from_path(path) {
                if seen.insert(handle) {
                    total += info.len();
                }
            }
        }
    });
    total
}

/// Names the ggml sizes present in a whisper.cpp repository snapshot.
fn whisper_cpp_label(folder: &Path) -> String {
    let snapshots = folder.join("snapshots");
    let mut names = HashSet::new();
    walk_files(folder, |entry, _, _| {
        if entry.starts_with(&snapshots) {
            if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
                names.insert(name.to_owned());
            }
        }
    });
    let sizes: Vec<_> = WHISPER_CPP_FILES
        .iter()
        .filter(|(_, file)| names.contains(*file))
        .map(|(size, _)| match size.strip_suffix(".en") {
            Some(base) => format!(
                "{} (English)",
                lookup(WHISPER_CATALOG_LABELS, base).unwrap_or(base)
            ),
            None => lookup(WHISPER_CATALOG_LABELS, size)
                .unwrap_or(size)
                .to_owned(),
        })
        .collect();
    if sizes.is_empty() {
        WHISPER_CPP_LABEL.into()
    } else {
        format!("Whisper {} (whisper.cpp)", sizes.join(", "))
    }
}

pub fn model_folder(cache: &Path, repo: &str) -> Option<PathBuf> {
    let folder = cache.join(repo_folder_name(repo));
    if folder.symlink_metadata().ok()?.is_symlink() || !folder.is_dir() {
        return None;
    }
    if folder.canonicalize().ok()?.parent()? != cache.canonicalize().ok()? {
        return None;
    }
    Some(folder)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedModel {
    pub repo: String,
    pub label: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub status: String,
}
impl CachedModel {
    pub fn protected(&self) -> bool {
        !self.status.is_empty()
    }
}

pub fn inventory(
    config: &Config,
    cache: Option<&Path>,
    mlx_available: impl Fn() -> bool,
    leases: Option<&[Lease]>,
) -> Vec<CachedModel> {
    let default_cache;
    let cache = match cache {
        Some(c) => c,
        None => {
            default_cache = hf_hub_cache(None);
            &default_cache
        }
    };
    let guard = protection(config, mlx_available, leases);
    let mut found: Vec<_> = catalog()
        .into_iter()
        .filter_map(|(repo, label)| {
            let path = model_folder(cache, &repo)?;
            let status = match guard.status(&repo) {
                "" if !guard.unknown.is_empty() => "Unknown use",
                status => status,
            }
            .into();
            let label = if repo == WHISPER_CPP_REPO {
                whisper_cpp_label(&path)
            } else {
                label
            };
            Some(CachedModel {
                size_bytes: folder_size(&path),
                repo,
                label,
                path,
                status,
            })
        })
        .collect();
    found.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.label.cmp(&b.label))
    });
    found
}

pub fn delete_unused(
    repo: &str,
    config: &Config,
    cache: Option<&Path>,
    mlx_available: impl Fn() -> bool,
    leases: Option<&[Lease]>,
) -> Result<u64> {
    let Some((_, label)) = catalog().into_iter().find(|(r, _)| r == repo) else {
        bail!("That isn't a speech model LocalFlow manages.");
    };
    let guard = protection(config, mlx_available, leases);
    if !guard.unknown.is_empty() {
        bail!("{}", guard.unknown);
    }
    if guard.running.contains(repo) {
        bail!("{label} is loaded by a running LocalFlow process, so it was kept.");
    }
    if guard.selected.contains(repo) {
        bail!("{label} is the speech model in your settings, so it was kept.");
    }
    let default_cache;
    let cache = match cache {
        Some(c) => c,
        None => {
            default_cache = hf_hub_cache(None);
            &default_cache
        }
    };
    let Some(folder) = model_folder(cache, repo) else {
        bail!("{label} isn't downloaded (or isn't a plain folder).");
    };
    let before = folder_size(&folder);
    if let Err(error) = fs::remove_dir_all(&folder) {
        let freed = before.saturating_sub(folder_size(&folder));
        bail!(
            "Couldn't fully delete {label} ({}); freed {}.",
            error_reason(&error),
            format_bytes(freed)
        );
    }
    Ok(before)
}

fn error_reason(error: &std::io::Error) -> String {
    // Rust appends an OS error code; Python's OSError.strerror does not.
    let text = error.to_string();
    text.rsplit_once(" (os error ")
        .map(|(reason, _)| reason)
        .unwrap_or(&text)
        .to_owned()
}

pub fn format_bytes(size: impl Into<i128>) -> String {
    let mut value = size.into() as f64;
    for unit in ["bytes", "KB", "MB", "GB"] {
        if value < 1000.0 || unit == "GB" {
            return if unit == "bytes" {
                format!("{} bytes", value as i128)
            } else if value < 10.0 {
                format!("{value:.1} {unit}")
            } else {
                format!("{value:.0} {unit}")
            };
        }
        value /= 1000.0;
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const BASE: &str = "Systran/faster-whisper-base";
    const SMALL: &str = "Systran/faster-whisper-small";
    const MLX: &str = "mlx-community/whisper-base-mlx";
    fn config(backend: &str, size: &str, device: &str) -> Config {
        let mut config = Config::default();
        for (key, value) in [
            ("stt_backend", backend),
            ("model_size", size),
            ("stt_device", device),
        ] {
            config.set_field(key, json!(value));
        }
        config
    }
    fn repo(cache: &Path, name: &str, bytes: usize) -> PathBuf {
        let folder = cache.join(repo_folder_name(name));
        fs::create_dir_all(folder.join("blobs")).unwrap();
        fs::create_dir_all(folder.join("refs")).unwrap();
        fs::write(folder.join("blobs/data"), vec![0; bytes]).unwrap();
        fs::write(folder.join("refs/main"), "rev1").unwrap();
        #[cfg(unix)]
        {
            for rev in ["rev1", "rev2"] {
                let snap = folder.join("snapshots").join(rev);
                fs::create_dir_all(&snap).unwrap();
                std::os::unix::fs::symlink("../../blobs/data", snap.join("file")).unwrap();
            }
            fs::hard_link(folder.join("blobs/data"), folder.join("duplicate")).unwrap();
        }
        folder
    }

    #[test]
    fn selected_repos_and_unknown_backends() {
        for (backend, size, device, mlx, expected) in [
            (
                "auto",
                "base",
                "auto",
                true,
                vec![BASE, MLX, WHISPER_CPP_REPO],
            ),
            ("auto", "base", "auto", false, vec![BASE, WHISPER_CPP_REPO]),
            ("auto", "base", "cpu", true, vec![BASE, WHISPER_CPP_REPO]),
            ("mlx", "base", "auto", false, vec![BASE, WHISPER_CPP_REPO]),
            (
                "mlx-whisper",
                "base",
                "cpu",
                false,
                vec![BASE, WHISPER_CPP_REPO],
            ),
            (
                "faster-whisper",
                "base",
                "cpu",
                false,
                vec![BASE, WHISPER_CPP_REPO],
            ),
            (
                "mlx-whisper",
                "distil-large-v3",
                "auto",
                true,
                vec!["Systran/faster-distil-whisper-large-v3"],
            ),
            (
                "faster-whisper",
                "large",
                "auto",
                true,
                vec!["Systran/faster-whisper-large-v3"],
            ),
            (
                "faster-whisper",
                "org/custom-ct2",
                "auto",
                true,
                vec!["org/custom-ct2"],
            ),
            ("faster-whisper", "/models/local", "auto", true, vec![]),
            (
                "parakeet",
                "large-v3",
                "auto",
                true,
                vec![PARAKEET_REPO, PARAKEET_ONNX_REPO],
            ),
            ("whisper-cpp", "base", "auto", false, vec![WHISPER_CPP_REPO]),
            (
                "parakeet-onnx",
                "base",
                "auto",
                false,
                vec![PARAKEET_ONNX_REPO],
            ),
        ] {
            assert_eq!(
                selected_repos(&config(backend, size, device), || mlx),
                SelectedRepos::Known(expected.into_iter().map(str::to_owned).collect())
            );
        }
        assert_eq!(
            selected_repos(&config("plugin", "base", "auto"), || false),
            SelectedRepos::Unknown
        );
    }

    #[test]
    fn aliased_native_models_cannot_be_deleted() {
        for (backend, native) in [
            ("auto", WHISPER_CPP_REPO),
            ("mlx", WHISPER_CPP_REPO),
            ("mlx-whisper", WHISPER_CPP_REPO),
            ("faster-whisper", WHISPER_CPP_REPO),
            ("parakeet", PARAKEET_ONNX_REPO),
        ] {
            let root = tempfile::tempdir().unwrap();
            let folder = repo(root.path(), native, 100);
            let cfg = config(backend, "base", "auto");
            let models = inventory(&cfg, Some(root.path()), || false, Some(&[]));
            assert_eq!(
                models.iter().find(|m| m.repo == native).unwrap().status,
                "Selected"
            );
            assert!(delete_unused(native, &cfg, Some(root.path()), || false, Some(&[])).is_err());
            assert!(folder.is_dir());
        }
    }

    #[test]
    fn inventory_and_deletion_recheck_all_guards() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("hub");
        repo(&cache, SMALL, 1500);
        repo(&cache, BASE, 200);
        let mlx = repo(&cache, MLX, 300);
        let unrelated = repo(&cache, "Qwen/LLM", 5000);
        let outside = root.path().join("outside");
        fs::write(&outside, vec![0; 7000]).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, mlx.join("escape")).unwrap();
            std::os::unix::fs::symlink(
                &unrelated,
                cache.join(repo_folder_name("Systran/faster-whisper-tiny")),
            )
            .unwrap();
        }
        let cfg = config("faster-whisper", "small", "auto");
        let inv = inventory(&cfg, Some(&cache), || false, Some(&[]));
        assert_eq!(inv.len(), 3);
        assert_eq!(inv[0].repo, SMALL);
        assert_eq!(inv[0].size_bytes, 1504);
        assert_eq!(inv[0].status, "Selected");
        assert!(inv[0].protected());
        assert_eq!(folder_size(&mlx), 304);
        let running = [Lease {
            pid: 42,
            backend: "faster-whisper".into(),
            repo: Some(SMALL.into()),
            known: true,
        }];
        let next = config("auto", "base", "auto");
        let inv = inventory(&next, Some(&cache), || true, Some(&running));
        assert_eq!(inv[0].status, "In use");
        assert_eq!(
            delete_unused(SMALL, &next, Some(&cache), || true, Some(&running))
                .unwrap_err()
                .to_string(),
            "Whisper Small is loaded by a running LocalFlow process, so it was kept."
        );
        assert_eq!(
            delete_unused(MLX, &next, Some(&cache), || true, Some(&[]))
                .unwrap_err()
                .to_string(),
            "Whisper Base (Apple Silicon) is the speech model in your settings, so it was kept."
        );
        let plugin = config("plugin", "base", "auto");
        assert!(inventory(&plugin, Some(&cache), || false, Some(&[]))
            .iter()
            .all(|m| m.status == "Unknown use"));
        assert_eq!(
            delete_unused(MLX, &plugin, Some(&cache), || false, Some(&[]))
                .unwrap_err()
                .to_string(),
            UNKNOWN_SELECTED
        );
        let unknown = [Lease {
            pid: 42,
            backend: "unknown".into(),
            repo: None,
            known: false,
        }];
        assert_eq!(
            delete_unused(MLX, &plugin, Some(&cache), || false, Some(&unknown))
                .unwrap_err()
                .to_string(),
            UNKNOWN_RUNNING
        );
        for name in ["Qwen/LLM", "../../etc", "/", "", "llama3.2:3b"] {
            assert_eq!(
                delete_unused(name, &cfg, Some(&cache), || false, Some(&[]))
                    .unwrap_err()
                    .to_string(),
                "That isn't a speech model LocalFlow manages."
            );
        }
        assert!(delete_unused(
            "Systran/faster-whisper-tiny",
            &cfg,
            Some(&cache),
            || false,
            Some(&[])
        )
        .unwrap_err()
        .to_string()
        .contains("isn't downloaded"));
        assert_eq!(
            delete_unused(MLX, &cfg, Some(&cache), || false, Some(&[])).unwrap(),
            304
        );
        assert!(!mlx.exists());
        assert!(unrelated.exists() && outside.exists());
        assert!(inventory(
            &cfg,
            Some(&root.path().join("missing")),
            || false,
            Some(&[])
        )
        .is_empty());
    }

    #[test]
    fn whisper_cpp_inventory_names_the_stored_sizes() {
        let label = |files: &[&str]| {
            let root = tempfile::tempdir().unwrap();
            let folder = root.path().join(repo_folder_name(WHISPER_CPP_REPO));
            let snap = folder.join("snapshots/rev1");
            fs::create_dir_all(folder.join("blobs")).unwrap();
            fs::create_dir_all(folder.join(".no_exist/rev1")).unwrap();
            fs::create_dir_all(folder.join(".locks")).unwrap();
            fs::create_dir_all(&snap).unwrap();
            fs::write(folder.join(".no_exist/rev1/ggml-small.bin"), "").unwrap();
            fs::write(folder.join(".locks/ggml-medium.bin.lock"), "").unwrap();
            fs::write(folder.join("blobs/partial.incomplete"), [0; 8]).unwrap();
            for (i, file) in files.iter().enumerate() {
                let blob = format!("blob{i}");
                fs::write(folder.join("blobs").join(&blob), [0; 8]).unwrap();
                #[cfg(unix)]
                std::os::unix::fs::symlink(format!("../../blobs/{blob}"), snap.join(file)).unwrap();
                #[cfg(not(unix))]
                fs::copy(folder.join("blobs").join(&blob), snap.join(file)).unwrap();
            }
            #[cfg(unix)]
            {
                let outside = root.path().join("ggml-large-v3.bin");
                fs::write(&outside, [0; 8]).unwrap();
                std::os::unix::fs::symlink(&outside, snap.join("ggml-large-v3.bin")).unwrap();
            }
            let cfg = config("parakeet", "base", "auto");
            let inv = inventory(&cfg, Some(root.path()), || false, Some(&[]));
            assert_eq!(inv.len(), 1);
            assert_eq!(inv[0].repo, WHISPER_CPP_REPO);
            inv[0].label.clone()
        };
        assert_eq!(label(&["ggml-tiny.bin"]), "Whisper Tiny (whisper.cpp)");
        assert_eq!(
            label(&["ggml-base.en.bin", "ggml-tiny.bin"]),
            "Whisper Tiny, Base (English) (whisper.cpp)"
        );
        assert_eq!(
            label(&["ggml-large-v3-turbo.bin", "ggml-tiny.en.bin"]),
            "Whisper Tiny (English), Large v3 Turbo (whisper.cpp)"
        );
        assert_eq!(label(&[]), "Whisper (whisper.cpp)");
    }

    #[test]
    fn cache_precedence_catalog_and_sizes() {
        for (env, suffix) in [
            (vec![("XDG_CACHE_HOME", "/x")], "/x/huggingface/hub"),
            (vec![("HF_HOME", "/h"), ("XDG_CACHE_HOME", "/x")], "/h/hub"),
            (
                vec![("HUGGINGFACE_HUB_CACHE", "/old"), ("HF_HOME", "/h")],
                "/old",
            ),
            (
                vec![("HF_HUB_CACHE", "/new"), ("HUGGINGFACE_HUB_CACHE", "/old")],
                "/new",
            ),
        ] {
            assert_eq!(
                hf_hub_cache(Some(
                    &env.into_iter().map(|(k, v)| (k.into(), v.into())).collect()
                )),
                PathBuf::from(suffix)
            );
        }
        let cat = catalog();
        assert_eq!(cat.len(), 16);
        assert_eq!(
            cat[0],
            ("Systran/faster-whisper-tiny".into(), "Whisper Tiny".into())
        );
        assert_eq!(cat[13], (PARAKEET_REPO.into(), "Parakeet v2".into()));
        for (bytes, expected) in [
            (0, "0 bytes"),
            (512, "512 bytes"),
            (999, "999 bytes"),
            (1000, "1.0 KB"),
            (486_000_000, "486 MB"),
            (1_622_000_000, "1.6 GB"),
        ] {
            assert_eq!(format_bytes(bytes), expected);
        }
    }
}
