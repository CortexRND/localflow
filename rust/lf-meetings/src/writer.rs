use crate::{
    has_speakers, is_diarized, strip_remote_media,
    titles::{heading_text, safe_title, MAX_NAME_ATTEMPTS},
    trim, write_text, Segment,
};
use chrono::NaiveDateTime;
use lf_core::symbols::pycompat as py;
use std::{
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct SavedNote {
    pub notes_path: PathBuf,
    pub log_path: PathBuf,
    pub title: String,
}
pub fn reserve(notes_dir: &Path, logs_dir: &Path, base: &str) -> io::Result<(PathBuf, PathBuf)> {
    for attempt in 1..=MAX_NAME_ATTEMPTS {
        let name = if attempt == 1 {
            base.into()
        } else {
            format!("{base} {attempt}")
        };
        let notes = notes_dir.join(format!("{name}.md"));
        let log = logs_dir.join(format!("{name} (transcript).md"));
        match OpenOptions::new().write(true).create_new(true).open(&notes) {
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        match OpenOptions::new().write(true).create_new(true).open(&log) {
            Ok(_) => return Ok((notes, log)),
            Err(e) => {
                let _ = fs::remove_file(&notes);
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(e);
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free file name for {}", py::repr_str(base)),
    ))
}
/// One display label per distinct speaker identity for the `speakers:`
/// frontmatter, in first-appearance order (labels may repeat), or empty below
/// two identities. Only diarized meetings qualify; a mic segment without an id
/// is the local user, and un-attributed call audio is not a speaker.
fn diarized_speakers(segments: &[Segment]) -> Vec<String> {
    if !is_diarized(segments) {
        return Vec::new();
    }
    // `None` is the local user (mic without an id).
    let mut labels: Vec<(Option<&str>, String)> = Vec::new();
    for s in segments {
        let identity = match (&s.speaker_id, s.source == crate::SOURCE_ME) {
            (Some(id), _) => Some(id.as_str()),
            (None, true) => None,
            (None, false) => continue,
        };
        if !labels.iter().any(|(seen, _)| *seen == identity) {
            labels.push((identity, s.speaker().into_owned()));
        }
    }
    if labels.len() < 2 {
        return Vec::new();
    }
    labels.into_iter().map(|(_, label)| label).collect()
}
pub struct ObsidianWriter {
    pub vault: PathBuf,
    pub notes_folder: PathBuf,
    pub logs_folder: PathBuf,
}
pub struct WriteNote<'a> {
    pub title: &'a str,
    pub category: &'a str,
    pub started_at: NaiveDateTime,
    pub duration_seconds: f64,
    pub notes_md: &'a str,
    pub segments: &'a [Segment],
    pub user_notes: &'a str,
    pub template: &'a str,
    pub generated_title: bool,
}
impl ObsidianWriter {
    pub fn new(vault: &Path, notes_folder: &Path, logs_folder: &Path) -> Self {
        Self {
            vault: crate::expand(vault),
            notes_folder: notes_folder.into(),
            logs_folder: logs_folder.into(),
        }
    }
    pub fn write(&self, n: &WriteNote<'_>) -> io::Result<SavedNote> {
        let heading = if n.generated_title {
            heading_text(n.title)
        } else {
            safe_title(n.title)
        };
        let title = safe_title(&heading);
        let category = safe_title(n.category);
        let stamp = n.started_at.format("%Y-%m-%d %H%M");
        let notes_dir = self.vault.join(&self.notes_folder).join(&category);
        let logs_dir = self.vault.join(&self.logs_folder).join(&category);
        fs::create_dir_all(&notes_dir)?;
        fs::create_dir_all(&logs_dir)?;
        let (notes_path, log_path) = reserve(&notes_dir, &logs_dir, &format!("{stamp} {title}"))?;
        let minutes = (n.duration_seconds / 60.0).floor() as i64;
        let speakers = diarized_speakers(n.segments);
        // Quoted so an all-digit hex stays a YAML string; labels are already
        // cleaned, and raw UTF-8 avoids surrogate-pair escapes.
        let speaker_meta = if speakers.is_empty() {
            String::new()
        } else {
            let speakers = speakers.into_iter().map(serde_json::Value::String);
            format!(
                "meeting_id: \"{}\"\nspeakers: {}\n",
                lf_core::compact_uuid_v4(),
                py::json_dumps(&serde_json::Value::Array(speakers.collect()), None, false)
            )
        };
        let frontmatter=format!("---\ntitle: {}\ndate: {}\nplatform: {category}\nduration_minutes: {minutes}\n{}{speaker_meta}tags: [meeting, localflow]\n---\n\n",py::json_dumps(&serde_json::Value::String(heading.clone()),None,true),n.started_at.format("%Y-%m-%dT%H:%M"),if n.template.is_empty(){String::new()}else{format!("template: {}\n",safe_title(n.template))});
        let result = (|| {
            write_text(
                &notes_path,
                &format!(
                    "{frontmatter}# {heading}\n\n{}\n\nRaw transcript: [[{}]]\n",
                    trim(&strip_remote_media(n.notes_md)),
                    log_path.file_stem().unwrap().to_string_lossy()
                ),
            )?;
            let speakers = has_speakers(n.segments) || is_diarized(n.segments);
            let body = n
                .segments
                .iter()
                .map(|s| {
                    if speakers {
                        format!("**{}** **{}:** {}", s.stamp(), s.speaker(), s.text)
                    } else {
                        format!("**{}** {}", s.stamp(), s.text)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            let raw = if trim(n.user_notes).is_empty() {
                String::new()
            } else {
                format!("## My raw notes\n\n{}\n\n", trim(n.user_notes))
            };
            write_text(
                &log_path,
                &format!(
                    "{frontmatter}# {heading} — transcript\n\n{}\n\n{raw}Notes: [[{}]]\n",
                    if body.is_empty() {
                        "(no speech captured)"
                    } else {
                        &body
                    },
                    notes_path.file_stem().unwrap().to_string_lossy()
                ),
            )
        })();
        if let Err(e) = result {
            for path in [&notes_path, &log_path] {
                if path.metadata().is_ok_and(|m| m.len() == 0) {
                    let _ = fs::remove_file(path);
                }
            }
            return Err(e);
        }
        Ok(SavedNote {
            notes_path,
            log_path,
            title: heading,
        })
    }
}
