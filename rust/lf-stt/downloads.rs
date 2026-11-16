use std::path::PathBuf;

use anyhow::{Context, Result};
use hf_hub::api::sync::ApiBuilder;

use crate::model_storage;

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
