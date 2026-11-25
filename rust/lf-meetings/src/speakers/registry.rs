//! Local voiceprint registry (`localflow/diarization/registry.py` + `staging.py`).
//!
//! `<root>/registry.json` holds names, ids, timestamps and model ids (never a
//! vector); `<root>/<id>.npy` one float32 unit centroid each (0600 in 0700).
//! Callers enforce the recognition gate; nothing here is read otherwise.
//! Voiceprints only ever match vectors from the same embedder and dimension.
//! Error texts are `registry.py`'s (they reach HTTP details and the CLI).

use super::{
    atomic_write, bundle, create_private, is_plain_file, npy, private_dir, RegistryLock,
    SidecarLock, SpeakerData,
};
use crate::clean_speaker_name;
use caseless::Caseless;
use lf_core::symbols::pycompat as py;
use serde_json::{json, Value};
use std::{
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub const SCHEMA_VERSION: i64 = 1;
pub const REGISTRY_FILE: &str = "registry.json";
pub const MAX_REGISTRY_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Cosine-similarity floor of the CI-only mock embedder.
pub const MOCK_MATCH_THRESHOLD: f64 = 0.9;

const INTERRUPTED: &str = "enrolled speakers were forgotten during this enrollment; try again";

#[derive(Debug)]
pub enum RegistryError {
    InvalidName,
    Unknown,
    Ambiguous,
    BadVector(&'static str),
    UnsupportedModel,
    Unreadable,
    Interrupted,
    ErasureIncomplete {
        forgotten: usize,
        failed: usize,
        audio_failed: usize,
    },
    /// Any other registry failure, with a fixed message.
    Other(&'static str),
    Io(io::Error),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => f.write_str("invalid speaker name"),
            Self::Unknown => f.write_str("unknown speaker"),
            Self::Ambiguous => f.write_str(
                "more than one enrolled speaker has this name; use its id (lf speakers list --json)",
            ),
            Self::BadVector(message) | Self::Other(message) => f.write_str(message),
            Self::UnsupportedModel => f.write_str("voice data from this model can't be enrolled"),
            Self::Unreadable => f.write_str("the speaker registry can't be read by this version"),
            Self::Interrupted => f.write_str(INTERRUPTED),
            Self::ErasureIncomplete {
                failed,
                audio_failed,
                ..
            } => {
                let mut parts = Vec::new();
                if *failed > 0 {
                    parts.push(format!("{failed} meeting speaker file(s)"));
                }
                if *audio_failed > 0 {
                    parts.push(format!("{audio_failed} voice-data file(s)"));
                }
                write!(f, "{} could not be erased", parts.join(" and "))
            }
            Self::Io(_) => f.write_str("speaker registry error"),
        }
    }
}
impl std::error::Error for RegistryError {}
impl From<io::Error> for RegistryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
pub type Result<T> = std::result::Result<T, RegistryError>;

#[derive(Clone, Debug, PartialEq)]
pub struct Voiceprint {
    pub id: String,
    pub name: String,
    pub model_id: String,
    pub created: String,
    pub samples: u64,
}

impl Voiceprint {
    /// The API shape (never a vector).
    pub fn public(&self) -> Value {
        json!({"id": self.id, "name": self.name, "created": self.created,
               "samples": self.samples, "model_id": self.model_id})
    }
}

/// Cosine-similarity floor for a model, or `None` (never match).
pub fn match_threshold(model_id: &str) -> Option<f64> {
    if model_id == "mock" {
        return Some(MOCK_MATCH_THRESHOLD);
    }
    lf_diarize::models::DIAR_MODELS
        .iter()
        .find(|m| m.id == model_id)
        .and_then(|m| m.match_threshold)
}

/// The embedder id a `diarization_backend` produces, without loading anything.
pub fn embedder_id_for_backend(backend: &str) -> Option<&'static str> {
    match backend {
        "mock" => Some("mock"),
        "sherpa-onnx" => Some(lf_diarize::models::EMBEDDING.id),
        _ => None,
    }
}

pub fn valid_voiceprint_id(id: &str) -> bool {
    super::store::valid_meeting_id(id)
}

fn valid_model_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_npy_name(name: &str) -> bool {
    name.strip_suffix(".npy").is_some_and(valid_voiceprint_id)
}

/// Temp files this module creates: `.` + owned name + `.` + random + `.tmp`.
fn is_tmp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.').and_then(|r| r.strip_suffix(".tmp")) else {
        return false;
    };
    // `<32 hex>.npy.` is 37 ASCII bytes; checked splits never cut a character.
    let owned = rest.strip_prefix("registry.json.").or_else(|| {
        let (head, random) = (rest.get(..37)?, rest.get(37..)?);
        head.strip_suffix('.').filter(|n| is_npy_name(n))?;
        Some(random)
    });
    owned.is_some_and(|random| !random.contains(['/', '\\']))
}

/// `.enroll-<32 hex>.audio`.
pub fn is_staged_name(name: &str) -> bool {
    name.strip_prefix(".enroll-")
        .and_then(|r| r.strip_suffix(".audio"))
        .is_some_and(valid_voiceprint_id)
}

/// True when `root` is absent or a real directory (not a symlink or file).
pub fn plain_dir_or_missing(root: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(root) {
        Ok(meta) => Ok(meta.file_type().is_dir()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

/// Delete abandoned staged enrollment audio in `root`: `(removed, failed)`.
/// Never creates `root`, never fails, never logs a path or file name.
pub fn sweep(root: &Path) -> (usize, usize) {
    match plain_dir_or_missing(root) {
        Ok(true) => {}
        Ok(false) => {
            log::warn!(
                "skipped the enrollment audio check: the speakers folder is not a plain folder"
            );
            return (0, 0);
        }
        Err(_) => {
            log::warn!("could not check for abandoned enrollment audio");
            return (0, 1);
        }
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return (0, 0)
        }
        Err(_) => {
            log::warn!("could not check for abandoned enrollment audio");
            return (0, 1);
        }
    };
    let (mut removed, mut failed) = (0, 0);
    for name in entries.filter_map(|e| e.ok()?.file_name().into_string().ok()) {
        if !is_staged_name(&name) {
            continue;
        }
        let path = root.join(&name);
        if !is_plain_file(&path) {
            continue; // a symlink or directory is not something this module wrote
        }
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => failed += 1,
        }
    }
    if failed > 0 {
        log::warn!("could not delete {failed} abandoned enrollment audio files");
    } else if removed > 0 {
        log::info!("deleted {removed} abandoned enrollment audio files");
    }
    (removed, failed)
}

/// A usable registry entry with its name re-cleaned.
pub(crate) fn entry(raw: &Value) -> Option<Voiceprint> {
    let id = raw.get("id")?.as_str()?;
    let name = clean_speaker_name(raw.get("name")?.as_str()?)?;
    let model_id = raw.get("model_id")?.as_str()?;
    let created = raw.get("created")?.as_str()?;
    let samples = raw.get("samples")?.as_u64().filter(|&n| n > 0)?;
    valid_voiceprint_id(id).then(|| Voiceprint {
        id: id.into(),
        name,
        model_id: model_id.into(),
        created: created.into(),
        samples,
    })
}

fn sort_key(vp: &Voiceprint) -> (String, String) {
    (vp.name.chars().default_case_fold().collect(), vp.id.clone())
}
fn fold(text: &str) -> String {
    text.chars().default_case_fold().collect()
}

/// `registry.json` text for a list of entries.
pub(crate) fn registry_json(entries: Vec<Value>) -> String {
    py::json_dumps(
        &json!({"schema_version": SCHEMA_VERSION, "voiceprints": entries}),
        Some(2),
        false,
    ) + "\n"
}

/// A private copy of enrollment audio, deleted by [`StagedAudio::finish`] or on drop.
pub struct StagedAudio {
    path: PathBuf,
    done: bool,
}
impl StagedAudio {
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Delete the copy; "already gone" (a forget swept it) is fine.
    pub fn finish(mut self) -> Result<()> {
        self.done = true;
        if discard(&self.path) {
            Ok(())
        } else {
            log::warn!("could not delete staged enrollment audio");
            Err(RegistryError::Other("temporary audio could not be deleted"))
        }
    }
}
impl Drop for StagedAudio {
    fn drop(&mut self) {
        if !self.done && !discard(&self.path) {
            log::warn!("could not delete staged enrollment audio");
        }
    }
}
fn discard(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(e) => e.kind() == io::ErrorKind::NotFound,
    }
}

/// Voiceprints under one root.
pub struct Registry<'a> {
    data: &'a SpeakerData,
    root: PathBuf,
}

impl<'a> Registry<'a> {
    pub(crate) fn new(data: &'a SpeakerData, root: PathBuf) -> Self {
        Self { data, root }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn warn_once(&self, reason: &str) {
        if self.data.first_warning(format!("registry:{reason}")) {
            log::warn!("ignoring the speaker registry: {reason}");
        }
    }

    /// Raw entries; empty when absent; `None` when unreadable or newer.
    fn load_raw(&self) -> Option<Vec<Value>> {
        let path = self.root.join(REGISTRY_FILE);
        let bytes = match fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_REGISTRY_FILE_BYTES => {
                self.warn_once("too large");
                return None;
            }
            Ok(_) => fs::read(&path),
            Err(e) => Err(e),
        };
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Some(Vec::new()),
            Err(_) => {
                self.warn_once("unreadable");
                return None;
            }
        };
        let Ok(data) = serde_json::from_slice::<Value>(&bytes) else {
            self.warn_once("unreadable");
            return None;
        };
        let Some(map) = data.as_object() else {
            self.warn_once("shape");
            return None;
        };
        if let Some(reason) = super::store::version_problem(map.get("schema_version")) {
            self.warn_once(reason);
            return None;
        }
        match map.get("voiceprints") {
            Some(Value::Array(entries)) => Some(entries.clone()),
            _ => {
                self.warn_once("shape");
                None
            }
        }
    }
    fn writable_raw(&self) -> Result<Vec<Value>> {
        self.load_raw().ok_or(RegistryError::Unreadable)
    }
    fn save_raw(&self, entries: Vec<Value>) -> io::Result<()> {
        atomic_write(
            &self.root,
            REGISTRY_FILE,
            REGISTRY_FILE,
            registry_json(entries).as_bytes(),
        )
        .map(drop)
    }
    pub(crate) fn read_vector(&self, id: &str) -> Option<Vec<f32>> {
        if !valid_voiceprint_id(id) {
            return None;
        }
        let path = self.root.join(format!("{id}.npy"));
        if fs::metadata(&path).ok()?.len() > npy::MAX_FILE_BYTES as u64 {
            return None;
        }
        npy::from_bytes(&fs::read(path).ok()?)
    }
    /// Usable voiceprints in enrollment (file) order.
    fn enrolled(&self) -> Vec<Voiceprint> {
        self.load_raw()
            .unwrap_or_default()
            .iter()
            .filter_map(entry)
            .collect()
    }
    /// Usable voiceprints sorted by name (casefold), then id.
    pub fn list(&self) -> Vec<Voiceprint> {
        let mut list = self.enrolled();
        list.sort_by_key(sort_key);
        list
    }
    /// By exact id, else by cleaned name (casefold).
    pub fn find(&self, reference: &str) -> Result<Voiceprint> {
        let list = self.list();
        if let Some(vp) = list.iter().find(|vp| vp.id == reference) {
            return Ok(vp.clone());
        }
        let Some(name) = clean_speaker_name(reference).map(|n| fold(&n)) else {
            return Err(RegistryError::Unknown);
        };
        let mut hits = list.into_iter().filter(|vp| fold(&vp.name) == name);
        match (hits.next(), hits.next()) {
            (None, _) => Err(RegistryError::Unknown),
            (Some(_), Some(_)) => Err(RegistryError::Ambiguous),
            (Some(vp), None) => Ok(vp),
        }
    }
    /// Best same-model, same-dimension voiceprint at or above the model's
    /// threshold; ties go to the earlier enrollment.
    pub fn match_vector(&self, vector: &[f32], model_id: &str) -> Option<(Voiceprint, f64)> {
        let threshold = match_threshold(model_id)?;
        let query = npy::unit_vector(vector.iter().map(|&v| f64::from(v)))?;
        let candidates: Vec<_> = {
            let _held = self.data.lock_registry();
            self.enrolled()
                .into_iter()
                .filter(|vp| vp.model_id == model_id)
                .map(|vp| {
                    let vector = self.read_vector(&vp.id);
                    (vp, vector)
                })
                .collect()
        };
        let mut best: Option<(f64, Voiceprint)> = None;
        for (vp, reference) in candidates {
            let Some(reference) = reference.filter(|r| r.len() == query.len()) else {
                continue;
            };
            let score: f64 = reference
                .iter()
                .zip(&query)
                .map(|(a, b)| f64::from(*a) * f64::from(*b))
                .sum();
            if !score.is_finite() || score < threshold {
                continue;
            }
            if best.as_ref().is_none_or(|(top, _)| score > *top) {
                best = Some((score, vp));
            }
        }
        best.map(|(score, vp)| (vp, score))
    }

    /// Store the normalised mean of `vectors` as a new voiceprint. With
    /// `expected_epoch` (the epoch read before the vectors were computed), a
    /// forget since then refuses instead of recreating voice data.
    pub fn enroll(
        &self,
        _held: &RegistryLock<'_>,
        name: &str,
        vectors: &[Vec<f32>],
        model_id: &str,
        expected_epoch: Option<u64>,
    ) -> Result<Voiceprint> {
        let clean = clean_speaker_name(name).ok_or(RegistryError::InvalidName)?;
        if !(valid_model_id(model_id) && match_threshold(model_id).is_some()) {
            return Err(RegistryError::UnsupportedModel);
        }
        const NO_SAMPLE: &str = "no usable voice sample";
        let units: Vec<Vec<f32>> = vectors
            .iter()
            .map(|v| npy::unit_vector(v.iter().map(|&x| f64::from(x))))
            .collect::<Option<_>>()
            .filter(|u: &Vec<_>| !u.is_empty())
            .ok_or(RegistryError::BadVector(NO_SAMPLE))?;
        let dim = units[0].len();
        if units.iter().any(|u| u.len() != dim) {
            return Err(RegistryError::BadVector(
                "voice samples have different sizes",
            ));
        }
        let mean = (0..dim)
            .map(|i| units.iter().map(|u| f64::from(u[i])).sum::<f64>() / units.len() as f64);
        let centroid = npy::unit_vector(mean).ok_or(RegistryError::BadVector(NO_SAMPLE))?;
        let vp = Voiceprint {
            id: lf_core::compact_uuid_v4(),
            name: clean,
            model_id: model_id.into(),
            created: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            samples: units.len() as u64,
        };
        if expected_epoch.is_some_and(|epoch| epoch != self.data.forget_epoch()) {
            return Err(RegistryError::Interrupted);
        }
        let mut raw = self.writable_raw()?;
        // Vector first: a crash leaves an orphan .npy that nothing reads and
        // forget --all removes, never an entry without its vector.
        let npy_name = format!("{}.npy", vp.id);
        atomic_write(&self.root, &npy_name, &npy_name, &npy::to_bytes(&centroid))?;
        raw.push(
            json!({"id": vp.id, "name": vp.name, "model_id": vp.model_id,
                        "created": vp.created, "samples": vp.samples}),
        );
        self.save_raw(raw)?;
        log::info!("speaker enrolled ({} samples)", vp.samples);
        Ok(vp)
    }

    /// Rename for later meetings; past transcripts and sidecars stay.
    pub fn rename(&self, _held: &RegistryLock<'_>, id: &str, name: &str) -> Result<()> {
        let clean = clean_speaker_name(name).ok_or(RegistryError::InvalidName)?;
        if !valid_voiceprint_id(id) {
            return Err(RegistryError::Unknown);
        }
        let mut raw = self.writable_raw()?;
        let Some(item) = raw.iter_mut().find(|item| {
            item.get("id").and_then(Value::as_str) == Some(id) && entry(item).is_some()
        }) else {
            return Err(RegistryError::Unknown);
        };
        item["name"] = json!(clean);
        self.save_raw(raw)?;
        Ok(())
    }

    /// Forget one voiceprint (or all with `None`): first bump the epoch, null
    /// every stored centroid in every sidecar and the matching links (all for
    /// `None`), then delete the voiceprint files (and, for all, staged audio).
    /// Takes both locks (sidecar → registry).
    pub fn forget(&self, voiceprint_id: Option<&str>) -> Result<usize> {
        if voiceprint_id.is_some_and(|id| !valid_voiceprint_id(id)) {
            return Err(RegistryError::Unknown);
        }
        let sidecars: SidecarLock<'_> = self.data.lock_sidecars();
        let _registry = self.data.lock_registry();
        // Bump first: an in-flight finalize or enrollment that read the epoch
        // earlier now discards its voice data even if something below fails.
        self.data.bump_epoch();
        let keep = match voiceprint_id {
            Some(id) => {
                let raw = self.writable_raw()?;
                let total = raw.len();
                let keep: Vec<Value> = raw
                    .into_iter()
                    .filter(|item| item.get("id").and_then(Value::as_str) != Some(id))
                    .collect();
                if keep.len() == total {
                    return Err(RegistryError::Unknown);
                }
                Some(keep)
            }
            None => None,
        };
        let (strip_error, changed, failed) = match self
            .data
            .store()
            .strip_voiceprints(&sidecars, voiceprint_id)
        {
            Ok((changed, failed)) => (None, changed, failed),
            Err(error) => (Some(error), 0, 0),
        };
        let mut audio_failed = 0;
        let removed = match (voiceprint_id, keep) {
            (Some(id), Some(keep)) => {
                unlink(&self.root.join(format!("{id}.npy")))?;
                self.save_raw(keep)?;
                1
            }
            _ => {
                let removed = self.load_raw().unwrap_or_default().len();
                // Staged audio first, so a failure below can't skip it.
                audio_failed = sweep(&self.root).1;
                self.remove_owned_files()?;
                removed
            }
        };
        log::info!("speakers forgotten: {removed} voiceprints, {changed} meeting entries");
        if let Some(error) = strip_error {
            return Err(error.into());
        }
        if failed > 0 || audio_failed > 0 {
            return Err(RegistryError::ErasureIncomplete {
                forgotten: removed,
                failed,
                audio_failed,
            });
        }
        Ok(removed)
    }

    /// Delete registry.json, every `<id>.npy` (orphans too) and this module's
    /// temp files; nothing else in the folder.
    fn remove_owned_files(&self) -> io::Result<()> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for name in entries.filter_map(|e| e.ok()?.file_name().into_string().ok()) {
            if name == REGISTRY_FILE || is_npy_name(&name) || is_tmp_name(&name) {
                let path = self.root.join(&name);
                let meta = fs::symlink_metadata(&path);
                if meta.is_ok_and(|m| m.file_type().is_file() || m.file_type().is_symlink()) {
                    unlink(&path)?;
                }
            }
        }
        Ok(())
    }

    /// A private seekable copy of `data` (`.enroll-<hex>.audio`, 0600, in the
    /// 0700 registry folder), created under the registry lock after an epoch
    /// check, so a forget --all either sweeps it or runs before it exists.
    pub fn staged_audio(&self, data: &[u8], expected_epoch: Option<u64>) -> Result<StagedAudio> {
        let path = self
            .root
            .join(format!(".enroll-{}.audio", lf_core::compact_uuid_v4()));
        let _held = self.data.lock_registry();
        if expected_epoch.is_some_and(|epoch| epoch != self.data.forget_epoch()) {
            return Err(RegistryError::Interrupted);
        }
        const PREPARE: RegistryError = RegistryError::Other("could not prepare the audio");
        if !plain_dir_or_missing(&self.root).map_err(|_| PREPARE)? {
            return Err(PREPARE);
        }
        private_dir(&self.root).map_err(|_| PREPARE)?;
        let mut file = create_private(&path).map_err(|_| PREPARE)?;
        let staged = StagedAudio { path, done: false };
        file.write_all(data)
            .and_then(|()| file.flush())
            .map_err(|_| PREPARE)?; // `staged` deletes the partial copy on drop
        Ok(staged)
    }

    /// A ZIP of `registry.json` + `<id>.npy` for every usable voiceprint.
    pub fn export_zip(&self) -> Result<Vec<u8>> {
        let items: Vec<(Voiceprint, Vec<f32>)> = {
            let _held = self.data.lock_registry();
            self.list()
                .into_iter()
                .filter_map(|vp| {
                    let vector = self.read_vector(&vp.id)?;
                    Some((vp, vector))
                })
                .collect()
        };
        let data = bundle::export(&items)?;
        log::info!("speaker registry exported: {} voiceprints", items.len());
        Ok(data)
    }
}

fn unlink(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
