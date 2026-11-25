//! The B6 speaker-label models (`localflow/diarization/models.py` `DIAR_MODELS`),
//! read from the Hugging Face hub cache at their pinned revision.
//!
//! Read-only: nothing here downloads, and nothing deletes a file that fails
//! verification (`lf speakers download` owns repair and eviction).

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiarModel {
    pub id: &'static str,
    /// Shown in status output.
    pub label: &'static str,
    pub repo: &'static str,
    /// Pinned 40-hex commit.
    pub revision: &'static str,
    pub filename: &'static str,
    pub sha256: &'static str,
    pub size_bytes: u64,
    pub licence: &'static str,
    /// Embedders only: cosine-similarity floor for naming a recognised speaker.
    pub match_threshold: Option<f64>,
}

/// Whether this binary links the sherpa-onnx runtime (`extra_installed`).
pub const RUNTIME_INSTALLED: bool = cfg!(feature = "sherpa");

pub const SEGMENTATION: DiarModel = DiarModel {
    id: "pyannote-segmentation-3.0",
    repo: "csukuangfj/sherpa-onnx-pyannote-segmentation-3-0",
    revision: "9403a6902bb58e3d5ae8c7e77c3422de279db2e0",
    filename: "model.onnx",
    sha256: "220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079",
    size_bytes: 5_992_913,
    label: "Speaker labels: segmentation",
    licence: "MIT",
    match_threshold: None,
};

pub const EMBEDDING: DiarModel = DiarModel {
    id: "3dspeaker-campplus-zh-en-16k-advanced",
    repo: "csukuangfj/speaker-embedding-models",
    revision: "a8055d6daef723a6a4ecf4de233708575bf4994f",
    filename: "3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
    sha256: "53cab20ba8f2eb2f6e2d89ecc18e88ef3222169b61b4419cbc13666744a4574b",
    size_bytes: 28_281_165,
    label: "Speaker labels: voice embedding",
    licence: "Apache-2.0",
    // Provisional and deliberately strict, as `models.py` (B10 sets the measured value).
    match_threshold: Some(0.6),
};

pub const DIAR_MODELS: [DiarModel; 2] = [SEGMENTATION, EMBEDDING];

/// `models--<org>--<name>`, as `model_storage.repo_folder_name`.
pub fn repo_folder(cache: &Path, model: &DiarModel) -> PathBuf {
    cache.join(format!("models--{}", model.repo.replace('/', "--")))
}

/// The snapshot pointer `<folder>/snapshots/<revision>/<filename>` (not verified).
pub fn model_path(cache: &Path, model: &DiarModel) -> PathBuf {
    repo_folder(cache, model)
        .join("snapshots")
        .join(model.revision)
        .join(model.filename)
}

/// The verified snapshot path for `model`, or a user-facing reason.
///
/// Accepts a regular file (copy layout) or a symlink that resolves to exactly
/// `<folder>/blobs/<sha256>`; the repo, `snapshots` and revision folders must be
/// plain directories. Size, then a streaming SHA-256, must match the pin.
pub fn verified_path(cache: &Path, model: &DiarModel) -> Result<PathBuf, String> {
    use sha2::{Digest, Sha256};
    use std::{fs, io::Read};
    const NOT_DOWNLOADED: &str = "Speaker models are not downloaded. Run `lf speakers download`.";
    let plain_dir = |path: &Path| {
        fs::symlink_metadata(path)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false)
    };
    let folder = repo_folder(cache, model);
    let revision = folder.join("snapshots").join(model.revision);
    for dir in [&folder, &folder.join("snapshots"), &revision] {
        if fs::symlink_metadata(dir).is_err() {
            return Err(NOT_DOWNLOADED.into());
        }
        if !plain_dir(dir) {
            return Err("The speaker model cache folder isn't a plain folder.".into());
        }
    }
    let pointer = revision.join(model.filename);
    let info = fs::symlink_metadata(&pointer).map_err(|_| NOT_DOWNLOADED.to_string())?;
    let file = if info.file_type().is_symlink() {
        let blob = folder.join("blobs").join(model.sha256);
        let (Ok(target), Ok(expected)) = (fs::canonicalize(&pointer), fs::canonicalize(&blob))
        else {
            return Err(NOT_DOWNLOADED.into());
        };
        if target != expected {
            return Err(format!(
                "{} doesn't point to its pinned download.",
                model.filename
            ));
        }
        blob
    } else if info.file_type().is_file() {
        pointer.clone()
    } else {
        return Err(NOT_DOWNLOADED.into());
    };
    let mismatch = || {
        format!(
            "{} failed its size or checksum check. Run `lf speakers download`.",
            model.filename
        )
    };
    let mut handle = fs::File::open(&file).map_err(|e| format!("{}: {e}", model.filename))?;
    let meta = handle
        .metadata()
        .map_err(|e| format!("{}: {e}", model.filename))?;
    if !meta.is_file() || meta.len() != model.size_bytes {
        return Err(mismatch());
    }
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = handle
            .read(&mut buffer)
            .map_err(|e| format!("{}: {e}", model.filename))?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    let hex: String = digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if hex != model.sha256 {
        return Err(mismatch());
    }
    Ok(pointer)
}
