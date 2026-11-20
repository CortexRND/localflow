//! Portable meeting flow over `MeetingClient` (the AppKit page's state machine
//! lives in `lf_ui::mac`, which cannot build here). Blocking calls; the page
//! runs them on the background executor and applies results on the UI thread.

use lf_ui::backends::meeting::{
    is_default_title, MeetingClient, MeetingNotesWriter, MAX_CHAT_QUESTION_CHARS,
    MAX_USER_NOTES_CHARS,
};
use lf_ui::hub_model;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Status not loaded yet.
    Loading,
    /// The service did not answer.
    Offline(String),
    Idle,
    Starting,
    Recording {
        seconds: i64,
        capture: String,
    },
    Stopping,
    Saved {
        message: String,
        notes_path: String,
    },
    Error(String),
}

/// A template choice `(id, name)`.
pub type Template = (String, String);

/// Everything one status poll learns.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub phase: Phase,
    pub templates: Vec<Template>,
    pub default_template: String,
    pub chat_available: bool,
    pub detected: bool,
    /// `session.user_notes` of a running meeting (`None` when idle/offline).
    pub notes: Option<String>,
}

/// Keeps the notes composer and the server's `session.user_notes` in step.
///
/// * A running meeting's notes are **adopted** into the composer, so opening
///   the page during a meeting (or after a restart) shows them and Stop never
///   overwrites them with an empty composer.
/// * Local edits are **never clobbered**: once the user types, the composer is
///   dirty until a save of that exact edit is confirmed.
/// * Poll/save races: every edit and every finished save bumps an epoch. A
///   poll result is adopted only if the epoch is unchanged since the poll was
///   sent, so a status read that left before a save finished cannot roll the
///   composer back to older server text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotesSync {
    epoch: u64,
    dirty: bool,
    /// Epoch of the newest local edit.
    edit_epoch: u64,
}

impl NotesSync {
    /// Call when sending a status poll; pass the result to [`Self::adopt`].
    pub fn poll_started(&self) -> u64 {
        self.epoch
    }

    /// The user changed the composer. Returns the edit's token for the save.
    pub fn edited(&mut self) -> u64 {
        self.epoch += 1;
        self.dirty = true;
        self.edit_epoch = self.epoch;
        self.edit_epoch
    }

    /// A save of edit `token` finished. Only a successful save of the newest
    /// edit makes the composer clean; a failed or superseded one keeps it dirty.
    pub fn save_finished(&mut self, token: u64, ok: bool) {
        self.epoch += 1;
        if ok && token == self.edit_epoch {
            self.dirty = false;
        }
    }

    /// Token of the newest local edit (what a save sent now covers).
    pub fn edited_token(&self) -> u64 {
        self.edit_epoch
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The text to load into the composer from a poll sent at `poll_epoch`,
    /// or `None` to keep the composer as it is.
    pub fn adopt(&self, poll_epoch: u64, local: &str, server: Option<&str>) -> Option<String> {
        let server = server?;
        if self.dirty || poll_epoch != self.epoch || local == server {
            return None;
        }
        Some(server.to_string())
    }

    /// A new meeting (or no meeting): forget all state.
    pub fn reset(&mut self) {
        self.epoch += 1;
        self.dirty = false;
    }
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Read `/meeting/status` (+ templates when `with_templates`).
pub fn poll(client: &dyn MeetingClient, with_templates: bool) -> Snapshot {
    let status = match client.status() {
        Ok(status) => status,
        Err(err) => {
            return Snapshot {
                phase: Phase::Offline(err.0),
                templates: Vec::new(),
                default_template: String::new(),
                chat_available: false,
                detected: false,
                notes: None,
            }
        }
    };
    let (templates, default_template) = if with_templates {
        templates(client)
    } else {
        (Vec::new(), String::new())
    };
    let session = status.get("session").filter(|s| s.is_object());
    let notes = session.map(|s| text(s, "user_notes"));
    let phase = match session {
        Some(session) => Phase::Recording {
            seconds: session.get("seconds").and_then(Value::as_i64).unwrap_or(0),
            capture: hub_model::meeting_capture_status(session),
        },
        None => Phase::Idle,
    };
    Snapshot {
        phase,
        templates,
        default_template,
        chat_available: status
            .get("chat_available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        detected: status
            .get("detected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        notes,
    }
}

pub fn templates(client: &dyn MeetingClient) -> (Vec<Template>, String) {
    match client.templates() {
        Ok(value) => {
            let list = value
                .get("templates")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|t| {
                            let id = text(t, "id");
                            let name = Some(text(t, "name"))
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| id.clone());
                            (id, name)
                        })
                        .filter(|(id, _)| !id.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            (list, text(&value, "default"))
        }
        Err(_) => (Vec::new(), String::new()),
    }
}

/// Validate the form before starting.
pub fn validate_notes(notes: &str) -> Result<(), String> {
    if notes.chars().count() > MAX_USER_NOTES_CHARS {
        return Err(format!(
            "Notes are limited to {MAX_USER_NOTES_CHARS} characters."
        ));
    }
    Ok(())
}

/// `POST /meeting/start`. A blank or default title is named when saved.
pub fn start(client: &dyn MeetingClient, title: &str, template: &str, notes: &str) -> Phase {
    if let Err(message) = validate_notes(notes) {
        return Phase::Error(message);
    }
    let auto = title.trim().is_empty() || is_default_title(title);
    match client.start(title.trim(), "", template, notes, Some(auto)) {
        Ok(_) => Phase::Recording {
            seconds: 0,
            capture: String::new(),
        },
        Err(err) => Phase::Error(err.0),
    }
}

/// Flush the newest notes first, then `POST /meeting/stop`.
pub fn stop(
    client: &dyn MeetingClient,
    writer: &MeetingNotesWriter,
    ticket: u64,
    notes: &str,
) -> Phase {
    if let Err(err) = writer.save(client, ticket, notes) {
        return Phase::Error(format!(
            "Couldn't save your notes before stopping: {}",
            err.0
        ));
    }
    match client.stop() {
        Ok(saved) => Phase::Saved {
            message: hub_model::saved_notes_message(&saved),
            notes_path: text(&saved, "notes_path"),
        },
        Err(err) => Phase::Error(err.0),
    }
}

/// `POST /meeting/chat` with the same limits as the server.
pub fn chat(client: &dyn MeetingClient, question: &str) -> Result<String, String> {
    let q = question.trim();
    if q.is_empty() {
        return Err("Type a question first.".into());
    }
    if q.chars().count() > MAX_CHAT_QUESTION_CHARS {
        return Err(format!(
            "Questions are limited to {MAX_CHAT_QUESTION_CHARS} characters."
        ));
    }
    client
        .chat(q)
        .map(|answer| text(&answer, "answer"))
        .map_err(|err| err.0)
}

/// `"12:05"` for an elapsed time.
pub fn elapsed(seconds: i64) -> String {
    let s = seconds.max(0);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_ui::backends::meeting::{FakeMeetingClient, MeetingCall, SERVER_DOWN};

    #[test]
    fn start_record_stop_flushes_notes_then_saves() {
        let client = FakeMeetingClient::new();
        let snap = poll(&client, true);
        assert_eq!(snap.phase, Phase::Idle);
        assert_eq!(snap.templates[0].0, "general");
        assert!(matches!(
            start(&client, "", "general", "first"),
            Phase::Recording { .. }
        ));
        assert!(matches!(
            poll(&client, false).phase,
            Phase::Recording { .. }
        ));
        let writer = MeetingNotesWriter::new();
        let old = writer.ticket();
        let newest = writer.ticket();
        let phase = stop(&client, &writer, newest, "final notes");
        assert!(matches!(phase, Phase::Saved { .. }), "{phase:?}");
        // A superseded save is dropped and never overwrites the flush.
        assert!(writer
            .save(&client, old, "stale")
            .unwrap()
            .as_object()
            .unwrap()
            .is_empty());
        let calls = client.calls();
        let save_at = calls
            .iter()
            .position(|c| *c == MeetingCall::SaveNotes("final notes".into()))
            .unwrap();
        let stop_at = calls.iter().position(|c| *c == MeetingCall::Stop).unwrap();
        assert!(save_at < stop_at);
        assert!(chat(&client, "what was decided?").is_ok());
        assert!(chat(&client, "  ").is_err());
    }

    #[test]
    fn offline_and_errors_are_reported_not_hidden() {
        let client = FakeMeetingClient::new();
        client.set_down(Some(SERVER_DOWN));
        assert_eq!(
            poll(&client, true).phase,
            Phase::Offline(SERVER_DOWN.into())
        );
        assert_eq!(
            start(&client, "x", "", ""),
            Phase::Error(SERVER_DOWN.into())
        );
        client.set_down(None);
        assert!(matches!(start(&client, "x", "nope", ""), Phase::Error(_)));
        let long = "a".repeat(MAX_USER_NOTES_CHARS + 1);
        assert!(matches!(start(&client, "x", "", &long), Phase::Error(_)));
        assert!(chat(&client, "q").is_err(), "chat needs a saved meeting");
    }

    /// A meeting already running on the server (started elsewhere, or before
    /// this window opened) with notes: the page adopts them, so Stop and save
    /// sends those notes, not an empty composer.
    #[test]
    fn running_meeting_notes_are_adopted_and_survive_stop() {
        let client = FakeMeetingClient::new();
        client
            .start("Standup", "", "general", "- ship the preview", Some(false))
            .unwrap();
        let sync = NotesSync::default();
        let sent = sync.poll_started();
        let snap = poll(&client, false);
        assert!(matches!(snap.phase, Phase::Recording { .. }));
        assert_eq!(snap.notes.as_deref(), Some("- ship the preview"));
        let composer = sync
            .adopt(sent, "", snap.notes.as_deref())
            .expect("empty composer adopts the running notes");
        let writer = MeetingNotesWriter::new();
        let phase = stop(&client, &writer, writer.ticket(), &composer);
        assert!(matches!(phase, Phase::Saved { .. }));
        assert!(client
            .calls()
            .contains(&MeetingCall::SaveNotes("- ship the preview".into())));
        assert!(!client
            .calls()
            .contains(&MeetingCall::SaveNotes(String::new())));
        // Idle server: nothing to adopt.
        assert_eq!(poll(&client, false).notes, None);
        assert_eq!(sync.adopt(sync.poll_started(), "x", None), None);
    }

    #[test]
    fn local_edits_and_racing_polls_never_clobber_notes() {
        let client = FakeMeetingClient::new();
        client.start("", "", "", "server v1", None).unwrap();
        let mut sync = NotesSync::default();

        // Dirty local text wins over whatever the server reports.
        let token = sync.edited();
        let sent = sync.poll_started();
        let server = poll(&client, false).notes;
        assert_eq!(sync.adopt(sent, "local draft", server.as_deref()), None);

        // A poll sent before the save finished returns pre-save notes:
        // the epoch moved, so it is ignored even though the edit is now clean.
        let stale_poll = sync.poll_started();
        let stale = poll(&client, false).notes; // still "server v1"
        let writer = MeetingNotesWriter::new();
        writer
            .save(&client, writer.ticket(), "local draft")
            .unwrap();
        sync.save_finished(token, true);
        assert!(!sync.is_dirty());
        assert_eq!(
            sync.adopt(stale_poll, "local draft", stale.as_deref()),
            None
        );

        // A fresh poll after the save agrees with the composer: no change.
        let fresh = sync.poll_started();
        let now = poll(&client, false).notes;
        assert_eq!(now.as_deref(), Some("local draft"));
        assert_eq!(sync.adopt(fresh, "local draft", now.as_deref()), None);

        // Server notes changed elsewhere while the composer is clean: adopted.
        client.save_notes("edited on another device").unwrap();
        let later = sync.poll_started();
        let other = poll(&client, false).notes;
        assert_eq!(
            sync.adopt(later, "local draft", other.as_deref())
                .as_deref(),
            Some("edited on another device")
        );

        // Only a successful save of the newest edit clears the dirty flag.
        let first = sync.edited();
        let second = sync.edited();
        sync.save_finished(first, true);
        assert!(sync.is_dirty(), "older save must not clean a newer edit");
        sync.save_finished(second, false);
        assert!(sync.is_dirty(), "failed save keeps the edit dirty");
        sync.save_finished(second, true);
        assert!(!sync.is_dirty());
        sync.reset();
        assert!(!sync.is_dirty());
    }

    #[test]
    fn elapsed_formats() {
        assert_eq!(elapsed(65), "1:05");
        assert_eq!(elapsed(3725), "1:02:05");
    }
}
