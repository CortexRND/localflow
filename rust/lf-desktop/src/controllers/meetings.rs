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
///   dirty until a write carrying that exact edit is confirmed.
/// * One acknowledgement boundary for every write of the composer text to the
///   server, autosave and meeting start alike: [`Self::begin_write`] captures
///   the token of the text being sent and [`Self::write_finished`] acknowledges
///   it. Only a successful write of the newest edit makes the composer clean;
///   edits typed while the write was in flight, and failed writes, stay dirty.
/// * Poll/write races: every edit and every finished write bumps an epoch. A
///   poll result is adopted only if the epoch is unchanged since the poll was
///   sent, so a status read that left before a write finished cannot roll the
///   composer back to older server text.
/// * Lifecycle races: Start and Stop are acknowledged with
///   [`Self::action_finished`], which also advances a lifecycle generation. A
///   whole status snapshot from a poll sent before the latest Start/Stop
///   finished is obsolete ([`Self::is_current`]) and must not be applied, so a
///   pre-start "idle" or pre-stop "recording" answer cannot change the phase.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotesSync {
    epoch: u64,
    dirty: bool,
    /// Epoch of the newest local edit.
    edit_epoch: u64,
    /// Advanced by every finished Start/Stop (and by `reset`).
    generation: u64,
}

/// What a status poll was sent against (see [`NotesSync::poll_started`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollTicket {
    epoch: u64,
    generation: u64,
}

/// The edit a write of the composer text covers (see [`NotesSync::begin_write`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteToken(u64);

/// What a window close must do about the meeting notes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseNotes {
    /// Nothing unsaved.
    Nothing,
    /// A meeting is recording and the composer has unsent edits: send them.
    Send,
    /// The notes can't be made safe right now; keep the window open with
    /// this reason (the user may still close anyway).
    Blocked(String),
}

/// Decide what closing means for the notes in `phase`.
///
/// Start and Stop are in flight while `Starting`/`Stopping`; their completion
/// callbacks cannot run during a synchronous close wait, so a close then is
/// refused with the reason instead of guessing (notes typed during Start are
/// flushed by a close retried after it finished). Unsent notes outside a
/// recording have nowhere to go and are reported, never dropped silently.
pub fn close_notes(phase: &Phase, dirty: bool, composer_empty: bool) -> CloseNotes {
    match phase {
        Phase::Starting => CloseNotes::Blocked(
            "A meeting is still starting. Wait until it's recording, then close again so your notes are saved.".into(),
        ),
        Phase::Stopping => CloseNotes::Blocked(
            "Your meeting is still being saved. Wait until it finishes, then close again.".into(),
        ),
        Phase::Recording { .. } if dirty => CloseNotes::Send,
        Phase::Recording { .. } => CloseNotes::Nothing,
        _ if dirty && !composer_empty => CloseNotes::Blocked(
            "Meeting notes you typed weren't saved because no meeting is recording.".into(),
        ),
        _ => CloseNotes::Nothing,
    }
}

/// What a status poll means for the composer during a meeting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconcile {
    /// Load the server's notes into the clean composer.
    Adopt(String),
    /// The composer has edits the server doesn't have: send them.
    Resend,
    /// Nothing to do.
    Keep,
}

impl NotesSync {
    /// Call when sending a status poll; check the result with
    /// [`Self::is_current`] and pass it to [`Self::reconcile`].
    pub fn poll_started(&self) -> PollTicket {
        PollTicket {
            epoch: self.epoch,
            generation: self.generation,
        }
    }

    /// False when a Start/Stop finished (or the meeting was reset) after the
    /// poll was sent: the whole snapshot is obsolete.
    pub fn is_current(&self, ticket: PollTicket) -> bool {
        ticket.generation == self.generation
    }

    /// The user changed the composer.
    pub fn edited(&mut self) {
        self.epoch += 1;
        self.dirty = true;
        self.edit_epoch = self.epoch;
    }

    /// Call in the same step that reads the composer text for a write (an
    /// autosave or the notes sent with meeting start): the token names exactly
    /// the edits that text contains.
    pub fn begin_write(&self) -> WriteToken {
        WriteToken(self.edit_epoch)
    }

    /// A write started with [`Self::begin_write`] finished. A success clears
    /// the dirty state only if no edit happened since the write began; a
    /// failure never does.
    pub fn write_finished(&mut self, token: WriteToken, ok: bool) {
        self.epoch += 1;
        if ok && token.0 == self.edit_epoch {
            self.dirty = false;
        }
    }

    /// A Start or Stop (which both write the composer text) finished:
    /// acknowledge its write like any other, and make every status poll sent
    /// before it obsolete.
    pub fn action_finished(&mut self, token: WriteToken, ok: bool) {
        self.write_finished(token, ok);
        self.generation += 1;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The text to load into the composer from a poll sent with `ticket`, or
    /// `None` to keep the composer as it is.
    pub fn adopt(&self, ticket: PollTicket, local: &str, server: Option<&str>) -> Option<String> {
        let server = server?;
        if self.dirty || ticket != self.poll_started() || local == server {
            return None;
        }
        Some(server.to_string())
    }

    /// Decide what a poll sent with `ticket` means for the composer: adopt
    /// newer server notes into a clean composer, resend local edits the server
    /// lacks, or leave both alone.
    pub fn reconcile(&self, ticket: PollTicket, local: &str, server: Option<&str>) -> Reconcile {
        if let Some(text) = self.adopt(ticket, local, server) {
            return Reconcile::Adopt(text);
        }
        if self.dirty && server.is_some_and(|s| s != local) {
            return Reconcile::Resend;
        }
        Reconcile::Keep
    }

    /// A new meeting (or no meeting): forget all state.
    pub fn reset(&mut self) {
        self.epoch += 1;
        self.generation += 1;
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
        sync.edited();
        let token = sync.begin_write();
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
        sync.write_finished(token, true);
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

        // Only a successful write of the newest edit clears the dirty flag.
        sync.edited();
        let first = sync.begin_write();
        sync.edited();
        let second = sync.begin_write();
        sync.write_finished(first, true);
        assert!(sync.is_dirty(), "older write must not clean a newer edit");
        sync.write_finished(second, false);
        assert!(sync.is_dirty(), "failed write keeps the edit dirty");
        sync.write_finished(second, true);
        assert!(!sync.is_dirty());
        sync.reset();
        assert!(!sync.is_dirty());
    }

    /// Draft typed before Start: Start writes it, so a successful start
    /// acknowledges it. Newer notes written by another frontend are then
    /// adopted and the stale draft is never resent.
    #[test]
    fn a_draft_saved_by_start_is_acknowledged_so_newer_server_notes_win() {
        let client = FakeMeetingClient::new();
        let mut sync = NotesSync::default();
        sync.edited(); // typed while idle
        let token = sync.begin_write(); // read with the notes for start
        let phase = start(&client, "", "general", "draft");
        assert!(matches!(phase, Phase::Recording { .. }));
        sync.action_finished(token, matches!(phase, Phase::Recording { .. }));
        assert!(!sync.is_dirty(), "start saved the draft");

        client.save_notes("newer, from another frontend").unwrap();
        let sent = sync.poll_started();
        let snap = poll(&client, false);
        assert_eq!(
            sync.reconcile(sent, "draft", snap.notes.as_deref()),
            Reconcile::Adopt("newer, from another frontend".into())
        );
        let saves = client
            .calls()
            .iter()
            .filter(|c| matches!(c, MeetingCall::SaveNotes(_)))
            .count();
        assert_eq!(
            saves, 1,
            "only the other frontend's write; no resend of the draft"
        );
    }

    /// Typing while Start is in flight: the start acknowledges only what it
    /// sent; the newer local edit stays dirty, is resent, and is never
    /// replaced by server text (neither its own older copy nor a third
    /// party's).
    #[test]
    fn an_edit_typed_during_start_stays_dirty_and_is_resent() {
        let client = FakeMeetingClient::new();
        let mut sync = NotesSync::default();
        sync.edited();
        let token = sync.begin_write();
        let phase = start(&client, "", "", "draft");
        sync.edited(); // "draft + more" typed before the start returned
        sync.action_finished(token, matches!(phase, Phase::Recording { .. }));
        assert!(sync.is_dirty());

        let sent = sync.poll_started();
        let snap = poll(&client, false);
        assert_eq!(snap.notes.as_deref(), Some("draft"));
        assert_eq!(
            sync.reconcile(sent, "draft + more", snap.notes.as_deref()),
            Reconcile::Resend
        );
        client.save_notes("someone else's notes").unwrap();
        let sent = sync.poll_started();
        let snap = poll(&client, false);
        assert_eq!(
            sync.reconcile(sent, "draft + more", snap.notes.as_deref()),
            Reconcile::Resend,
            "dirty local text is never clobbered"
        );
        // The resend is acknowledged through the same boundary.
        let resend = sync.begin_write();
        let writer = MeetingNotesWriter::new();
        writer
            .save(&client, writer.ticket(), "draft + more")
            .unwrap();
        sync.write_finished(resend, true);
        assert!(!sync.is_dirty());
        let sent = sync.poll_started();
        let snap = poll(&client, false);
        assert_eq!(
            sync.reconcile(sent, "draft + more", snap.notes.as_deref()),
            Reconcile::Keep
        );
    }

    /// A failed start saved nothing: the draft stays dirty and is not
    /// replaced when a meeting later turns up with other notes.
    #[test]
    fn a_failed_start_keeps_the_draft_dirty() {
        let client = FakeMeetingClient::new();
        let mut sync = NotesSync::default();
        sync.edited();
        let token = sync.begin_write();
        client.set_down(Some(SERVER_DOWN));
        let phase = start(&client, "", "", "draft");
        assert!(matches!(phase, Phase::Error(_)));
        sync.action_finished(token, matches!(phase, Phase::Recording { .. }));
        assert!(sync.is_dirty());

        client.set_down(None);
        client.start("", "", "", "started elsewhere", None).unwrap();
        let sent = sync.poll_started();
        let snap = poll(&client, false);
        assert_eq!(
            sync.reconcile(sent, "draft", snap.notes.as_deref()),
            Reconcile::Resend
        );
        // A poll that left before the failed start finished is never adopted.
        let mut clean = NotesSync::default();
        let stale = clean.poll_started();
        let token = clean.begin_write();
        clean.write_finished(token, false);
        assert_eq!(clean.reconcile(stale, "", Some("server")), Reconcile::Keep);
    }

    /// A poll sent before Start (answering "idle") that returns after Start
    /// finished is obsolete, so the phase can't flip back to Idle. Autosave
    /// acknowledgements don't invalidate snapshots.
    #[test]
    fn a_pre_start_poll_cannot_reset_the_phase() {
        let client = FakeMeetingClient::new();
        let mut sync = NotesSync::default();
        let before = sync.poll_started();
        let stale = poll(&client, false);
        assert_eq!(stale.phase, Phase::Idle);
        let token = sync.begin_write();
        let phase = start(&client, "", "", "");
        sync.action_finished(token, matches!(phase, Phase::Recording { .. }));
        assert!(!sync.is_current(before), "pre-start snapshot rejected");

        // A failed start also ends the lifecycle step.
        let mut failed = NotesSync::default();
        let ticket = failed.poll_started();
        let token = failed.begin_write();
        failed.action_finished(token, false);
        assert!(!failed.is_current(ticket));

        // Polls during the meeting stay current across autosaves.
        let during = sync.poll_started();
        sync.edited();
        let save = sync.begin_write();
        sync.write_finished(save, true);
        assert!(sync.is_current(during));
        assert_eq!(sync.reconcile(during, "x", Some("y")), Reconcile::Keep);
    }

    /// Stop uses the same acknowledgement pair: a poll that still saw the
    /// meeting recording before Stop finished can't change state afterwards,
    /// and Stop's notes write acknowledges the composer.
    #[test]
    fn a_pre_stop_poll_cannot_revive_the_recording() {
        let client = FakeMeetingClient::new();
        let mut sync = NotesSync::default();
        let token = sync.begin_write();
        let phase = start(&client, "", "", "");
        sync.action_finished(token, matches!(phase, Phase::Recording { .. }));
        sync.edited();
        let before = sync.poll_started();
        let stale = poll(&client, false);
        assert!(matches!(stale.phase, Phase::Recording { .. }));

        let token = sync.begin_write();
        let writer = MeetingNotesWriter::new();
        let phase = stop(&client, &writer, writer.ticket(), "final notes");
        let saved = matches!(phase, Phase::Saved { .. });
        assert!(saved);
        sync.action_finished(token, saved);
        assert!(!sync.is_current(before), "pre-stop snapshot rejected");
        assert!(!sync.is_dirty(), "stop wrote the composer text");

        // A failed stop keeps the notes dirty for the next attempt.
        let mut failing = NotesSync::default();
        failing.edited();
        let token = failing.begin_write();
        client.set_down(Some(SERVER_DOWN));
        let phase = stop(&client, &writer, writer.ticket(), "notes");
        assert!(matches!(phase, Phase::Error(_)));
        failing.action_finished(token, false);
        assert!(failing.is_dirty());
    }

    /// Closing while Start/Stop is in flight keeps the window open with the
    /// reason; notes typed during Start are flushed by a close retried after
    /// Start finished; unsent notes outside a recording are reported.
    #[test]
    fn close_during_busy_actions_and_newer_notes() {
        assert!(
            matches!(close_notes(&Phase::Starting, true, false), CloseNotes::Blocked(r) if r.contains("still starting"))
        );
        assert!(
            matches!(close_notes(&Phase::Stopping, false, true), CloseNotes::Blocked(r) if r.contains("still being saved"))
        );

        let client = std::sync::Arc::new(FakeMeetingClient::new());
        let mut sync = NotesSync::default();
        sync.edited();
        let token = sync.begin_write();
        let phase = start(client.as_ref(), "", "", "draft");
        sync.edited(); // "draft + typed during start"
        sync.action_finished(token, matches!(phase, Phase::Recording { .. }));
        assert_eq!(
            close_notes(&phase, sync.is_dirty(), false),
            CloseNotes::Send
        );
        let writer = std::sync::Arc::new(MeetingNotesWriter::new());
        let found = crate::controllers::shutdown::failures();
        let ticket = writer.ticket();
        crate::controllers::shutdown::send_meeting_notes(
            client.clone(),
            writer,
            ticket,
            "draft + typed during start".into(),
            found.clone(),
        );
        let check = crate::controllers::shutdown::finish(
            Vec::new(),
            &found,
            crate::controllers::shutdown::CLOSE_WAIT,
        );
        assert!(check.ok(), "{check:?}");
        assert!(client
            .calls()
            .contains(&MeetingCall::SaveNotes("draft + typed during start".into())));

        // Clean recording: nothing to send. Failed start with typed notes:
        // reported. Idle with an empty composer: nothing.
        assert_eq!(
            close_notes(
                &Phase::Recording {
                    seconds: 1,
                    capture: String::new()
                },
                false,
                false
            ),
            CloseNotes::Nothing
        );
        assert!(
            matches!(close_notes(&Phase::Error("down".into()), true, false), CloseNotes::Blocked(r) if r.contains("weren't saved"))
        );
        assert_eq!(close_notes(&Phase::Idle, true, true), CloseNotes::Nothing);
        assert_eq!(close_notes(&Phase::Idle, false, false), CloseNotes::Nothing);
    }

    #[test]
    fn elapsed_formats() {
        assert_eq!(elapsed(65), "1:05");
        assert_eq!(elapsed(3725), "1:02:05");
    }
}
