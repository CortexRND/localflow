//! Per-meeting speaker sidecars (`localflow/diarization/store.py`).
//!
//! `<root>/<32 hex>.json`: speaker identities plus segment timings and ids,
//! never transcript text. Records are kept as JSON values so keys this version
//! does not know (and newer schema versions, when scrubbed) survive rewrites.
//! `centroid` / `voiceprint_id` are only non-null when speaker recognition
//! was on; [`MeetingSpeakerStore::strip_voiceprints`] erases them.

use super::{atomic_write, is_plain_file, py_round, SidecarLock, SpeakerData};
use crate::Segment;
use lf_core::symbols::pycompat as py;
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

pub const SCHEMA_VERSION: i64 = 1;
pub const MAX_CENTROID_DIM: usize = 4096;
const PUBLIC_KEYS: [&str; 5] = ["id", "label", "name", "source", "seconds"];

/// True for a 32-char lowercase hex id (`uuid4().hex`).
pub fn valid_meeting_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `.<meeting id>.<8 of [a-z0-9_]>.tmp`, the staging names `save` creates.
fn is_staging_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.').and_then(|r| r.strip_suffix(".tmp")) else {
        return false;
    };
    let Some((id, token)) = rest.split_once('.') else {
        return false;
    };
    valid_meeting_id(id)
        && token.len() == 8
        && token
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// MM:SS exactly like `Segment.stamp`.
pub fn stamp(seconds: f64) -> String {
    let s = seconds as i64;
    format!("{:02}:{:02}", s.div_euclid(60), s.rem_euclid(60))
}

/// One speaker as the stop path hands it to [`build_record`].
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerEntry {
    pub id: String,
    pub source: String,
    pub label: String,
    pub name: Option<String>,
    pub voiceprint_id: Option<String>,
    pub seconds: f64,
    /// Already rounded to 6 decimals; only with recognition on.
    pub centroid: Option<Vec<f64>>,
}

fn number(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// A sidecar record from stop-payload speakers and the saved segments.
#[allow(clippy::too_many_arguments)]
pub fn build_record(
    meeting_id: &str,
    log_path: &str,
    notes_path: &str,
    started_at: &str,
    embedder: Option<&str>,
    status: &str,
    speakers: &[SpeakerEntry],
    segments: &[Segment],
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "meeting_id": meeting_id,
        "log_path": log_path,
        "notes_path": notes_path,
        "started_at": started_at,
        "embedder": embedder,
        "status": status,
        "speakers": speakers.iter().map(|sp| json!({
            "id": sp.id,
            "source": sp.source,
            "label": sp.label,
            "name": sp.name,
            "voiceprint_id": sp.voiceprint_id,
            "seconds": number(sp.seconds),
            "centroid": sp.centroid.as_ref().map(|c| c.iter().map(|&x| number(x)).collect::<Vec<_>>()),
        })).collect::<Vec<_>>(),
        "segments": segments.iter().map(|s| json!({
            "start": number(s.offset_seconds),
            "end": s.end_seconds.map(number),
            "source": s.source,
            "speaker_id": s.speaker_id,
        })).collect::<Vec<_>>(),
    })
}

fn speakers(record: &Value) -> &[Value] {
    record["speakers"].as_array().map_or(&[], Vec::as_slice)
}
fn segments(record: &Value) -> &[Value] {
    record["segments"].as_array().map_or(&[], Vec::as_slice)
}

/// Speakers without voiceprint data or any private field.
pub fn public_speakers(record: &Value) -> Value {
    Value::Array(
        speakers(record)
            .iter()
            .map(|sp| {
                Value::Object(
                    PUBLIC_KEYS
                        .iter()
                        .map(|k| (k.to_string(), sp.get(*k).cloned().unwrap_or(Value::Null)))
                        .collect(),
                )
            })
            .collect(),
    )
}

pub fn find_speaker<'a>(record: &'a Value, speaker_id: &str) -> Option<&'a Value> {
    speakers(record).iter().find(|sp| sp["id"] == speaker_id)
}

fn labels(record: &Value) -> BTreeMap<&str, &str> {
    speakers(record)
        .iter()
        .filter_map(|sp| Some((sp["id"].as_str()?, sp["label"].as_str()?)))
        .collect()
}

/// `(identity, label)` of a segment, or `None` for an unknown speaker id.
fn identity(labels: &BTreeMap<&str, &str>, seg: &Value) -> Option<(String, String)> {
    if let Some(sid) = seg["speaker_id"].as_str() {
        return labels
            .get(sid)
            .map(|label| (format!("id:{sid}"), label.to_string()));
    }
    let source = seg["source"].as_str().unwrap_or_default();
    Some(match source {
        "me" => ("me".into(), "Me".into()),
        "others" => (format!("ch:{source}"), "Others".into()),
        _ => (format!("ch:{source}"), source.into()),
    })
}

/// The `speakers:` frontmatter list: one label per identity, first appearance.
pub fn frontmatter_speakers(record: &Value) -> Vec<String> {
    let known = labels(record);
    let mut seen: Vec<(String, String)> = Vec::new();
    for seg in segments(record) {
        let Some((key, label)) = identity(&known, seg) else {
            continue;
        };
        if key.starts_with("ch:") || seen.iter().any(|(k, _)| *k == key) {
            continue;
        }
        seen.push((key, label));
    }
    seen.into_iter().map(|(_, label)| label).collect()
}

/// Per stamp, one flag per transcript line rendered with this speaker's
/// current label, in segment order: true where the line is this speaker's.
/// Only stamps with at least one line of this speaker are included.
pub fn speaker_lines(record: &Value, speaker_id: &str) -> BTreeMap<String, Vec<bool>> {
    let Some(label) = find_speaker(record, speaker_id).and_then(|sp| sp["label"].as_str()) else {
        return BTreeMap::new();
    };
    let known = labels(record);
    let mut flags: BTreeMap<String, Vec<bool>> = BTreeMap::new();
    for seg in segments(record) {
        if identity(&known, seg).is_some_and(|(_, l)| l == label) {
            flags
                .entry(stamp(seg["start"].as_f64().unwrap_or(0.0)))
                .or_default()
                .push(seg["speaker_id"] == speaker_id);
        }
    }
    flags.retain(|_, v| v.iter().any(|&f| f));
    flags
}

fn speaker_mut<'a>(record: &'a mut Value, speaker_id: &str) -> Option<&'a mut Map<String, Value>> {
    record["speakers"]
        .as_array_mut()?
        .iter_mut()
        .find(|sp| sp["id"] == speaker_id)?
        .as_object_mut()
}

/// A copy with that speaker's name and label set to `name`.
pub fn rename_speaker(record: &Value, speaker_id: &str, name: &str) -> Option<Value> {
    let mut new = record.clone();
    let speaker = speaker_mut(&mut new, speaker_id)?;
    speaker.insert("name".into(), json!(name));
    speaker.insert("label".into(), json!(name));
    Some(new)
}

/// A copy with `from`'s segments and seconds moved to `into` (seconds rounded
/// to one decimal) and `from` removed.
pub fn merge_speakers(record: &Value, from: &str, into: &str) -> Option<Value> {
    if from == into {
        return None;
    }
    let from_seconds = find_speaker(record, from)?["seconds"].as_f64()?;
    let mut new = record.clone();
    let target = speaker_mut(&mut new, into)?;
    let total = target.get("seconds").and_then(Value::as_f64).unwrap_or(0.0) + from_seconds;
    target.insert("seconds".into(), number(py_round(total, 1)));
    for seg in new["segments"].as_array_mut().into_iter().flatten() {
        if seg["speaker_id"] == from {
            seg["speaker_id"] = json!(into);
        }
    }
    if let Some(list) = new["speakers"].as_array_mut() {
        list.retain(|sp| sp["id"] != from);
    }
    Some(new)
}

fn optional_str(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null) | Some(Value::String(_)))
}
fn real(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Number(_)))
}
fn centroid_ok(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::Array(items)) => {
            !items.is_empty()
                && items.len() <= MAX_CENTROID_DIM
                && items.iter().all(Value::is_number)
        }
        _ => false,
    }
}

/// An integer `schema_version` (never a bool or float) of at least 1 that this
/// version can read; otherwise `"newer schema"` or `"shape"`.
pub(crate) fn version_problem(version: Option<&Value>) -> Option<&'static str> {
    match version {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => match n.as_i64() {
            Some(v) if v < 1 => Some("shape"),
            Some(v) if v <= SCHEMA_VERSION => None,
            _ => Some("newer schema"),
        },
        _ => Some("shape"),
    }
}

/// `_problem`: why a record is unusable, or `None`.
pub fn problem(record: &Value, meeting_id: &str) -> Option<&'static str> {
    let Some(map) = record.as_object() else {
        return Some("shape");
    };
    if let Some(reason) = version_problem(map.get("schema_version")) {
        return Some(reason);
    }
    if map.get("meeting_id").and_then(Value::as_str) != Some(meeting_id)
        || !map.get("log_path").is_some_and(Value::is_string)
    {
        return Some("shape");
    }
    let Some(list) = map.get("speakers").and_then(Value::as_array) else {
        return Some("shape");
    };
    let mut ids = Vec::new();
    for sp in list {
        let Some(sp) = sp.as_object() else {
            return Some("shape");
        };
        if !["id", "label", "source"]
            .iter()
            .all(|k| sp.get(*k).is_some_and(Value::is_string))
            || !optional_str(sp.get("name"))
            || !real(sp.get("seconds"))
            || !optional_str(sp.get("voiceprint_id"))
            || !centroid_ok(sp.get("centroid"))
        {
            return Some("shape");
        }
        let id = sp["id"].as_str().unwrap_or_default();
        if ids.contains(&id) {
            return Some("shape");
        }
        ids.push(id);
    }
    let Some(list) = map.get("segments").and_then(Value::as_array) else {
        return Some("shape");
    };
    for seg in list {
        let Some(seg) = seg.as_object() else {
            return Some("shape");
        };
        let end_ok = matches!(
            seg.get("end"),
            None | Some(Value::Null) | Some(Value::Number(_))
        );
        if !real(seg.get("start"))
            || !end_ok
            || !seg.get("source").is_some_and(Value::is_string)
            || !optional_str(seg.get("speaker_id"))
        {
            return Some("shape");
        }
    }
    None
}

/// Null every stored centroid, and `voiceprint_id` where it equals the given
/// one (every one with `None`), in place. Speakers changed, or `None` when the
/// record has no readable speaker list. Absent keys stay absent.
pub fn scrub(record: &mut Value, voiceprint_id: Option<&str>) -> Option<usize> {
    let list = record
        .as_object_mut()?
        .get_mut("speakers")?
        .as_array_mut()?;
    if !list.iter().all(Value::is_object) {
        return None;
    }
    let mut changed = 0;
    for sp in list.iter_mut().filter_map(Value::as_object_mut) {
        let mut hit = false;
        if sp.get("centroid").is_some_and(|c| !c.is_null()) {
            sp.insert("centroid".into(), Value::Null);
            hit = true;
        }
        let linked = sp.get("voiceprint_id").filter(|v| !v.is_null()).cloned();
        if let Some(linked) = linked {
            if voiceprint_id.is_none_or(|id| linked == id) {
                sp.insert("voiceprint_id".into(), Value::Null);
                hit = true;
            }
        }
        changed += usize::from(hit);
    }
    Some(changed)
}

/// Per-meeting speaker sidecars under one root.
pub struct MeetingSpeakerStore<'a> {
    data: &'a SpeakerData,
    root: PathBuf,
}

impl<'a> MeetingSpeakerStore<'a> {
    pub(crate) fn new(data: &'a SpeakerData, root: PathBuf) -> Self {
        Self { data, root }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn path(&self, meeting_id: &str) -> Option<PathBuf> {
        valid_meeting_id(meeting_id).then(|| self.root.join(format!("{meeting_id}.json")))
    }
    fn warn_once(&self, meeting_id: &str, reason: &str) {
        if self
            .data
            .first_warning(format!("meeting:{meeting_id}:{reason}"))
        {
            log::warn!("ignoring meeting speaker data {meeting_id}: {reason}");
        }
    }
    /// The stored record, or `None` when absent or unusable. Never fails.
    pub fn load(&self, meeting_id: &str) -> Option<Value> {
        let target = self.path(meeting_id)?;
        let bytes = match fs::read(&target) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(_) => {
                self.warn_once(meeting_id, "unreadable");
                return None;
            }
        };
        let Ok(record) = serde_json::from_slice::<Value>(&bytes) else {
            self.warn_once(meeting_id, "unreadable");
            return None;
        };
        if let Some(reason) = problem(&record, meeting_id) {
            self.warn_once(meeting_id, reason);
            return None;
        }
        Some(record)
    }
    /// Write the record atomically (0600 file in a 0700 directory).
    pub fn save(&self, held: &SidecarLock<'_>, record: &Value) -> io::Result<PathBuf> {
        let meeting_id = record["meeting_id"].as_str().unwrap_or_default();
        if !valid_meeting_id(meeting_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid meeting id",
            ));
        }
        self.write(held, meeting_id, record)
    }
    fn write(
        &self,
        _held: &SidecarLock<'_>,
        meeting_id: &str,
        record: &Value,
    ) -> io::Result<PathBuf> {
        let data = py::json_dumps(record, Some(2), false) + "\n";
        atomic_write(
            &self.root,
            &format!("{meeting_id}.json"),
            meeting_id,
            data.as_bytes(),
        )
    }

    /// Erase stored voice data from every sidecar (see `store.py`): returns
    /// `(speakers changed, files that could not be erased)`. A folder that
    /// can't be listed (other than missing) is an error.
    pub fn strip_voiceprints(
        &self,
        held: &SidecarLock<'_>,
        voiceprint_id: Option<&str>,
    ) -> io::Result<(usize, usize)> {
        let mut names: Vec<String> = match fs::read_dir(&self.root) {
            Ok(entries) => entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(e) => return Err(e),
        };
        names.sort();
        let (mut changed, mut failed) = (0, 0);
        for name in names {
            let path = self.root.join(&name);
            if is_staging_name(&name) {
                if !is_plain_file(&path) {
                    continue; // not something `save` creates
                }
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(_) => failed += 1,
                }
                continue;
            }
            let Some(meeting_id) = name.strip_suffix(".json").filter(|id| valid_meeting_id(id))
            else {
                continue;
            };
            let Ok(mut record) = fs::read(&path)
                .map_err(drop)
                .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(drop))
            else {
                failed += 1;
                continue;
            };
            let Some(hits) = scrub(&mut record, voiceprint_id) else {
                failed += 1;
                continue;
            };
            if hits > 0 {
                if self.write(held, meeting_id, &record).is_err() {
                    failed += 1;
                    continue;
                }
                changed += hits;
            }
        }
        if failed > 0 {
            log::warn!("could not erase voice data from {failed} meeting speaker files");
        }
        Ok((changed, failed))
    }
}
