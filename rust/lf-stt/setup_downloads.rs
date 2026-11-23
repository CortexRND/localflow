//! Native engine artifacts in the standard, Python-compatible HF cache layout.
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use fs2::FileExt;
use lf_providers::transfer;
use reqwest::{RequestBuilder, Response, Url};
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
        if !valid_repo_id(&self.repo)
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
            Self::download_inner(
                &self.repo,
                Some(&self.files),
                root,
                endpoint,
                progress,
                cancel,
            ),
        )
    }

    async fn download_inner(
        repo_id: &str,
        files: Option<&[String]>,
        root: &Path,
        endpoint: &str,
        mut progress: impl FnMut(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let client = transfer::client(endpoint)?;
        let endpoint = endpoint.trim_end_matches('/');
        let token = std::env::var("HF_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(|| {
                if files.is_none() {
                    std::env::var_os("HF_TOKEN_PATH")
                        .and_then(|path| std::fs::read_to_string(path).ok())
                        .map(|token| token.trim().to_owned())
                        .filter(|t| !t.is_empty())
                        .or_else(|| hf_hub::Cache::from_env().token())
                } else {
                    hf_hub::Cache::new(root.into()).token()
                }
            });
        let token =
            token.filter(|_| files.is_some() || !hf_env_flag("HF_HUB_DISABLE_IMPLICIT_TOKEN"));
        let request = |url| {
            let request = client.get(url);
            if let Some(token) = &token {
                request.bearer_auth(token)
            } else {
                request
            }
        };
        let inspect_error = |message: String| {
            if files.is_none() {
                format!("Could not inspect model files: {message}")
            } else {
                message
            }
        };
        let metadata_url = if files.is_some() {
            format!("{endpoint}/api/models/{repo_id}/revision/main?blobs=true")
        } else {
            let url = format!("{endpoint}/api/models/{repo_id}?blobs=true");
            if hf_offline_mode() {
                return Err(inspect_error(format!(
                    "Cannot reach {url}: offline mode is enabled. To disable it, please unset the `HF_HUB_OFFLINE` environment variable."
                )));
            }
            url
        };
        let metadata: Metadata = checked_response(request(metadata_url), files.is_none())
            .await
            .map_err(inspect_error)?
            .json()
            .await
            .map_err(|e| inspect_error(transfer::error(&e)))?;
        if files.is_none() && !metadata.siblings.iter().any(|file| file.size > 0) {
            return Err("The model repository has no downloadable files".into());
        }
        if !hex(&metadata.sha, 40) {
            return Err("Invalid HF commit hash.".into());
        }
        let entries = if let Some(files) = files {
            files
                .iter()
                .map(|name| {
                    metadata
                        .siblings
                        .iter()
                        .find(|file| &file.rfilename == name)
                        .ok_or_else(|| format!("Missing model artifact: {name}"))
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            metadata.siblings.iter().collect()
        };
        let mut artifacts = Vec::new();
        for entry in entries {
            let name = &entry.rfilename;
            let oid = entry
                .lfs
                .as_ref()
                .map(|l| l.sha256.as_str())
                .unwrap_or(&entry.blob_id);
            if !valid_artifact_path(name)
                || (files.is_some() && entry.size == 0)
                || !(hex(oid, 40) || hex(oid, 64))
            {
                return Err(format!("Invalid model metadata: {name}"));
            }
            artifacts.push((name, entry, oid));
        }
        let total = artifacts
            .iter()
            .try_fold(0_u64, |sum, (_, file, _)| sum.checked_add(file.size))
            .ok_or("Model size overflow.")?;
        let repo = root.join(model_storage::repo_folder_name(repo_id));
        let blobs = repo.join("blobs");
        let snapshot = repo.join("snapshots").join(&metadata.sha);
        let locks = root
            .join(".locks")
            .join(model_storage::repo_folder_name(repo_id));
        for directory in [&blobs, &snapshot, &locks] {
            std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
        }
        let mut done = 0_u64;
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
            let size_known = entry.size > 0 || files.is_some();
            if !std::fs::metadata(&blob)
                .is_ok_and(|m| m.is_file() && (!size_known || m.len() == entry.size))
            {
                let mut temporary =
                    tempfile::NamedTempFile::new_in(&blobs).map_err(|e| e.to_string())?;
                let mut url = Url::parse(&format!("{endpoint}/{repo_id}/resolve/{}", metadata.sha))
                    .map_err(|e| e.to_string())?;
                url.path_segments_mut()
                    .map_err(|_| "Invalid HF endpoint.")?
                    .extend(name.split('/'));
                let mut response = checked_response(request(url.into()), files.is_none())
                    .await
                    .map_err(|message| {
                        if files.is_none() {
                            format!("Could not download {name}: {message}")
                        } else {
                            message
                        }
                    })?;
                let mut count = 0_u64;
                let mut digest = Sha256::new();
                while let Some(chunk) = response.chunk().await.map_err(|e| transfer::error(&e))? {
                    transfer::check_cancel(cancel)?;
                    count = count
                        .checked_add(chunk.len() as u64)
                        .ok_or("Download size overflow.")?;
                    if size_known && count > entry.size {
                        return Err(format!("Invalid size for {name}"));
                    }
                    temporary.write_all(&chunk).map_err(|e| e.to_string())?;
                    digest.update(&chunk);
                    if reported.elapsed() >= std::time::Duration::from_millis(100)
                        || count == entry.size
                    {
                        progress(done.saturating_add(count).min(total), total);
                        reported = std::time::Instant::now();
                    }
                }
                if (size_known && count != entry.size)
                    || (entry.lfs.is_some() && format!("{:x}", digest.finalize()) != oid)
                {
                    return Err(format!("Model integrity check failed: {name}"));
                }
                transfer::check_cancel(cancel)?;
                temporary.as_file().sync_all().map_err(|e| e.to_string())?;
                temporary.persist(&blob).map_err(|e| e.to_string())?;
            }
            let destination = snapshot.join(name);
            let blob_size = std::fs::metadata(&blob).map_err(|e| e.to_string())?.len();
            if std::fs::metadata(&destination).map(|m| m.len()).ok() != Some(blob_size) {
                std::fs::create_dir_all(destination.parent().unwrap())
                    .map_err(|e| e.to_string())?;
                if destination.symlink_metadata().is_ok() {
                    std::fs::remove_file(&destination).map_err(|e| e.to_string())?;
                }
                #[cfg(unix)]
                {
                    let mut relative = std::path::PathBuf::new();
                    for _ in 0..=name.split('/').count() {
                        relative.push("..");
                    }
                    std::os::unix::fs::symlink(relative.join("blobs").join(oid), &destination)
                        .map_err(|e| e.to_string())?;
                }
                #[cfg(not(unix))]
                std::fs::hard_link(&blob, &destination)
                    .or_else(|_| std::fs::copy(&blob, &destination).map(|_| ()))
                    .map_err(|e| e.to_string())?;
            }
            done = done
                .saturating_add(if size_known { entry.size } else { blob_size })
                .min(total);
            progress(done, total);
        }
        transfer::check_cancel(cancel)?;
        hf_hub::Cache::new(root.into())
            .model(repo_id.into())
            .create_ref(&metadata.sha)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Error returned by [`download_snapshot`] when its `interrupt` resolved.
pub const INTERRUPTED: &str = "Download interrupted.";

/// Download an entire Python STT catalog repository into the shared HF cache.
///
/// `interrupt` is polled first and before every step of the transfer, so a
/// signal listener registers before any temporary file exists. Once it
/// resolves, the in-flight transfer future is dropped (its `NamedTempFile`
/// is removed and no blob, snapshot link or ref is written for it) and
/// [`INTERRUPTED`] is returned.
pub fn download_snapshot(
    repo: &str,
    root: &Path,
    endpoint: &str,
    progress: impl FnMut(u64, u64),
    interrupt: impl Future<Output = ()>,
) -> Result<(), String> {
    if !valid_repo_id(repo) {
        return Err("Invalid HF model repository ID.".into());
    }
    let cancel = AtomicBool::new(false);
    transfer::run(&cancel, async {
        let mut interrupt = std::pin::pin!(interrupt);
        let mut operation = std::pin::pin!(NativeModel::download_inner(
            repo, None, root, endpoint, progress, &cancel
        ));
        std::future::poll_fn(|cx| {
            if interrupt.as_mut().poll(cx).is_ready() {
                return std::task::Poll::Ready(Err(INTERRUPTED.to_owned()));
            }
            operation.as_mut().poll(cx)
        })
        .await
    })
}

fn valid_artifact_path(name: &str) -> bool {
    name.split('/').all(|part| {
        !part.is_empty() && !part.ends_with(['.', ' ']) && !part.contains(['\\', ':', '\0'])
    })
}

fn valid_repo_id(repo: &str) -> bool {
    repo.split('/').count() == 2
        && repo.split('/').all(|part| {
            !part.is_empty()
                && !part.ends_with('.')
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
}

async fn checked_response(req: RequestBuilder, python_errors: bool) -> Result<Response, String> {
    let response = req.send().await.map_err(|e| transfer::error(&e))?;
    if python_errors && (response.status().is_client_error() || response.status().is_server_error())
    {
        let status = response.status();
        let kind = if status.is_client_error() {
            "Client"
        } else {
            "Server"
        };
        let reason = status.canonical_reason().unwrap_or_default();
        return Err(format!(
            "{} {kind} Error: {reason} for url: {}",
            status.as_u16(),
            response.url()
        ));
    }
    response.error_for_status().map_err(|e| transfer::error(&e))
}

fn hf_env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.to_ascii_uppercase().as_str(),
            "1" | "ON" | "YES" | "TRUE"
        )
    })
}

/// Mirrors huggingface_hub `_is_true(HF_HUB_OFFLINE or TRANSFORMERS_OFFLINE)`:
/// a nonempty primary value wins (even if false), an unset or empty one falls
/// back to the legacy name, and the match uses Python's Unicode `str.upper()`.
fn hf_offline_mode() -> bool {
    let value = std::env::var_os("HF_HUB_OFFLINE")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("TRANSFORMERS_OFFLINE"));
    value.is_some_and(|value| {
        matches!(
            value.to_string_lossy().to_uppercase().as_str(),
            "1" | "ON" | "YES" | "TRUE"
        )
    })
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
    #[serde(default, deserialize_with = "optional_size")]
    size: u64,
    #[serde(rename = "blobId", default)]
    blob_id: String,
    lfs: Option<Lfs>,
}
fn optional_size<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or_default())
}
#[derive(Deserialize)]
struct Lfs {
    sha256: String,
}

#[cfg(test)]
#[path = "setup_downloads_tests.rs"]
mod tests;
