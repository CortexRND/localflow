//! Close and quit durability: flush dirty page state, wait for real writes,
//! and report what could not be saved instead of dropping it.
//!
//! Built on the existing portable helpers: `HotPhrasesEditor::leave` (commit a
//! valid dirty snippet, keep an invalid one with a message) and
//! `lf_ui::shutdown::{register_pending_write, wait_for_pending_writes}` (a
//! process-wide counter of in-flight writes). Meeting-note writes run on their
//! own OS thread and hold a pending-write guard, so the UI thread can wait for
//! them without depending on the UI executor (no deadlock).

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lf_ui::backends::meeting::{MeetingClient, MeetingNotesWriter};
use lf_ui::hot_phrases_editor::{HotPhrasesEditor, PhrasesBackend};

/// How long a close or quit waits for in-flight writes.
pub const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Failures collected from writer threads.
pub type Failures = Arc<Mutex<Vec<String>>>;

pub fn failures() -> Failures {
    Arc::new(Mutex::new(Vec::new()))
}

/// Commit the snippet being edited. `Err` carries the reason it was kept.
pub fn flush_snippets<M: PhrasesBackend>(editor: &mut HotPhrasesEditor<M>) -> Result<(), String> {
    if editor.leave() {
        Ok(())
    } else {
        Err(format!("A snippet wasn't saved: {}", editor.error))
    }
}

/// Send meeting notes on a dedicated thread, registered as a pending write.
pub fn send_meeting_notes(
    client: Arc<dyn MeetingClient>,
    writer: Arc<MeetingNotesWriter>,
    ticket: u64,
    text: String,
    failures: Failures,
) -> JoinHandle<()> {
    let guard = lf_ui::shutdown::register_pending_write();
    std::thread::spawn(move || {
        let _guard = guard;
        if let Err(err) = writer.save(client.as_ref(), ticket, &text) {
            failures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!(
                    "Meeting notes weren't sent to LocalFlow: {}",
                    err.0
                ));
        }
    })
}

/// The result of preparing to close.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloseCheck {
    pub failures: Vec<String>,
    /// Writes were still running when the wait ended.
    pub timed_out: bool,
}

impl CloseCheck {
    pub fn ok(&self) -> bool {
        self.failures.is_empty() && !self.timed_out
    }

    pub fn message(&self) -> String {
        let mut lines = self.failures.clone();
        if self.timed_out {
            lines.push("Some changes are still being saved.".into());
        }
        lines.join(" ")
    }
}

/// Wait (bounded) for every registered write, then collect the failures.
pub fn finish(
    mut failures_so_far: Vec<String>,
    thread_failures: &Failures,
    wait: Duration,
) -> CloseCheck {
    let done = lf_ui::shutdown::wait_for_pending_writes(wait);
    failures_so_far.extend(
        thread_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..),
    );
    CloseCheck {
        failures: failures_so_far,
        timed_out: !done,
    }
}

/// What still keeps the window from closing, checked fresh right before it
/// does (never from an earlier snapshot): running model jobs, an in-flight
/// model deletion, and persistent writes (`writes_idle` comes from
/// `lf_ui::shutdown::wait_for_pending_writes`).
pub fn remaining_work(
    downloads: &crate::controllers::jobs::Downloads,
    writes_idle: bool,
) -> Vec<&'static str> {
    let mut out = Vec::new();
    if downloads.any_running() {
        out.push("A model download is still running.");
    }
    if downloads.deleting() {
        out.push("A speech model is still being deleted.");
    }
    if !writes_idle {
        out.push("A settings or key change is still being saved.");
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn closing_rechecks_work_that_started_after_the_drain() {
        use super::remaining_work;
        use crate::controllers::jobs::{speech_key, Downloads};
        let downloads = Downloads::new();
        assert!(remaining_work(&downloads, true).is_empty());
        // Work that appears after the one-time drain is still reported.
        let (go, gate) = std::sync::mpsc::channel::<()>();
        downloads.start(
            &speech_key("fw-base"),
            Box::new(move |_, _| {
                let _ = gate.recv();
                Ok(())
            }),
        );
        assert_eq!(
            remaining_work(&downloads, true),
            vec!["A model download is still running."]
        );
        go.send(()).unwrap();
        assert!(downloads.wait_idle(std::time::Duration::from_secs(3)));
        let guard = downloads.begin_delete().unwrap();
        assert_eq!(
            remaining_work(&downloads, false).len(),
            2,
            "deletion + pending write"
        );
        drop(guard);
        assert!(remaining_work(&downloads, true).is_empty());
    }

    use super::*;
    use lf_core::hotphrases::HotPhraseStore;
    use lf_core::phrases_model::{PhrasesModel, PhrasesSettings};
    use lf_ui::backends::meeting::{FakeMeetingClient, MeetingCall, SERVER_DOWN};
    use std::time::Instant;

    fn editor(dir: &std::path::Path) -> HotPhrasesEditor<PhrasesModel> {
        let store = HotPhraseStore::new(dir.join("hot_phrases.json"));
        let settings = PhrasesSettings {
            hot_phrases: true,
            ..PhrasesSettings::default()
        };
        HotPhrasesEditor::new(PhrasesModel::new(store, Some(settings)).unwrap())
    }

    #[test]
    fn a_snippet_typed_just_before_close_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut ed = editor(dir.path());
        assert!(ed.new_phrase());
        ed.edit(Some("my signature"), Some("Best,\nSam"), None);
        assert!(ed.pending(), "still inside the autosave debounce");
        flush_snippets(&mut ed).unwrap();
        // A fresh store reads it from disk.
        let entries = HotPhraseStore::new(dir.path().join("hot_phrases.json")).list();
        let text = serde_json::to_string(&entries).unwrap();
        assert!(
            text.contains("my signature") && text.contains("Best,\\nSam"),
            "{text}"
        );
    }

    #[test]
    fn an_invalid_snippet_is_reported_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut ed = editor(dir.path());
        assert!(ed.new_phrase());
        ed.edit(Some(""), Some("text without a trigger"), None);
        let err = flush_snippets(&mut ed).unwrap_err();
        assert!(err.starts_with("A snippet wasn't saved"), "{err}");
        assert_eq!(
            ed.text, "text without a trigger",
            "the edit stays in the editor"
        );
    }

    #[test]
    fn pending_meeting_notes_are_sent_and_awaited() {
        let client = Arc::new(FakeMeetingClient::new());
        client.start("", "", "", "", None).unwrap();
        let writer = Arc::new(MeetingNotesWriter::new());
        let found = failures();
        let ticket = writer.ticket();
        send_meeting_notes(
            client.clone(),
            writer,
            ticket,
            "typed at close".into(),
            found.clone(),
        );
        let check = finish(Vec::new(), &found, CLOSE_WAIT);
        assert!(check.ok(), "{check:?}");
        assert!(client
            .calls()
            .contains(&MeetingCall::SaveNotes("typed at close".into())));
    }

    #[test]
    fn failed_meeting_notes_are_reported() {
        let client = Arc::new(FakeMeetingClient::new());
        client.set_down(Some(SERVER_DOWN));
        let writer = Arc::new(MeetingNotesWriter::new());
        let found = failures();
        let ticket = writer.ticket();
        send_meeting_notes(client, writer, ticket, "notes".into(), found.clone());
        let check = finish(vec!["earlier failure".into()], &found, CLOSE_WAIT);
        assert!(!check.ok());
        assert_eq!(check.failures.len(), 2);
        assert!(check.message().contains("Meeting notes weren't sent"));
    }

    #[test]
    fn close_waits_for_an_in_flight_write() {
        let guard = lf_ui::shutdown::register_pending_write();
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(guard);
        });
        let check = finish(Vec::new(), &failures(), CLOSE_WAIT);
        assert!(check.ok());
        assert!(started.elapsed() >= Duration::from_millis(140));
        worker.join().unwrap();
        // A write that never finishes is reported as still saving.
        let stuck = lf_ui::shutdown::register_pending_write();
        let check = finish(Vec::new(), &failures(), Duration::from_millis(20));
        assert!(check.timed_out && !check.ok());
        assert!(check.message().contains("still being saved"));
        drop(stuck);
    }
}
