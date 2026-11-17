//! Native engine artifacts in the standard, Python-compatible HF cache layout.
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use fs2::FileExt;
use lf_providers::transfer;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::model_storage;

#[derive(Clone, Debug)]
pub struct NativeModel {
    pub repo: String,
    pub files: Vec<String>,
    pub format: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inspection {
    pub installed: bool,
    pub bytes: u64,
}

pub fn native_model(backend: &str, size: &str) -> Result<NativeModel, String> {
    let native = crate::factory::canonical_backend(backend).map_err(|e| e.to_string())?;
    if !crate::factory::backend_available(backend) {
        return Err(format!("{native} feature is not compiled in."));
    }
    let (repo, files, format) = if native == "whisper-cpp" {
        (
            model_storage::WHISPER_CPP_REPO,
            vec![model_storage::whisper_cpp_file(size)
                .ok_or_else(|| format!("Unsupported GGML model size: {size}"))?],
            "GGML",
        )
    } else {
        (
            model_storage::PARAKEET_ONNX_REPO,
            model_storage::PARAKEET_ONNX_FILES.to_vec(),
            "ONNX",
        )
    };
    Ok(NativeModel {
        repo: repo.into(),
        files: files.into_iter().map(str::to_owned).collect(),
        format,
    })
}

impl NativeModel {
    fn validate(&self) -> Result<(), String> {
        if self.repo.split('/').count() != 2
            || self.repo.split('/').any(|s| {
                s.is_empty()
                    || s == "."
                    || s == ".."
                    || !s
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            })
            || self.files.is_empty()
            || self.files.iter().any(|s| {
                s.is_empty()
                    || Path::new(s).components().count() != 1
                    || !matches!(
                        Path::new(s).components().next(),
                        Some(std::path::Component::Normal(_))
                    )
            })
        {
            return Err("Invalid native model artifact path.".into());
        }
        Ok(())
    }

    pub fn inspect(&self, root: &Path) -> Inspection {
        if self.validate().is_err() {
            return Inspection::default();
        }
        let cache = hf_hub::Cache::new(root.into()).model(self.repo.clone());
        let sizes: Vec<_> = self
            .files
            .iter()
            .filter_map(|f| {
                cache
                    .get(f)?
                    .metadata()
                    .ok()
                    .map(|m| m.len())
                    .filter(|n| *n > 0)
            })
            .collect();
        Inspection {
            installed: sizes.len() == self.files.len(),
            bytes: sizes.iter().sum(),
        }
    }

    pub fn download(
        &self,
        root: &Path,
        endpoint: &str,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        self.validate()?;
        transfer::run(
            cancel,
            self.download_inner(root, endpoint, progress, cancel),
        )
    }

    async fn download_inner(
        &self,
        root: &Path,
        endpoint: &str,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let client = transfer::client(endpoint)?;
        let endpoint = endpoint.trim_end_matches('/');
        let token = std::env::var("HF_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(|| hf_hub::Cache::new(root.into()).token());
        let request = |url| {
            let request = client.get(url);
            if let Some(token) = &token {
                request.bearer_auth(token)
            } else {
                request
            }
        };
        let metadata: Metadata = request(format!(
            "{endpoint}/api/models/{}/revision/main?blobs=true",
            self.repo
        ))
        .send()
        .await
        .map_err(|e| transfer::error(&e))?
        .error_for_status()
        .map_err(|e| transfer::error(&e))?
        .json()
        .await
        .map_err(|e| transfer::error(&e))?;
        if !hex(&metadata.sha, 40) {
            return Err("Invalid HF commit hash.".into());
        }
        let mut artifacts = Vec::new();
        for name in &self.files {
            let entry = metadata
                .siblings
                .iter()
                .find(|f| &f.rfilename == name)
                .ok_or_else(|| format!("Missing model artifact: {name}"))?;
            let oid = entry
                .lfs
                .as_ref()
                .map(|l| l.sha256.as_str())
                .unwrap_or(&entry.blob_id);
            if entry.size == 0 || !(hex(oid, 40) || hex(oid, 64)) {
                return Err(format!("Invalid model metadata: {name}"));
            }
            artifacts.push((name, entry, oid));
        }
        let total = artifacts
            .iter()
            .try_fold(0_u64, |sum, (_, file, _)| sum.checked_add(file.size))
            .ok_or("Model size overflow.")?;
        let repo = root.join(format!("models--{}", self.repo.replace('/', "--")));
        let blobs = repo.join("blobs");
        let snapshot = repo.join("snapshots").join(&metadata.sha);
        let locks = root
            .join(".locks")
            .join(format!("models--{}", self.repo.replace('/', "--")));
        for directory in [&blobs, &snapshot, &locks] {
            std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
        }
        let mut done = 0;
        let mut reported = std::time::Instant::now();
        progress(0, total);
        for (name, entry, oid) in artifacts {
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(locks.join(format!("{oid}.lock")))
                .map_err(|e| e.to_string())?;
            loop {
                match lock.try_lock_exclusive() {
                    Ok(()) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        transfer::check_cancel(cancel)?;
                        tokio_sleep().await;
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            let blob = blobs.join(oid);
            if std::fs::metadata(&blob).map(|m| m.len()).ok() != Some(entry.size) {
                let mut temporary =
                    tempfile::NamedTempFile::new_in(&blobs).map_err(|e| e.to_string())?;
                let mut response = request(format!(
                    "{endpoint}/{}/resolve/{}/{name}",
                    self.repo, metadata.sha
                ))
                .send()
                .await
                .map_err(|e| transfer::error(&e))?
                .error_for_status()
                .map_err(|e| transfer::error(&e))?;
                let mut count = 0_u64;
                let mut digest = Sha256::new();
                while let Some(chunk) = response.chunk().await.map_err(|e| transfer::error(&e))? {
                    transfer::check_cancel(cancel)?;
                    count = count
                        .checked_add(chunk.len() as u64)
                        .ok_or("Download size overflow.")?;
                    if count > entry.size {
                        return Err(format!("Invalid size for {name}"));
                    }
                    temporary.write_all(&chunk).map_err(|e| e.to_string())?;
                    digest.update(&chunk);
                    if reported.elapsed() >= std::time::Duration::from_millis(100)
                        || count == entry.size
                    {
                        progress(done + count, total);
                        reported = std::time::Instant::now();
                    }
                }
                if count != entry.size
                    || (entry.lfs.is_some() && format!("{:x}", digest.finalize()) != oid)
                {
                    return Err(format!("Model integrity check failed: {name}"));
                }
                transfer::check_cancel(cancel)?;
                temporary.as_file().sync_all().map_err(|e| e.to_string())?;
                temporary.persist(&blob).map_err(|e| e.to_string())?;
            }
            let destination = snapshot.join(name);
            if std::fs::metadata(&destination).map(|m| m.len()).ok() != Some(entry.size) {
                if destination.symlink_metadata().is_ok() {
                    std::fs::remove_file(&destination).map_err(|e| e.to_string())?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(Path::new("../../blobs").join(oid), &destination)
                    .map_err(|e| e.to_string())?;
                #[cfg(not(unix))]
                std::fs::hard_link(&blob, &destination)
                    .or_else(|_| std::fs::copy(&blob, &destination).map(|_| ()))
                    .map_err(|e| e.to_string())?;
            }
            done += entry.size;
            progress(done, total);
        }
        transfer::check_cancel(cancel)?;
        hf_hub::Cache::new(root.into())
            .model(self.repo.clone())
            .create_ref(&metadata.sha)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

async fn tokio_sleep() {
    lf_providers::transfer::pause().await;
}
fn hex(s: &str, length: usize) -> bool {
    s.len() == length && s.bytes().all(|c| c.is_ascii_hexdigit())
}
#[derive(Deserialize)]
struct Metadata {
    sha: String,
    siblings: Vec<Artifact>,
}
#[derive(Deserialize)]
struct Artifact {
    rfilename: String,
    size: u64,
    #[serde(rename = "blobId", default)]
    blob_id: String,
    lfs: Option<Lfs>,
}
#[derive(Deserialize)]
struct Lfs {
    sha256: String,
}

#[cfg(test)]
#[path = "setup_downloads_tests.rs"]
mod tests;
