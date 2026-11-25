//! Meeting speaker routes (B8: `/api/meetings/{id}/speakers*`) and the
//! opt-in voiceprint routes (B9: `/api/speakers*`, `/api/diarization/status`),
//! with `localflow/server.py`'s statuses and details.
//!
//! The voiceprint routes need both speaker flags (re-checked here even though
//! `lf-server` checks before reading the body); raw enrollment audio and the
//! export are additionally only for direct requests from this computer.
//! Nothing here logs a name, a vector, audio, or a path from a request.

use crate::{text, MeetingsBackend};
use lf_core::config::Config;
use lf_meetings::{
    clean_speaker_name,
    speakers::{
        self, recognition_on,
        registry::{embedder_id_for_backend, Voiceprint},
        relabel::{relabel_transcript, vault_notes_path, RelabelError},
        store::{
            find_speaker, frontmatter_speakers, merge_speakers, public_speakers, rename_speaker,
            speaker_lines,
        },
        RegistryError, SpeakerData,
    },
};
use lf_server::backend::{BackendRequest, BackendResponse, Download};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

type Reply<T = Value> = Result<T, BackendResponse>;

/// Enrollment clips: longer is refused, never cut (`server.ENROLL_MAX_SECONDS`).
pub const ENROLL_MAX_SECONDS: usize = 300;
const MAX_UPLOAD_BYTES: usize = lf_server::MAX_UPLOAD_BYTES;
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);
const FFMPEG_INPUT_FORMATS: &str = "matroska,webm,ogg,mov,mp4,m4a,wav,mp3,aac,flac,caf";

fn error(status: u16, detail: impl Into<String>) -> BackendResponse {
    BackendResponse::error(status, detail)
}
fn ok(body: Value) -> BackendResponse {
    BackendResponse { status: 200, body }
}

/// `_registry_errors`: fixed, safe messages (never a name or a vector).
fn registry_error(e: RegistryError) -> BackendResponse {
    match e {
        RegistryError::InvalidName => error(400, "invalid speaker name"),
        RegistryError::Unknown => error(404, "unknown speaker"),
        RegistryError::Ambiguous | RegistryError::Interrupted | RegistryError::Unreadable => {
            error(409, e.to_string())
        }
        RegistryError::BadVector(_) | RegistryError::UnsupportedModel => error(400, e.to_string()),
        RegistryError::ErasureIncomplete {
            forgotten,
            failed,
            audio_failed,
        } => {
            let mut parts = Vec::new();
            if failed > 0 {
                parts.push(format!("voice data in {failed} meeting file(s)"));
            }
            if audio_failed > 0 {
                parts.push(format!("{audio_failed} voice-data file(s)"));
            }
            error(
                500,
                format!(
                    "forgot {forgotten} speaker(s), but {} could not be erased",
                    parts.join(" and ")
                ),
            )
        }
        RegistryError::Other(_) | RegistryError::Io(_) => {
            log::warn!("speaker registry error");
            error(500, "speaker registry error")
        }
    }
}

/// A FastAPI `body: dict` (422 envelope otherwise).
fn dict_body(request: &BackendRequest) -> Reply {
    let invalid = |detail: Value| BackendResponse {
        status: 422,
        body: json!({ "detail": [detail] }),
    };
    let missing = json!({"type":"missing","loc":["body"],"msg":"Field required","input":null});
    if request.body.is_empty() {
        return Err(invalid(missing));
    }
    let value: Value = serde_json::from_slice(&request.body).map_err(|_| {
        invalid(json!({"type":"json_invalid","loc":["body",0],"msg":"JSON decode error","input":{},"ctx":{"error":"Invalid JSON"}}))
    })?;
    match value {
        Value::Null => Err(invalid(missing)),
        Value::Object(_) => Ok(value),
        other => Err(invalid(
            json!({"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary","input":other}),
        )),
    }
}

/// `_json_object`: a JSON object body (400 otherwise; size capped by lf-server).
fn json_object(request: &BackendRequest) -> Reply {
    match serde_json::from_slice::<Value>(&request.body) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err(error(400, "expected a JSON object")),
        Err(_) => Err(error(400, "invalid JSON")),
    }
}

fn clean_name(value: &Value) -> Option<String> {
    value.as_str().and_then(clean_speaker_name)
}

fn content_type(request: &BackendRequest) -> String {
    request
        .headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

impl MeetingsBackend {
    /// The live recognition gate: the freshest config available.
    pub(crate) fn recognition_live(&self, snapshot: &Config) -> bool {
        match &self.config_reader {
            Some(reader) => reader().is_ok_and(|config| recognition_on(&config)),
            None => recognition_on(snapshot),
        }
    }
    fn require_recognition(&self, config: &Config) -> Reply<()> {
        if self.recognition_live(config) {
            Ok(())
        } else {
            Err(error(403, "speaker recognition is off"))
        }
    }
    fn data(&self) -> &SpeakerData {
        &self.speakers
    }

    /// Routes of this module, or `None` for any other path.
    pub(crate) fn speaker_route(
        &self,
        request: &BackendRequest,
        local: bool,
    ) -> Option<BackendResponse> {
        let method = request.method.as_str();
        let path = request.path.as_str();
        let result = if let Some(rest) = path.strip_prefix("/api/meetings/") {
            let parts: Vec<&str> = rest.split('/').collect();
            match (method, parts.as_slice()) {
                ("GET", [id, "speakers"]) => self.meeting_speakers(id),
                ("POST", [id, "speakers", "merge"]) => self.merge(request, id),
                ("PUT", [id, "speakers", sid]) => self.rename(request, id, sid),
                _ => return None,
            }
        } else if path == "/api/diarization/status" && method == "GET" {
            Ok(ok(self.diarization_status(&request.config)))
        } else if path == "/api/speakers" {
            match method {
                "GET" => self.list(&request.config),
                "POST" => self.enroll(request, local),
                "DELETE" => self.forget(&request.config, None),
                _ => return None,
            }
        } else {
            let reference = path.strip_prefix("/api/speakers/")?;
            match method {
                // Reached only without the binary download seam: refuse, never JSON vectors.
                "GET" if reference == "export" => {
                    // Not local without the server's peer check: gate, then refuse.
                    self.export(&request.config, false)
                        .map(|_| error(500, "Internal Server Error"))
                }
                "PATCH" => self.rename_voiceprint(request, reference),
                "DELETE" => self.forget(&request.config, Some(reference)),
                _ => return None,
            }
        };
        Some(result.unwrap_or_else(|response| response))
    }

    pub(crate) fn diarization_status(&self, config: &Config) -> Value {
        let cache = self
            .model_cache
            .clone()
            .unwrap_or_else(speakers::default_hub_cache);
        let mut status = speakers::diarization_status(&cache, config);
        // No download runs in this server (models come from `lf speakers download`).
        status["download"] =
            json!({"state": "idle", "done_bytes": 0, "total_bytes": 0, "error": null});
        status
    }

    // ------------------------------------------------- B8: meeting speakers

    fn record(&self, meeting_id: &str) -> Reply {
        self.data()
            .store()
            .load(meeting_id)
            .ok_or_else(|| error(404, "no speaker data for this meeting"))
    }
    fn meeting_speakers(&self, meeting_id: &str) -> Reply<BackendResponse> {
        let record = self.record(meeting_id)?;
        Ok(ok(
            json!({"meeting_id": meeting_id, "speakers": public_speakers(&record)}),
        ))
    }
    fn rename(
        &self,
        request: &BackendRequest,
        meeting_id: &str,
        speaker_id: &str,
    ) -> Reply<BackendResponse> {
        let body = dict_body(request)?;
        let name = clean_name(&body["name"]).ok_or_else(|| error(400, "invalid speaker name"))?;
        let held = self.data().lock_sidecars();
        let record = self.record(meeting_id)?;
        let old = find_speaker(&record, speaker_id)
            .and_then(|sp| sp["label"].as_str())
            .ok_or_else(|| error(404, "unknown speaker"))?
            .to_string();
        if old == name {
            return Ok(ok(
                json!({"meeting_id": meeting_id, "speakers": public_speakers(&record)}),
            ));
        }
        let updated = rename_speaker(&record, speaker_id, &name)
            .ok_or_else(|| error(404, "unknown speaker"))?;
        let lines = speaker_lines(&record, speaker_id);
        self.apply_edit(
            &request.config,
            &held,
            meeting_id,
            &record,
            updated,
            &lines,
            &old,
            &name,
        )
    }
    fn merge(&self, request: &BackendRequest, meeting_id: &str) -> Reply<BackendResponse> {
        let body = dict_body(request)?;
        let (from, into) = match (body["from"].as_str(), body["into"].as_str()) {
            (Some(f), Some(i)) if !f.is_empty() && !i.is_empty() && f != i => (f, i),
            _ => {
                return Err(error(
                    400,
                    "from and into must be two different speaker ids",
                ))
            }
        };
        let held = self.data().lock_sidecars();
        let record = self.record(meeting_id)?;
        let (Some(source), Some(target)) =
            (find_speaker(&record, from), find_speaker(&record, into))
        else {
            return Err(error(404, "unknown speaker"));
        };
        let new = target["label"]
            .as_str()
            .and_then(clean_speaker_name)
            .ok_or_else(|| error(409, "speaker data is not usable"))?;
        let old = source["label"].as_str().unwrap_or_default().to_string();
        let updated =
            merge_speakers(&record, from, into).ok_or_else(|| error(404, "unknown speaker"))?;
        let lines = speaker_lines(&record, from);
        self.apply_edit(
            &request.config,
            &held,
            meeting_id,
            &record,
            updated,
            &lines,
            &old,
            &new,
        )
    }
    /// `_sidecar_transcript`: the transcript inside the vault, never the notes file.
    fn sidecar_transcript(&self, config: &Config, record: &Value) -> Reply<std::path::PathBuf> {
        let log_path = record["log_path"].as_str().unwrap_or_default();
        if let Some(resolved) = vault_notes_path(log_path, text(config, "vault_path")) {
            let is_notes = record["notes_path"].as_str().is_some_and(|notes| {
                lf_core::home::expand_user(Path::new(notes))
                    .ok()
                    .and_then(|p| std::fs::canonicalize(p).ok())
                    .is_some_and(|p| p == resolved)
            });
            if is_notes {
                return Err(error(409, "transcript is the meeting notes file"));
            }
            return Ok(resolved);
        }
        let exists = lf_core::home::expand_user(Path::new(log_path)).is_ok_and(|p| p.is_file());
        Err(if exists {
            error(409, "transcript is outside the meeting vault")
        } else {
            error(404, "transcript file not found")
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn apply_edit(
        &self,
        config: &Config,
        held: &speakers::SidecarLock<'_>,
        meeting_id: &str,
        record: &Value,
        updated: Value,
        lines: &BTreeMap<String, Vec<bool>>,
        old: &str,
        new: &str,
    ) -> Reply<BackendResponse> {
        let path = self.sidecar_transcript(config, record)?;
        let frontmatter = frontmatter_speakers(&updated);
        let changed =
            relabel_transcript(&path, lines, old, new, Some(&frontmatter), Some(meeting_id))
                .map_err(|e| match e {
                    RelabelError::NotFound => error(404, "transcript file not found"),
                    RelabelError::Changed(message) => error(409, message),
                    RelabelError::NotReadable => error(409, "transcript is not readable"),
                    RelabelError::Io(_) => {
                        log::warn!("rewriting transcript failed");
                        error(500, "could not rewrite the transcript")
                    }
                })?;
        if self.data().store().save(held, &updated).is_err() {
            log::warn!("saving meeting speaker data failed");
            return Err(error(
                500,
                "transcript updated but speaker data could not be saved",
            ));
        }
        let count = updated["speakers"].as_array().map_or(0, Vec::len);
        log::info!("meeting speakers updated: {changed} transcript lines, {count} speakers");
        let public = public_speakers(&updated);
        {
            let mut state = self.state();
            if let Some(last) = state.last_saved.as_mut() {
                if last["meeting_id"] == meeting_id {
                    last["speakers"] = public.clone();
                }
            }
        }
        Ok(ok(json!({"meeting_id": meeting_id, "speakers": public})))
    }

    // --------------------------------------------------- B9: voiceprints

    fn list(&self, config: &Config) -> Reply<BackendResponse> {
        self.require_recognition(config)?;
        let speakers: Vec<Value> = self
            .data()
            .registry()
            .list()
            .iter()
            .map(Voiceprint::public)
            .collect();
        let embedder = embedder_id_for_backend(text(config, "diarization_backend"));
        Ok(ok(json!({"speakers": speakers, "embedder": embedder})))
    }

    fn enroll(&self, request: &BackendRequest, local: bool) -> Reply<BackendResponse> {
        self.require_recognition(&request.config)?;
        let vp = match content_type(request).as_str() {
            "application/json" => self.enroll_from_meeting(request, json_object(request)?)?,
            "multipart/form-data" => {
                if !local {
                    return Err(error(
                        403,
                        "enrolling from audio is only available on this computer",
                    ));
                }
                let (raw_name, data) = enroll_upload(request)?;
                let name = raw_name
                    .as_deref()
                    .and_then(clean_speaker_name)
                    .ok_or_else(|| error(400, "invalid speaker name"))?;
                self.enroll_from_audio(&request.config, &name, data)?
            }
            _ => return Err(error(415, "send JSON or a multipart audio upload")),
        };
        Ok(BackendResponse {
            status: 201,
            body: vp.public(),
        })
    }

    fn enroll_from_meeting(&self, request: &BackendRequest, body: Value) -> Reply<Voiceprint> {
        let config = &request.config;
        self.require_recognition(config)?; // the gate may have closed while the body was read
        let name = clean_name(&body["name"]).ok_or_else(|| error(400, "invalid speaker name"))?;
        let (Some(meeting_id), Some(speaker_id)) =
            (body["meeting_id"].as_str(), body["speaker_id"].as_str())
        else {
            return Err(error(400, "meeting_id and speaker_id are required"));
        };
        let data = self.data();
        // forget takes the sidecar lock first: no enrollment from erased data.
        let held = data.lock_sidecars();
        let mut record = self.record(meeting_id)?;
        let speaker =
            find_speaker(&record, speaker_id).ok_or_else(|| error(404, "unknown speaker"))?;
        let centroid: Option<Vec<f32>> = speaker["centroid"]
            .as_array()
            .filter(|c| !c.is_empty())
            .map(|c| {
                c.iter()
                    .map(|v| v.as_f64().unwrap_or(f64::NAN) as f32)
                    .collect()
            });
        let (Some(embedder), Some(centroid)) =
            (record["embedder"].as_str().map(str::to_owned), centroid)
        else {
            return Err(error(409, "this meeting has no stored voice data"));
        };
        self.require_recognition(config)?; // last check right before voice data is stored
        let vp = {
            let registry = data.lock_registry();
            data.registry()
                .enroll(&registry, &name, &[centroid], &embedder, None)
                .map_err(registry_error)?
        };
        if let Some(sp) = record["speakers"]
            .as_array_mut()
            .and_then(|list| list.iter_mut().find(|sp| sp["id"] == speaker_id))
        {
            sp["voiceprint_id"] = json!(vp.id);
        }
        if data.store().save(&held, &record).is_err() {
            log::warn!("linking the meeting speaker failed");
        }
        Ok(vp)
    }

    /// Decode, embed and enroll. The audio is not retained; only an M4A/MP4
    /// that needs seeking is briefly copied to a private staged file.
    fn enroll_from_audio(&self, config: &Config, name: &str, data: Vec<u8>) -> Reply<Voiceprint> {
        let epoch = self.data().forget_epoch(); // a forget during the slow work voids this
                                                // Resolved up front (no model is loaded) so an embedder that can never
                                                // enroll stops the staged-copy path before raw audio touches disk; its
                                                // 503 is still reported after the pipe decode's own errors.
        let unavailable = |detail: String| error(503, detail);
        let embedder = match self.capture.diarizer(config) {
            Ok(Some(backend)) => match backend.embedder_id() {
                Some(id) => Ok((backend, id)),
                None => Err(unavailable("voice model unavailable".into())),
            },
            Ok(None) => Err(unavailable("voice model unavailable".into())),
            Err(e) => Err(unavailable(e.to_string())),
        };
        let refusal = embedder
            .as_ref()
            .err()
            .map(|e| error(e.status, e.body["detail"].as_str().unwrap_or_default()));
        let audio = self.decode_enrollment_audio(config, data, epoch, refusal)?;
        if audio.is_empty() {
            return Err(error(400, "decoded audio is empty"));
        }
        let threshold = config.field_f64("silence_threshold_dbfs").unwrap_or(-50.0);
        if lf_runtime::engine::rms_dbfs(&audio) < threshold {
            return Err(error(400, "no speech detected"));
        }
        let (mut backend, model_id) = embedder?;
        if let Err(e) = backend.load() {
            let reason = e.to_string();
            return Err(unavailable(if reason.is_empty() {
                "voice model unavailable".into()
            } else {
                reason
            }));
        }
        let vector = backend.embed(&audio).map_err(|_| {
            log::warn!("speaker enrollment embedding failed");
            error(500, "could not analyse the audio")
        })?;
        drop(audio);
        drop(backend);
        self.require_recognition(config)?; // still on after the slow part
        let data = self.data();
        let registry = data.lock_registry();
        data.registry()
            .enroll(&registry, name, &[vector], model_id, Some(epoch))
            .map_err(registry_error)
    }

    /// 16 kHz mono f32, at most [`ENROLL_MAX_SECONDS`]: the stdin pipe first;
    /// only an ISO BMFF clip that produced no audio is retried from a staged copy.
    fn decode_enrollment_audio(
        &self,
        config: &Config,
        data: Vec<u8>,
        epoch: u64,
        unavailable: Option<BackendResponse>,
    ) -> Reply<Vec<f32>> {
        let limit = ENROLL_MAX_SECONDS + 1; // one second past the bound detects overflow
        let mut decoded = run_ffmpeg(Some(&data), None, limit);
        if matches!(&decoded, Ok((_, out)) if out.is_empty()) && is_iso_bmff(&data) {
            self.require_recognition(config)?; // an opt-out meanwhile: never write raw audio
            if let Some(refusal) = unavailable {
                // Nothing could be enrolled: never stage the raw audio for it.
                return Err(refusal);
            }
            let staged = self
                .data()
                .registry()
                .staged_audio(&data, Some(epoch))
                .map_err(registry_error)?;
            decoded = run_ffmpeg(None, Some(staged.path()), limit);
            staged.finish().map_err(registry_error)?;
        }
        drop(data);
        let (success, output) = decoded.map_err(|e| match e {
            Ffmpeg::Missing => error(500, "ffmpeg not installed (brew install ffmpeg)"),
            Ffmpeg::Timeout => error(400, "audio decode timed out"),
            Ffmpeg::Failed => error(500, "Internal Server Error"),
        })?;
        if !success || output.is_empty() {
            return Err(error(400, "could not decode the audio"));
        }
        let audio: Vec<f32> = output
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        if audio.len() > ENROLL_MAX_SECONDS * 16_000 {
            return Err(error(
                400,
                format!(
                    "recording is longer than {ENROLL_MAX_SECONDS} seconds; use a shorter clip"
                ),
            ));
        }
        Ok(audio)
    }

    fn rename_voiceprint(
        &self,
        request: &BackendRequest,
        reference: &str,
    ) -> Reply<BackendResponse> {
        self.require_recognition(&request.config)?;
        let body = json_object(request)?;
        self.require_recognition(&request.config)?; // the gate may have closed meanwhile
        let name = clean_name(&body["name"]).ok_or_else(|| error(400, "invalid speaker name"))?;
        let data = self.data();
        // Lookup + write under one registry lock.
        let held = data.lock_registry();
        let registry = data.registry();
        let mut vp = registry.find(reference).map_err(registry_error)?;
        registry
            .rename(&held, &vp.id, &name)
            .map_err(registry_error)?;
        vp.name = name;
        Ok(ok(vp.public()))
    }

    fn forget(&self, config: &Config, reference: Option<&str>) -> Reply<BackendResponse> {
        self.require_recognition(config)?;
        let registry = self.data().registry();
        let id = match reference {
            Some(reference) => Some(registry.find(reference).map_err(registry_error)?.id),
            None => None,
        };
        let forgotten = registry.forget(id.as_deref()).map_err(registry_error)?;
        Ok(ok(json!({"forgotten": forgotten})))
    }

    /// `GET /api/speakers/export`: the only response holding vectors. Local only.
    pub(crate) fn export(&self, config: &Config, local: bool) -> Reply<Download> {
        self.require_recognition(config)?;
        if !local {
            return Err(error(403, "export is only available on this computer"));
        }
        let body = self
            .data()
            .registry()
            .export_zip()
            .map_err(registry_error)?;
        Ok(Download {
            content_type: "application/zip",
            headers: vec![
                ("cache-control", "no-store".into()),
                ("x-content-type-options", "nosniff".into()),
                (
                    "content-disposition",
                    "attachment; filename=\"localflow-speakers.zip\"".into(),
                ),
            ],
            body,
        })
    }
}

/// One speaker of a stopping meeting. `centroid` / `voiceprint_id` are only
/// ever set with recognition on and never leave this process except into the sidecar.
pub(crate) struct StopSpeaker {
    pub id: String,
    pub source: String,
    pub label: String,
    pub name: Option<String>,
    pub voiceprint_id: Option<String>,
    pub seconds: f64,
    pub centroid: Option<Vec<f32>>,
}
impl From<lf_meetings::LabelledSpeaker> for StopSpeaker {
    fn from(s: lf_meetings::LabelledSpeaker) -> Self {
        Self {
            label: format!("Speaker {}", s.id.trim_start_matches('S')),
            id: s.id,
            source: s.source,
            name: None,
            voiceprint_id: None,
            seconds: s.seconds,
            centroid: s.centroid,
        }
    }
}

impl MeetingsBackend {
    /// Name speakers whose centroid matches an enrolled voiceprint of this
    /// embedder (`collector._recognise`). Recognition turned off before or
    /// after a match leaves one coherent anonymous result without centroids.
    pub(crate) fn recognise(
        &self,
        config: &Config,
        speakers: &mut [StopSpeaker],
        segments: &mut [lf_meetings::Segment],
        embedder: Option<&str>,
    ) {
        let anonymous = |speakers: &mut [StopSpeaker]| {
            for sp in speakers.iter_mut() {
                sp.centroid = None;
            }
        };
        let Some(model_id) = embedder else {
            return anonymous(speakers);
        };
        let registry = self.data().registry();
        let mut names: Vec<(usize, String, String)> = Vec::new();
        for (at, sp) in speakers.iter().enumerate() {
            let Some(centroid) = &sp.centroid else {
                continue;
            };
            if !self.recognition_live(config) {
                return anonymous(speakers);
            }
            if let Some((vp, _score)) = registry.match_vector(centroid, model_id) {
                names.push((at, vp.name, vp.id));
            }
        }
        if !self.recognition_live(config) {
            return anonymous(speakers);
        }
        for (at, name, id) in names {
            let sp = &mut speakers[at];
            for segment in segments.iter_mut() {
                if segment.speaker_id.as_deref() == Some(sp.id.as_str()) {
                    segment.speaker_label = Some(name.clone());
                }
            }
            sp.label.clone_from(&name);
            sp.name = Some(name);
            sp.voiceprint_id = Some(id);
        }
        log::info!(
            "speaker recognition: {} of {} speakers named",
            speakers.iter().filter(|s| s.name.is_some()).count(),
            speakers.len()
        );
    }

    /// Persist the saved meeting's speaker record; never fails the stop.
    /// Voice data is stored only while recognition is still on and no forget
    /// ran since finalize (checked under the sidecar lock forget holds).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_sidecar(
        &self,
        config: &Config,
        saved: &lf_meetings::SavedNote,
        meeting_id: &str,
        finished: &crate::session::Finished,
        status: &str,
        speakers: Vec<StopSpeaker>,
        epoch: u64,
    ) {
        let data = self.data();
        let held = data.lock_sidecars();
        let private = self.recognition_live(config) && data.forget_epoch() == epoch;
        let entries: Vec<speakers::store::SpeakerEntry> = speakers
            .into_iter()
            .map(|sp| {
                let rendered = finished
                    .segments
                    .iter()
                    .rev()
                    .find(|s| s.speaker_id.as_deref() == Some(sp.id.as_str()))
                    .map(|s| s.speaker().into_owned());
                speakers::store::SpeakerEntry {
                    label: rendered.unwrap_or(sp.label),
                    voiceprint_id: sp.voiceprint_id.filter(|_| private),
                    centroid: sp.centroid.filter(|_| private).map(|c| {
                        c.iter()
                            .map(|&x| speakers::py_round(f64::from(x), 6))
                            .collect()
                    }),
                    id: sp.id,
                    source: sp.source,
                    name: sp.name,
                    // The stop payload's value (`round(seconds, 1)`), as Python stores it.
                    seconds: speakers::py_round(sp.seconds, 1),
                }
            })
            .collect();
        let record = speakers::store::build_record(
            meeting_id,
            &saved.log_path.to_string_lossy(),
            &saved.notes_path.to_string_lossy(),
            &finished.started_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            finished.diarization.embedder,
            status,
            &entries,
            &finished.segments,
        );
        if data.store().save(&held, &record).is_err() {
            log::warn!("writing meeting speaker data failed; notes still saved");
        }
    }
}

/// `(name field, audio bytes)` from the multipart body, parsed in memory:
/// one file and one other field (at most 1024 bytes), as Starlette's limits.
fn enroll_upload(request: &BackendRequest) -> Reply<(Option<String>, Vec<u8>)> {
    let invalid = || error(400, "invalid upload");
    let header = request
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let boundary = multer::parse_boundary(header).map_err(|_| invalid())?;
    futures_executor::block_on(async {
        let body = request.body.clone();
        let input =
            futures_util::stream::once(async move { Ok::<_, std::convert::Infallible>(body) });
        let mut multipart = multer::Multipart::new(input, boundary);
        let (mut files, mut fields) = (0, 0);
        let (mut file, mut name): (Option<Vec<u8>>, Option<Option<String>>) = (None, None);
        while let Some(field) = multipart.next_field().await.map_err(|_| invalid())? {
            let key = field.name().map(str::to_owned);
            if field.file_name().is_some() {
                files += 1;
                if files > 1 {
                    return Err(invalid());
                }
                let bytes = field.bytes().await.map_err(|_| invalid())?;
                match key.as_deref() {
                    Some("file") => file = Some(bytes.to_vec()),
                    Some("name") => name = Some(None), // an upload is not a name
                    _ => {}
                }
            } else {
                fields += 1;
                let bytes = field.bytes().await.map_err(|_| invalid())?;
                if fields > 1 || bytes.len() > 1024 {
                    return Err(invalid());
                }
                if key.as_deref() == Some("name") {
                    name = Some(String::from_utf8(bytes.to_vec()).ok());
                }
            }
        }
        let data = file.ok_or_else(|| error(400, "missing audio file"))?;
        if data.len() > MAX_UPLOAD_BYTES {
            return Err(error(413, "upload too large"));
        }
        Ok((name.flatten(), data))
    })
}

/// ISO BMFF (M4A/MP4): an `ftyp` box first, after at most a few padding boxes.
pub fn is_iso_bmff(data: &[u8]) -> bool {
    let mut offset = 0usize;
    for _ in 0..4 {
        let Some(head) = data.get(offset..offset + 8) else {
            return false;
        };
        let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let kind = &head[4..8];
        if kind == b"ftyp" {
            return (16..=data.len() - offset).contains(&size);
        }
        if !matches!(kind, b"free" | b"skip" | b"wide") || size < 8 {
            return false;
        }
        offset += size;
    }
    false
}

enum Ffmpeg {
    Missing,
    Timeout,
    Failed,
}

/// ffmpeg to 16 kHz mono f32le: `stdin` bytes, or one local ISO BMFF `source`
/// file (file protocol only, mov demuxer forced, external references off).
fn run_ffmpeg(
    stdin: Option<&[u8]>,
    source: Option<&Path>,
    seconds: usize,
) -> Result<(bool, Vec<u8>), Ffmpeg> {
    use wait_timeout::ChildExt;
    let mut command = Command::new("ffmpeg");
    command.arg("-nostdin");
    match source {
        None => command.args([
            "-protocol_whitelist",
            "pipe",
            "-format_whitelist",
            FFMPEG_INPUT_FORMATS,
            "-i",
            "pipe:0",
        ]),
        Some(path) => command
            .args([
                "-protocol_whitelist",
                "file",
                "-format_whitelist",
                FFMPEG_INPUT_FORMATS,
            ])
            .args(["-f", "mov", "-enable_drefs", "0", "-i"])
            .arg(format!("file:{}", path.display())),
    };
    command
        .args([
            "-t",
            &seconds.max(1).to_string(),
            "-f",
            "f32le",
            "-ac",
            "1",
            "-ar",
            "16000",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Ffmpeg::Missing
        } else {
            Ffmpeg::Failed
        }
    })?;
    let mut input = child.stdin.take().ok_or(Ffmpeg::Failed)?;
    let mut output = child.stdout.take().ok_or(Ffmpeg::Failed)?;
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let _ = input.write_all(stdin.unwrap_or_default());
        });
        let reader = scope.spawn(move || {
            let mut buffer = Vec::new();
            output.read_to_end(&mut buffer).map(|_| buffer)
        });
        let status = child.wait_timeout(FFMPEG_TIMEOUT);
        let status = match status {
            Ok(Some(status)) => status,
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(Ffmpeg::Timeout);
            }
        };
        let output = reader
            .join()
            .map_err(|_| Ffmpeg::Failed)?
            .map_err(|_| Ffmpeg::Failed)?;
        Ok((status.success(), output))
    })
}

#[cfg(test)]
mod tests {
    use super::is_iso_bmff;

    #[test]
    fn iso_bmff_detection_matches_python() {
        let ftyp = |size: u32| [&size.to_be_bytes()[..], b"ftypM4A \0\0\0\0"].concat();
        assert!(is_iso_bmff(&ftyp(16)));
        assert!(!is_iso_bmff(&ftyp(15)));
        assert!(!is_iso_bmff(&ftyp(17)));
        let padded = [&8u32.to_be_bytes()[..], b"free", &ftyp(16)].concat();
        assert!(is_iso_bmff(&padded));
        assert!(!is_iso_bmff(b"RIFF\0\0\0\0WAVEfmt "));
        assert!(!is_iso_bmff(
            &[&4u32.to_be_bytes()[..], b"free", &ftyp(16)].concat()
        ));
    }
}
