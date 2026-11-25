//! The voiceprint export ZIP (`registry.export_zip` / `_read_bundle` /
//! `write_bundle`): `registry.json` then `<id>.npy` per usable voiceprint,
//! deflated, dated 1980-01-01, mode 0600.
//!
//! Reading is for untrusted bytes: the whole archive and every member are
//! size-capped before anything is decompressed, each read is bounded, and
//! nothing is written unless the entire bundle validates.

use super::{
    atomic_write, npy,
    registry::{entry, registry_json, RegistryError, Result, Voiceprint, REGISTRY_FILE},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
};
use zip::{write::SimpleFileOptions, CompressionMethod, DateTime, ZipArchive, ZipWriter};

pub const MAX_EXPORT_BYTES: usize = 64 * 1024 * 1024;

const NOT_EXPORT: RegistryError = RegistryError::Other("not a speaker export");
const UNEXPECTED: RegistryError =
    RegistryError::Other("the speaker export contains an unexpected file");

/// The export ZIP of `items` (voiceprints with their unit vectors).
pub fn export(items: &[(Voiceprint, Vec<f32>)]) -> Result<Vec<u8>> {
    let meta = registry_json(items.iter().map(|(vp, _)| vp.public()).collect());
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
        .unix_permissions(0o600);
    let failed = |_| RegistryError::Other("speaker registry error");
    zip.start_file(REGISTRY_FILE, options).map_err(failed)?;
    zip.write_all(meta.as_bytes())?;
    for (vp, vector) in items {
        zip.start_file(format!("{}.npy", vp.id), options)
            .map_err(failed)?;
        zip.write_all(&npy::to_bytes(vector))?;
    }
    Ok(zip.finish().map_err(failed)?.into_inner())
}

/// A validated bundle: registry entries (public shape) and `{npy name: bytes}`.
pub struct Bundle {
    pub entries: Vec<Value>,
    pub files: BTreeMap<String, Vec<u8>>,
}

/// Validate export ZIP bytes with `_read_bundle`'s limits and messages.
pub fn read(data: &[u8]) -> Result<Bundle> {
    if data.len() > MAX_EXPORT_BYTES {
        return Err(NOT_EXPORT);
    }
    let mut archive = ZipArchive::new(Cursor::new(data)).map_err(|_| NOT_EXPORT)?;
    let mut files = BTreeMap::new();
    let mut meta = None;
    let mut total: u64 = 0;
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).map_err(|_| NOT_EXPORT)?;
        let name = member.name().map_err(|_| NOT_EXPORT)?.into_owned();
        let is_registry = name == REGISTRY_FILE;
        let limit = if is_registry {
            super::registry::MAX_REGISTRY_FILE_BYTES
        } else {
            npy::MAX_FILE_BYTES as u64
        };
        let npy_name = name
            .strip_suffix(".npy")
            .is_some_and(super::registry::valid_voiceprint_id);
        if member.is_dir()
            || !(is_registry || npy_name)
            || files.contains_key(&name)
            || (is_registry && meta.is_some())
            || member.size() > limit
        {
            return Err(UNEXPECTED);
        }
        total += member.size();
        if total > MAX_EXPORT_BYTES as u64 {
            return Err(RegistryError::Other("the speaker export is too large"));
        }
        let mut blob = Vec::new();
        (&mut member)
            .take(limit + 1)
            .read_to_end(&mut blob)
            .map_err(|_| NOT_EXPORT)?;
        if blob.len() as u64 > limit {
            return Err(UNEXPECTED);
        }
        if is_registry {
            meta = Some(serde_json::from_slice::<Value>(&blob).map_err(|_| NOT_EXPORT)?);
        } else {
            if npy::from_bytes(&blob).is_none() {
                return Err(RegistryError::Other(
                    "the speaker export contains an unusable vector",
                ));
            }
            files.insert(name, blob);
        }
    }
    let meta = meta.ok_or(NOT_EXPORT)?;
    let raw = match (&meta["schema_version"], meta["voiceprints"].as_array()) {
        (Value::Number(n), Some(list)) if n.as_i64() == Some(1) && n.is_i64() => list,
        _ => return Err(NOT_EXPORT),
    };
    let mut entries = Vec::new();
    for item in raw {
        let vp = entry(item)
            .filter(|vp| files.contains_key(&format!("{}.npy", vp.id)))
            .ok_or(RegistryError::Other("the speaker export is incomplete"))?;
        entries.push(vp.public());
    }
    if files.len() != entries.len() {
        return Err(UNEXPECTED);
    }
    Ok(Bundle { entries, files })
}

/// Validate an export ZIP and write its files into `target` (created 0700
/// when missing, otherwise an empty folder; files 0600, atomic). Nothing is
/// written unless the whole bundle is valid. Returns `(dir, count)`.
pub fn write_bundle(data: &[u8], target: &Path) -> Result<(PathBuf, usize)> {
    let Bundle { entries, files } = read(data)?;
    let target = lf_core::home::expand_user(target).unwrap_or_else(|_| target.to_path_buf());
    let exists = fs::symlink_metadata(&target).is_ok();
    if exists
        && (!target.is_dir()
            || fs::read_dir(&target)
                .map(|mut d| d.next().is_some())
                .unwrap_or(true))
    {
        return Err(RegistryError::Other(
            "the export folder must be new or empty",
        ));
    }
    for (name, blob) in &files {
        atomic_write(&target, name, name, blob)?;
    }
    let count = entries.len();
    atomic_write(
        &target,
        REGISTRY_FILE,
        REGISTRY_FILE,
        registry_json(entries).as_bytes(),
    )?;
    Ok((target, count))
}
