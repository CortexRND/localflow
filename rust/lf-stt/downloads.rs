use std::path::PathBuf;

use anyhow::{Context, Result};
use hf_hub::api::sync::ApiBuilder;

use crate::model_storage;

#[cfg(any(feature = "whisper", feature = "parakeet", test))]
pub(crate) fn cached_file_exists(path: &std::path::Path) -> bool {
    path.is_file()
}

/// An already installed model file in the HF cache at `cache_root`, or
/// `None`. Never touches the network: `hf_hub::Cache` only reads the local
/// `refs`/`snapshots` layout. Empty files don't count as installed (the same
/// rule as `setup_downloads::NativeModel::inspect`).
pub fn cached_model_file(
    cache_root: &std::path::Path,
    repo: &str,
    filename: &str,
) -> Option<PathBuf> {
    hf_hub::Cache::new(cache_root.into())
        .model(repo.into())
        .get(filename)
        .filter(|path| std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0))
}

/// Resolve a model file through the shared standard HF blobs/snapshots cache.
pub fn model_file(repo: &str, filename: &str) -> Result<PathBuf> {
    ApiBuilder::from_env()
        .with_cache_dir(model_storage::hf_hub_cache(None))
        .with_progress(false)
        .build()?
        .model(repo.into())
        .get(filename)
        .with_context(|| format!("could not download {repo}/{filename}"))
}

/// Test helper: install `files` for `repo` in the standard HF layout under `root`.
#[cfg(test)]
pub(crate) fn install_for_test(root: &std::path::Path, repo: &str, files: &[(&str, &[u8])]) {
    let folder = root.join(format!("models--{}", repo.replace('/', "--")));
    std::fs::create_dir_all(folder.join("refs")).unwrap();
    std::fs::write(folder.join("refs/main"), "rev1").unwrap();
    let snapshot = folder.join("snapshots/rev1");
    std::fs::create_dir_all(&snapshot).unwrap();
    for (name, bytes) in files {
        std::fs::write(snapshot.join(name), bytes).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_lookup_needs_a_present_non_empty_file_and_reads_only_the_cache() {
        let root = tempfile::tempdir().unwrap();
        assert!(cached_model_file(root.path(), "org/repo", "a.bin").is_none());
        install_for_test(
            root.path(),
            "org/repo",
            &[("a.bin", b"w"), ("empty.bin", b"")],
        );
        let found = cached_model_file(root.path(), "org/repo", "a.bin").unwrap();
        assert!(found.starts_with(root.path()));
        assert!(cached_model_file(root.path(), "org/repo", "empty.bin").is_none());
        assert!(cached_model_file(root.path(), "org/repo", "missing.bin").is_none());
        assert!(cached_model_file(root.path(), "org/other", "a.bin").is_none());
        std::fs::remove_file(&found).unwrap();
        assert!(cached_model_file(root.path(), "org/repo", "a.bin").is_none());
    }

    #[test]
    fn cached_paths_are_rechecked_after_deletion() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("weights.bin");
        assert!(!cached_file_exists(&file));
        assert!(!cached_file_exists(root.path()));
        std::fs::write(&file, b"weights").unwrap();
        assert!(cached_file_exists(&file));
        #[cfg(unix)]
        let link = {
            let link = root.path().join("snapshot.bin");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(cached_file_exists(&link));
            link
        };
        std::fs::remove_file(&file).unwrap();
        assert!(!cached_file_exists(&file));
        #[cfg(unix)]
        assert!(!cached_file_exists(&link));
    }
}
