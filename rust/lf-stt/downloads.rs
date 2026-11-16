use std::path::PathBuf;

use anyhow::{Context, Result};
use hf_hub::api::sync::ApiBuilder;

use crate::model_storage;

#[cfg(any(feature = "whisper", feature = "parakeet", test))]
pub(crate) fn cached_file_exists(path: &std::path::Path) -> bool {
    path.is_file()
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

#[cfg(test)]
mod tests {
    use super::*;

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
