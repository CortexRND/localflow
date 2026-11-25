//! Speaker labels of saved meetings (post-stop and Recent panels): the
//! AppKit Hub's state machine (`lf_ui::mac::pages::meetings`) ported onto the
//! shared `hub_model` helpers. Pure and GPUI-free; the blocking calls at the
//! end run on the background executor.
//!
//! Ordering rules:
//! * Recent notes are read only when their frontmatter has a `meeting_id`,
//!   so notes saved without speaker labels never cause a request. A 404 is
//!   cached ("no speaker data"); a failure is not, and is read again once the
//!   server answers after being unreachable.
//! * One probe runs at a time; ids asked for meanwhile wait in a queue.
//! * Every edit reply bumps a sequence number: a probe or status poll sent
//!   before it may predate the edit, so its speakers are not applied (a probe
//!   is re-queued instead).
//! * Results are keyed by meeting id, and edit replies must match the
//!   editor's token and meeting id, so nothing attaches to another meeting.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use lf_ui::backends::meeting::MeetingClient;
use lf_ui::hub_model::{
    self, MeetingSpeaker, MergePicker, MergeTarget, RecentNote, SpeakerEditor, SpeakerMode,
};
use serde_json::{json, Value};

/// Shown when a rename is saved without a name (nothing is sent).
pub const NAME_NEEDED: &str = "Type a name first.";
/// Under the merge picker: the servers keep no undo for a merge.
pub const MERGE_FINAL: &str = "Merging can't be undone.";
/// The servers' speaker-name cap (`lf_meetings::MAX_SPEAKER_NAME_CHARS`,
/// Python `meetings.MAX_SPEAKER_NAME_CHARS`, `hub_model.SPEAKER_NAME_MAX`):
/// the rename field holds no more, so a longer name is never cut silently.
/// Restated because `lf-meetings` is only linked with the `runtime` feature.
/// The count is of raw characters; the server counts after NFKC, so a rare
/// expanding compatibility character (`㎒` -> `MHz`) can still be cut there.
pub const MAX_SPEAKER_NAME_CHARS: usize = 40;

/// One speaker edit as sent to the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Rename { speaker_id: String, name: String },
    Merge { from: String, into: String },
}

/// An edit the page sends (see [`SpeakerStore::commit`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditRequest {
    pub token: u64,
    pub meeting_id: String,
    pub change: Change,
}

/// What an edit request returned: the write's reply and, after a refusal,
/// one fresh read of that meeting (the data may have changed underneath).
#[derive(Clone, Debug, PartialEq)]
pub struct EditReply {
    pub result: Result<Value, String>,
    pub refetch: Option<Result<Option<Value>, String>>,
}

/// One running read of several meetings' speakers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub ids: Vec<String>,
    seq: u64,
}

/// `(meeting id, GET /api/meetings/{id}/speakers)`; `Ok(None)` is the 404.
pub type ProbeResults = Vec<(String, Result<Option<Value>, String>)>;

/// The poll ticket of [`SpeakerStore::poll_started`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedTicket(u64);

#[derive(Clone, Debug, Default)]
pub struct SpeakerStore {
    /// The last saved meeting (stop payload or `last_saved` of a status poll).
    saved: Option<Value>,
    /// meeting id -> speakers; `None`: the server has no speaker data (404).
    cache: BTreeMap<String, Option<Vec<MeetingSpeaker>>>,
    /// Recent note path -> its frontmatter `meeting_id`.
    recent_ids: BTreeMap<String, String>,
    /// The Recent meeting whose panel is open in the detail pane.
    open_recent: Option<String>,
    pub editor: SpeakerEditor,
    /// The speaker id picked as the merge target (empty: none).
    pub merge_choice: String,
    in_flight: bool,
    queue: BTreeSet<String>,
    after_reconnect: bool,
    seq: u64,
}

/// What identifies a saved meeting record: its `meeting_id`, else (an
/// unlabelled save, e.g. a recovery checkpoint) its notes or log path.
/// `None` when the record names none of them.
fn record_key(saved: &Value) -> Option<(&'static str, &str)> {
    ["meeting_id", "notes_path", "log_path"]
        .into_iter()
        .find_map(|field| {
            let value = saved.get(field)?.as_str()?.trim();
            (!value.is_empty()).then_some((field, value))
        })
}

/// Whether two saved records are the same meeting. Records without any
/// key are only the same when identical, so two unrelated id-less saves
/// are never confused.
fn same_record(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => match (record_key(a), record_key(b)) {
            (Some(ka), Some(kb)) => ka == kb,
            (None, None) => a == b,
            _ => false,
        },
        _ => false,
    }
}

impl SpeakerStore {
    // ------------------------------------------------------- saved panel ---

    /// The meeting shown by the post-stop panel: none while recording.
    pub fn saved_id(&self, recording: bool) -> Option<String> {
        if recording {
            return None;
        }
        self.saved.as_ref().and_then(hub_model::saved_meeting_id)
    }

    /// The newest saved meeting record (a Stop payload or a polled
    /// `last_saved`). Never changes which panel is open.
    pub fn set_saved(&mut self, saved: Option<Value>) {
        if let Some(id) = saved.as_ref().and_then(hub_model::saved_meeting_id) {
            let speakers = saved.as_ref().map(hub_model::meeting_speakers);
            self.cache.insert(id, speakers);
        }
        self.saved = saved;
        self.drop_stale_merge_choice();
    }

    /// This window's own Stop finished. A meeting saved with speakers is
    /// presented right away, closing a Recent panel left open from before;
    /// an edit still running there keeps its token and finishes normally.
    /// Status polls use [`Self::adopt_polled`] and never close a panel.
    pub fn present_saved(&mut self, saved: Option<Value>) {
        self.set_saved(saved);
        let Some(id) = self.saved_id(false) else {
            return;
        };
        if self.speakers_of(&id).is_empty() {
            return;
        }
        if let Some(open) = self.open_recent.take() {
            if self.editor.meeting_id == open && !self.editor.busy {
                self.editor.reset();
                self.merge_choice.clear();
            }
        }
    }

    /// A merge target that is no longer another speaker of the meeting is
    /// forgotten, so it can never come back as a pick by itself.
    fn drop_stale_merge_choice(&mut self) {
        if let SpeakerMode::Merge(from) = &self.editor.mode {
            if !self.merge_choice.is_empty() && !self.editor.busy {
                let speakers = self.speakers_of(&self.editor.meeting_id);
                if hub_model::resolve_merge_target(&speakers, from, &self.merge_choice).is_none() {
                    self.merge_choice.clear();
                }
            }
        }
    }

    /// Call when sending a status poll; pass the ticket to [`Self::adopt_polled`].
    pub fn poll_started(&self) -> SavedTicket {
        SavedTicket(self.seq)
    }

    /// `last_saved` from a status poll. A poll sent before the latest edit
    /// reply may hold the pre-edit speakers of the same meeting: kept local.
    ///
    /// `pinned`: this window still shows its own "Notes saved" banner, so
    /// the record from its Stop stays put; only fresher data of that exact
    /// record is taken. Unpinned, a different record (or none) is adopted.
    pub fn adopt_polled(&mut self, ticket: SavedTicket, saved: Option<Value>, pinned: bool) {
        let same = same_record(self.saved.as_ref(), saved.as_ref());
        if pinned && !same {
            return;
        }
        if ticket.0 != self.seq && same {
            return;
        }
        self.set_saved(saved);
    }

    // ------------------------------------------------------ recent notes ---

    /// The Recent list was (re)loaded: note which notes have a meeting id
    /// and queue every one not answered yet.
    pub fn set_recent(&mut self, notes: &[RecentNote]) {
        self.recent_ids = notes
            .iter()
            .filter_map(|note| {
                let id = hub_model::note_meeting_id(Path::new(&note.path))?;
                Some((note.path.clone(), id))
            })
            .collect();
        self.queue_uncached();
    }

    fn queue_uncached(&mut self) {
        let ids: Vec<String> = self
            .recent_ids
            .values()
            .filter(|id| !self.cache.contains_key(*id))
            .cloned()
            .collect();
        self.queue.extend(ids);
    }

    /// The meeting id behind a Recent row's "Speakers…" action: only when
    /// that meeting is known to have speakers, and not while the same
    /// meeting already shows its post-stop panel.
    pub fn recent_action(&self, path: &str, recording: bool) -> Option<String> {
        let id = self.recent_ids.get(path)?;
        let has = matches!(self.cache.get(id), Some(Some(list)) if !list.is_empty());
        (has && self.saved_id(recording).as_ref() != Some(id)).then(|| id.clone())
    }

    /// Open (or close) a Recent meeting's panel. Opening always reads the
    /// speakers again. Ignored while an edit is running.
    pub fn toggle_recent(&mut self, meeting_id: &str) -> bool {
        if self.editor.busy {
            return false;
        }
        if self.open_recent.as_deref() == Some(meeting_id) {
            self.open_recent = None;
            if self.editor.meeting_id == meeting_id {
                self.editor.reset();
            }
        } else {
            self.open_recent = Some(meeting_id.to_string());
            self.editor.reset();
            self.editor.open(meeting_id);
            self.queue.insert(meeting_id.to_string());
        }
        self.merge_choice.clear();
        true
    }

    /// The frontmatter meeting id of a Recent note, if it has one.
    pub fn recent_meeting(&self, path: &str) -> Option<&str> {
        self.recent_ids.get(path).map(String::as_str)
    }

    pub fn open_recent(&self) -> Option<&str> {
        self.open_recent.as_deref()
    }

    /// The meeting whose panel the detail pane shows: an open Recent
    /// meeting, else the post-stop panel. `None` without speakers.
    pub fn panel(&self, recording: bool) -> Option<String> {
        self.open_recent
            .clone()
            .or_else(|| self.saved_id(recording))
            .filter(|id| !self.speakers_of(id).is_empty())
    }

    /// The panel stopped being drawn (e.g. a meeting started): an open
    /// rename or merge is dropped so it can't reappear later with an old
    /// draft. A running request is left to finish.
    pub fn forget_undrawn_edit(&mut self, drawn: bool) {
        if !drawn && !self.editor.busy && self.editor.mode != SpeakerMode::List {
            self.editor.cancel();
            self.merge_choice.clear();
        }
    }

    /// The speakers of `meeting_id` as shown.
    pub fn speakers_of(&self, meeting_id: &str) -> Vec<MeetingSpeaker> {
        if let Some(saved) = &self.saved {
            if hub_model::saved_meeting_id(saved).as_deref() == Some(meeting_id) {
                return hub_model::meeting_speakers(saved);
            }
        }
        self.cache
            .get(meeting_id)
            .cloned()
            .flatten()
            .unwrap_or_default()
    }

    /// Whether `meeting_id` has an answer cached (speakers or a 404).
    pub fn is_cached(&self, meeting_id: &str) -> bool {
        self.cache.contains_key(meeting_id)
    }

    /// New speakers for `meeting_id` (an edit reply or a fresh read);
    /// `None` means it has no speaker data any more.
    fn store(&mut self, meeting_id: &str, payload: Option<&Value>) {
        let speakers = payload
            .map(hub_model::meeting_speakers)
            .filter(|list| !list.is_empty());
        if let Some(saved) = self.saved.as_mut() {
            if saved.get("meeting_id").and_then(Value::as_str) == Some(meeting_id) {
                saved["speakers"] = payload
                    .and_then(|value| value.get("speakers"))
                    .filter(|_| speakers.is_some())
                    .cloned()
                    .unwrap_or_else(|| json!([]));
            }
        }
        // The saved meeting is cached too: once the next meeting starts its
        // panel hides and its Recent row needs this answer.
        self.cache.insert(meeting_id.to_string(), speakers.clone());
        self.drop_stale_merge_choice();
        if speakers.is_none() {
            if self.open_recent.as_deref() == Some(meeting_id) {
                self.open_recent = None;
            }
            if self.editor.meeting_id == meeting_id {
                self.editor.reset();
                self.merge_choice.clear();
            }
        }
    }

    // ------------------------------------------------------------ probes ---

    /// Read `meeting_id` again with the next probe.
    pub fn request(&mut self, meeting_id: &str) {
        self.queue.insert(meeting_id.to_string());
    }

    /// The next probe to send, unless one is running or nothing is queued.
    pub fn next_probe(&mut self) -> Option<Probe> {
        if self.in_flight || self.queue.is_empty() {
            return None;
        }
        self.in_flight = true;
        Some(Probe {
            ids: std::mem::take(&mut self.queue).into_iter().collect(),
            seq: self.seq,
        })
    }

    pub fn probe_running(&self) -> bool {
        self.in_flight
    }

    /// A probe answered. Sent before the latest edit reply, its reads may
    /// predate the edit: every id is read again instead. Failures are not
    /// cached.
    pub fn probe_finished(&mut self, probe: Probe, results: ProbeResults) {
        self.in_flight = false;
        if probe.seq != self.seq {
            self.queue.extend(probe.ids);
        } else {
            for (meeting_id, result) in results {
                if let Ok(payload) = result {
                    self.store(&meeting_id, payload.as_ref());
                }
            }
        }
        if std::mem::take(&mut self.after_reconnect) {
            // Only ids still uncached: a 404 stays cached.
            self.queue_uncached();
        }
    }

    /// The server answers again after being unreachable (or for the first
    /// time). A running probe may still fail against the old connection, so
    /// the uncached ids are queued once it finishes.
    pub fn reconnected(&mut self) {
        if self.in_flight {
            self.after_reconnect = true;
        } else {
            self.queue_uncached();
        }
    }

    // ------------------------------------------------------------- edits ---

    /// Start renaming `speaker_id` of `meeting_id`.
    pub fn begin_rename(&mut self, meeting_id: &str, speaker_id: &str) -> bool {
        let started = self
            .editor
            .begin(meeting_id, SpeakerMode::Rename(speaker_id.to_string()));
        if started {
            self.merge_choice.clear();
        }
        started
    }

    /// Start merging `speaker_id` of `meeting_id` into a speaker yet to pick.
    pub fn begin_merge(&mut self, meeting_id: &str, speaker_id: &str) -> bool {
        let started = self
            .editor
            .begin(meeting_id, SpeakerMode::Merge(speaker_id.to_string()));
        if started {
            self.merge_choice.clear();
        }
        started
    }

    pub fn cancel(&mut self) {
        self.editor.cancel();
        if self.editor.mode == SpeakerMode::List {
            self.merge_choice.clear();
        }
    }

    /// The merge picker for the open merge (empty when none).
    pub fn merge_picker(&self) -> (Vec<MergeTarget>, MergePicker) {
        let speakers = self.speakers_of(&self.editor.meeting_id);
        let items = match &self.editor.mode {
            SpeakerMode::Merge(from) => hub_model::merge_target_items(&speakers, from),
            _ => Vec::new(),
        };
        let picker = hub_model::merge_picker(&items, &speakers, &self.merge_choice);
        (items, picker)
    }

    /// The picker showed `title`: pick exactly that item (or nothing).
    pub fn choose_merge_title(&mut self, title: &str) {
        let (items, _) = self.merge_picker();
        self.merge_choice = hub_model::merge_choice_for_title(&items, title);
        self.editor.error.clear();
    }

    /// Save the open rename (with `name`) or merge. `None` when there is
    /// nothing to send: no edit open, one running, a blank name, or a merge
    /// target that no longer names another speaker (merging is not undoable,
    /// so it is never guessed; the user picks again).
    pub fn commit(&mut self, name: &str) -> Option<EditRequest> {
        let meeting_id = self.editor.meeting_id.clone();
        let change = match self.editor.mode.clone() {
            SpeakerMode::List => return None,
            SpeakerMode::Rename(speaker_id) => {
                let name = name.trim();
                if name.is_empty() {
                    if !self.editor.busy {
                        self.editor.error = NAME_NEEDED.into();
                    }
                    return None;
                }
                Change::Rename {
                    speaker_id,
                    name: name.to_string(),
                }
            }
            SpeakerMode::Merge(from) => {
                let speakers = self.speakers_of(&meeting_id);
                let Some(into) =
                    hub_model::resolve_merge_target(&speakers, &from, &self.merge_choice)
                else {
                    if !self.editor.busy {
                        self.merge_choice.clear();
                        self.editor.error = hub_model::MERGE_PICK_AGAIN.into();
                    }
                    return None;
                };
                Change::Merge { from, into }
            }
        };
        let token = self.editor.start()?;
        Some(EditRequest {
            token,
            meeting_id,
            change,
        })
    }

    /// An edit answered. Any reply means the server may have changed this
    /// meeting, so reads already on their way are older than it. A reply the
    /// editor no longer expects (panel closed, moved to another meeting, a
    /// newer edit) is not applied; the meeting is read again instead.
    pub fn edit_finished(&mut self, request: &EditRequest, reply: EditReply) {
        self.seq = self.seq.wrapping_add(1);
        let error = reply.result.as_ref().err().cloned();
        if !self
            .editor
            .finish(request.token, &request.meeting_id, error.as_deref())
        {
            self.queue.insert(request.meeting_id.clone());
            return;
        }
        match (reply.result, reply.refetch) {
            (Ok(payload), _) => self.store(&request.meeting_id, Some(&payload)),
            (Err(_), Some(Ok(payload))) => self.store(&request.meeting_id, payload.as_ref()),
            (Err(_), _) => {}
        }
        if self.editor.mode == SpeakerMode::List {
            self.merge_choice.clear();
        }
    }
}

// ---------------------------------------------------------- blocking calls ---

/// Read each meeting's speakers.
pub fn probe(client: &dyn MeetingClient, ids: &[String]) -> ProbeResults {
    ids.iter()
        .map(|id| (id.clone(), client.speakers(id).map_err(|e| e.0)))
        .collect()
}

/// Send one edit. `pending` (a registered pending write, so a close or quit
/// waits for the durable write) is dropped once the write answered, before
/// the refetch read after a refusal.
pub fn apply_edit<G>(client: &dyn MeetingClient, request: &EditRequest, pending: G) -> EditReply {
    let result = match &request.change {
        Change::Rename { speaker_id, name } => {
            client.rename_speaker(&request.meeting_id, speaker_id, name)
        }
        Change::Merge { from, into } => client.merge_speakers(&request.meeting_id, from, into),
    }
    .map_err(|e| e.0);
    drop(pending);
    let refetch = result
        .is_err()
        .then(|| client.speakers(&request.meeting_id).map_err(|e| e.0));
    EditReply { result, refetch }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_ui::backends::meeting::{
        FakeMeetingClient, MeetingCall, INVALID_SPEAKER_NAME, SERVER_DOWN,
    };

    const A: &str = "0123456789abcdef0123456789abcdef";
    const B: &str = "fedcba9876543210fedcba9876543210";

    fn two() -> Vec<Value> {
        vec![
            json!({"id": "S1", "label": "Speaker 1", "name": null, "source": "others", "seconds": 312.4}),
            json!({"id": "S2", "label": "Priya Raman", "name": "Priya Raman", "source": "others", "seconds": 95.0}),
        ]
    }

    fn saved(id: &str, speakers: Vec<Value>) -> Value {
        json!({"title": "Sync", "notes_path": "/n.md", "meeting_id": id,
               "diarization": "ok", "speakers": speakers})
    }

    fn note(dir: &Path, name: &str, id: Option<&str>) -> RecentNote {
        let path = dir.join(name);
        let front = id.map_or(String::new(), |id| format!("meeting_id: {id}\n"));
        std::fs::write(&path, format!("---\ntitle: x\n{front}---\n# x\n")).unwrap();
        RecentNote {
            title: name.into(),
            date_label: String::new(),
            path: path.to_string_lossy().into_owned(),
        }
    }

    fn run_probe(store: &mut SpeakerStore, client: &FakeMeetingClient) {
        while let Some(p) = store.next_probe() {
            let results = probe(client, &p.ids);
            store.probe_finished(p, results);
        }
    }

    fn speaker_calls(client: &FakeMeetingClient) -> usize {
        client
            .calls()
            .iter()
            .filter(|c| matches!(c, MeetingCall::Speakers(_)))
            .count()
    }

    #[test]
    fn notes_without_a_meeting_id_never_cause_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let client = FakeMeetingClient::new();
        let mut store = SpeakerStore::default();
        store.set_recent(&[
            note(dir.path(), "a.md", None),
            note(dir.path(), "b.md", None),
        ]);
        assert!(store.next_probe().is_none());
        assert_eq!(speaker_calls(&client), 0);
        assert_eq!(store.panel(false), None);
    }

    #[test]
    fn a_404_is_cached_and_hides_the_action_while_failures_are_retried_on_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let client = FakeMeetingClient::new();
        client.seed_speakers(A, two());
        let notes = [
            note(dir.path(), "a.md", Some(A)),
            note(dir.path(), "b.md", Some(B)),
        ];
        let mut store = SpeakerStore::default();
        store.set_recent(&notes);
        run_probe(&mut store, &client);
        assert_eq!(store.recent_action(&notes[0].path, false), Some(A.into()));
        assert_eq!(store.recent_action(&notes[1].path, false), None, "404");
        assert!(store.is_cached(B));
        let before = speaker_calls(&client);
        // Reloading the list asks for nothing new: both are cached.
        store.set_recent(&notes);
        run_probe(&mut store, &client);
        assert_eq!(speaker_calls(&client), before);

        // A failure is not cached; the reconnect reads it once, never the 404.
        let mut store = SpeakerStore::default();
        client.set_down(Some(SERVER_DOWN));
        store.set_recent(&notes);
        run_probe(&mut store, &client);
        assert!(!store.is_cached(A) && !store.is_cached(B));
        client.set_down(None);
        client.seed_speakers(B, vec![]);
        store.reconnected();
        run_probe(&mut store, &client);
        assert!(store.is_cached(A) && store.is_cached(B));
        let calls = speaker_calls(&client);
        store.reconnected();
        run_probe(&mut store, &client);
        assert_eq!(speaker_calls(&client), calls, "nothing uncached is left");
    }

    #[test]
    fn a_reconnect_during_a_probe_waits_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let client = FakeMeetingClient::new();
        client.seed_speakers(A, two());
        let notes = [note(dir.path(), "a.md", Some(A))];
        let mut store = SpeakerStore::default();
        store.set_recent(&notes);
        client.set_down(Some(SERVER_DOWN));
        let running = store.next_probe().unwrap();
        let failed = probe(&client, &running.ids);
        store.reconnected();
        assert!(store.next_probe().is_none(), "one probe at a time");
        client.set_down(None);
        store.probe_finished(running, failed);
        let again = store.next_probe().expect("re-read after the probe");
        assert_eq!(again.ids, vec![A.to_string()]);
    }

    #[test]
    fn a_probe_or_poll_sent_before_an_edit_does_not_replace_it() {
        let client = FakeMeetingClient::new();
        client.seed_speakers(A, two());
        let mut store = SpeakerStore::default();
        store.set_saved(Some(saved(A, two())));
        store.request(A);
        let early = store.next_probe().unwrap();
        let early_results = probe(&client, &early.ids);
        let ticket = store.poll_started();

        assert!(store.begin_rename(A, "S1"));
        let request = store.commit("Ana").unwrap();
        let reply = apply_edit(&client, &request, ());
        store.edit_finished(&request, reply);
        assert_eq!(store.speakers_of(A)[0].label, "Ana");

        // The probe read the pre-edit list: not applied, read again.
        store.probe_finished(early, early_results);
        assert_eq!(store.speakers_of(A)[0].label, "Ana");
        let again = store.next_probe().unwrap();
        assert_eq!(again.ids, vec![A.to_string()]);
        // A status poll sent before the edit with the old speakers: kept local.
        store.adopt_polled(ticket, Some(saved(A, two())), false);
        assert_eq!(store.speakers_of(A)[0].label, "Ana");
        // ... but a poll naming another meeting is adopted.
        store.adopt_polled(ticket, Some(saved(B, two())), false);
        assert_eq!(store.saved_id(false).as_deref(), Some(B));
    }

    #[test]
    fn a_late_reply_for_another_meeting_or_token_is_dropped_and_re_read() {
        let client = FakeMeetingClient::new();
        client.seed_speakers(A, two());
        client.seed_speakers(B, two());
        let mut store = SpeakerStore::default();
        store.set_saved(Some(saved(A, two())));
        store.request(B);
        run_probe(&mut store, &client);
        assert!(store.begin_rename(A, "S1"));
        let request = store.commit("Ana").unwrap();
        // The panel is reseeded and moves to meeting B before the reply lands.
        store.editor.reset();
        assert!(store.toggle_recent(B));
        run_probe(&mut store, &client);
        let reply = apply_edit(&client, &request, ());
        store.edit_finished(&request, reply);
        assert_eq!(store.editor.meeting_id, B);
        assert_eq!(
            store.speakers_of(B)[0].label,
            "Speaker 1",
            "never applied to B"
        );
        assert!(store.next_probe().unwrap().ids.contains(&A.to_string()));
    }

    #[test]
    fn refused_and_blank_renames_stay_inline() {
        let client = FakeMeetingClient::new();
        client.seed_speakers(A, two());
        let mut store = SpeakerStore::default();
        store.set_saved(Some(saved(A, two())));
        assert!(store.begin_rename(A, "S2"));
        assert_eq!(store.commit("   "), None);
        assert_eq!(store.editor.error, NAME_NEEDED);
        assert!(client.calls().is_empty(), "blank names are never sent");

        // The server refuses: error inline, rename still open, re-read once.
        let request = store.commit("Ana").unwrap();
        client.set_down(Some(SERVER_DOWN));
        let reply = apply_edit(&client, &request, ());
        assert!(reply.refetch.is_some());
        store.edit_finished(&request, reply);
        assert_eq!(store.editor.error, SERVER_DOWN);
        assert_eq!(store.editor.mode, SpeakerMode::Rename("S2".into()));
        client.set_down(None);
        let request = store.commit("Ana").unwrap();
        let reply = EditReply {
            result: Err(INVALID_SPEAKER_NAME.into()),
            refetch: Some(Ok(Some(
                json!({"meeting_id": A, "speakers": [two()[0].clone()]}),
            ))),
        };
        store.edit_finished(&request, reply);
        assert_eq!(store.editor.error, INVALID_SPEAKER_NAME);
        assert_eq!(store.speakers_of(A).len(), 1, "refetch applied");
    }

    #[test]
    fn merge_goes_to_the_picked_speaker_only() {
        let client = FakeMeetingClient::new();
        let dupes = vec![
            json!({"id": "S1", "label": "Sam", "seconds": 10.0}),
            json!({"id": "S2", "label": "Sam", "seconds": 10.0}),
            json!({"id": "S3", "label": "Lee", "seconds": 5.0}),
        ];
        client.seed_speakers(A, dupes.clone());
        let mut store = SpeakerStore::default();
        store.set_saved(Some(saved(A, dupes)));
        let titles = hub_model::speaker_titles(&store.speakers_of(A));
        assert_eq!(titles, vec!["Sam (S1)", "Sam (S2)", "Lee"]);

        assert!(store.begin_merge(A, "S3"));
        let (_, picker) = store.merge_picker();
        assert!(!picker.chosen, "nothing is preselected");
        assert_eq!(store.commit(""), None);
        assert_eq!(store.editor.error, hub_model::MERGE_PICK_AGAIN);
        store.choose_merge_title("Sam (S2)");
        assert_eq!(store.merge_choice, "S2");
        let request = store.commit("").unwrap();
        assert_eq!(
            request.change,
            Change::Merge {
                from: "S3".into(),
                into: "S2".into()
            }
        );
        let reply = apply_edit(&client, &request, ());
        store.edit_finished(&request, reply);
        let left = store.speakers_of(A);
        assert_eq!(left.len(), 2);
        assert_eq!(left[1].seconds, 15.0);
        assert_eq!(store.editor.mode, SpeakerMode::List);

        // A target that vanished (merged elsewhere) asks again; nothing sent.
        assert!(store.begin_merge(A, "S1"));
        store.choose_merge_title("Sam (S2)");
        store.store(
            A,
            Some(&json!({"speakers": [{"id": "S1", "label": "Sam", "seconds": 1.0}]})),
        );
        let calls = client.calls().len();
        assert_eq!(store.commit(""), None);
        assert_eq!(store.editor.error, hub_model::MERGE_PICK_AGAIN);
        assert_eq!(client.calls().len(), calls);
        let (items, _) = store.merge_picker();
        assert!(items.is_empty(), "one speaker left: no target");
    }

    #[test]
    fn the_saved_meeting_stays_cached_for_its_recent_row_once_recording() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SpeakerStore::default();
        store.set_saved(Some(saved(A, two())));
        let notes = [note(dir.path(), "a.md", Some(A))];
        store.set_recent(&notes);
        assert!(store.next_probe().is_none(), "already known from stop");
        assert_eq!(store.panel(false).as_deref(), Some(A));
        assert_eq!(store.recent_action(&notes[0].path, false), None);
        // A new meeting starts: the saved panel hides, the row offers it.
        assert_eq!(store.panel(true), None);
        assert_eq!(store.recent_action(&notes[0].path, true), Some(A.into()));
        // A stop without speakers shows no panel.
        store.set_saved(Some(json!({"title": "x", "meeting_id": null})));
        assert_eq!(store.panel(false), None);
    }

    #[test]
    fn data_vanishing_closes_the_panel_and_resets_the_editor() {
        let mut store = SpeakerStore::default();
        store.store(A, Some(&json!({"speakers": two()})));
        assert!(store.toggle_recent(A));
        assert!(store.begin_rename(A, "S1"));
        store.store(A, None);
        assert_eq!(store.open_recent(), None);
        assert_eq!(store.editor.mode, SpeakerMode::List);
        assert_eq!(store.panel(false), None);
    }

    /// Recent panel open -> Start -> Stop of a labelled meeting: the saved
    /// meeting is shown. Reopening Recent afterwards survives status polls.
    #[test]
    fn a_local_stop_presents_the_saved_meeting_and_polls_keep_recent_open() {
        let mut store = SpeakerStore::default();
        store.store(B, Some(&json!({"speakers": two()})));
        assert!(store.toggle_recent(B));
        assert!(store.begin_rename(B, "S1"));
        assert_eq!(
            store.panel(true).as_deref(),
            Some(B),
            "kept while recording"
        );

        store.present_saved(Some(saved(A, two())));
        assert_eq!(store.panel(false).as_deref(), Some(A));
        assert_eq!(store.open_recent(), None);
        assert_eq!(
            store.editor.mode,
            SpeakerMode::List,
            "the old draft is gone"
        );

        assert!(store.toggle_recent(B));
        for _ in 0..3 {
            let ticket = store.poll_started();
            store.adopt_polled(ticket, Some(saved(A, two())), false);
        }
        assert_eq!(
            store.panel(false).as_deref(),
            Some(B),
            "polls never close it"
        );

        // A stop without speakers leaves an open Recent panel alone.
        store.present_saved(Some(json!({"title": "x", "meeting_id": null})));
        assert_eq!(store.panel(false).as_deref(), Some(B));
    }

    /// A running edit on the Recent panel keeps its token across the
    /// switch and still applies to its own meeting.
    #[test]
    fn presenting_the_saved_meeting_keeps_a_running_edit_valid() {
        let client = FakeMeetingClient::new();
        client.seed_speakers(B, two());
        let mut store = SpeakerStore::default();
        store.store(B, Some(&json!({"speakers": two()})));
        assert!(store.toggle_recent(B));
        assert!(store.begin_rename(B, "S2"));
        let request = store.commit("Ana").unwrap();
        store.present_saved(Some(saved(A, two())));
        assert_eq!(store.panel(false).as_deref(), Some(A));
        let reply = apply_edit(&client, &request, ());
        store.edit_finished(&request, reply);
        assert!(!store.editor.busy);
        assert_eq!(store.speakers_of(B)[1].label, "Ana");
        assert_eq!(store.speakers_of(A)[1].label, "Priya Raman");
    }

    /// Another client merges the chosen target away: the next poll drops
    /// the choice (never re-picked by itself) and nothing is sent.
    #[test]
    fn a_poll_that_removes_the_chosen_target_drops_the_choice() {
        let mut store = SpeakerStore::default();
        store.set_saved(Some(saved(A, two())));
        assert!(store.begin_merge(A, "S1"));
        store.choose_merge_title("Priya Raman");
        assert!(store.merge_picker().1.chosen);

        let ticket = store.poll_started();
        store.adopt_polled(ticket, Some(saved(A, vec![two()[0].clone()])), false);
        assert_eq!(store.merge_choice, "");
        let (items, picker) = store.merge_picker();
        assert!(items.is_empty() && !picker.chosen);
        assert_eq!(
            picker.titles,
            vec![hub_model::MERGE_PLACEHOLDER.to_string()]
        );
        assert_eq!(store.commit(""), None);
        assert_eq!(store.editor.error, hub_model::MERGE_PICK_AGAIN);

        // The same id coming back is not a pick either.
        let ticket = store.poll_started();
        store.adopt_polled(ticket, Some(saved(A, two())), false);
        assert!(!store.merge_picker().1.chosen);
    }

    /// While this window's "Notes saved" banner is kept, a poll naming
    /// another meeting (another client's Stop) never replaces the panel.
    #[test]
    fn a_pinned_local_save_ignores_another_polled_meeting() {
        let mut store = SpeakerStore::default();
        store.present_saved(Some(saved(A, two())));
        for _ in 0..3 {
            let ticket = store.poll_started();
            store.adopt_polled(ticket, Some(saved(B, two())), true);
        }
        assert_eq!(store.saved_id(false).as_deref(), Some(A));
        assert_eq!(store.panel(false).as_deref(), Some(A));
        // A poll with nothing saved doesn't clear it either.
        let ticket = store.poll_started();
        store.adopt_polled(ticket, None, true);
        assert_eq!(store.panel(false).as_deref(), Some(A));
        // Once the banner is gone the poll is adopted again.
        let ticket = store.poll_started();
        store.adopt_polled(ticket, Some(saved(B, two())), false);
        assert_eq!(store.panel(false).as_deref(), Some(B));
    }

    /// An unlabelled local save shows no panel, and a labelled meeting
    /// saved elsewhere is not attached to its banner.
    #[test]
    fn a_pinned_unlabelled_save_never_shows_a_polled_meeting() {
        let mut store = SpeakerStore::default();
        store.present_saved(Some(json!({"title": "x", "notes_path": "/local.md"})));
        let ticket = store.poll_started();
        store.adopt_polled(ticket, Some(saved(B, two())), true);
        assert_eq!(store.panel(false), None);
        assert_eq!(store.saved_id(false), None);
    }

    /// Id-less records are told apart by their paths (or not at all).
    #[test]
    fn id_less_records_are_kept_apart() {
        let local = json!({"title": "x", "meeting_id": null, "notes_path": "/a.md"});
        let other = json!({"title": "y", "meeting_id": null, "notes_path": "/b.md"});
        assert!(!same_record(Some(&local), Some(&other)));
        assert!(same_record(Some(&local), Some(&local.clone())));
        let bare = json!({"title": "x"});
        assert!(!same_record(Some(&bare), Some(&json!({"title": "y"}))));
        assert!(!same_record(Some(&bare), Some(&local)));
        assert!(!same_record(Some(&bare), None));

        let mut store = SpeakerStore::default();
        store.present_saved(Some(local.clone()));
        let ticket = store.poll_started();
        store.adopt_polled(ticket, Some(other.clone()), true);
        assert_eq!(store.saved, Some(local));
        // Unpinned, a different id-less record is adopted.
        let ticket = store.poll_started();
        store.adopt_polled(ticket, Some(other.clone()), false);
        assert_eq!(store.saved, Some(other));
    }

    /// Pinned, the same meeting still takes fresh speakers; a poll sent
    /// before the latest edit does not undo it.
    #[test]
    fn a_pinned_save_takes_fresh_data_of_the_same_meeting_only() {
        let client = FakeMeetingClient::new();
        client.seed_speakers(A, two());
        let mut store = SpeakerStore::default();
        store.present_saved(Some(saved(A, two())));
        let ticket = store.poll_started();
        store.adopt_polled(ticket, Some(saved(A, vec![two()[0].clone()])), true);
        assert_eq!(store.speakers_of(A).len(), 1, "fresh poll applied");

        store.adopt_polled(store.poll_started(), Some(saved(A, two())), true);
        let stale = store.poll_started();
        assert!(store.begin_rename(A, "S1"));
        let request = store.commit("Ana").unwrap();
        let reply = apply_edit(&client, &request, ());
        store.edit_finished(&request, reply);
        store.adopt_polled(stale, Some(saved(A, two())), true);
        assert_eq!(store.speakers_of(A)[0].label, "Ana", "stale poll ignored");
    }

    /// A Recent panel opened under a pinned banner survives its polls.
    #[test]
    fn a_recent_panel_survives_pinned_polls() {
        let mut store = SpeakerStore::default();
        store.store(B, Some(&json!({"speakers": two()})));
        store.present_saved(Some(saved(A, two())));
        assert!(store.toggle_recent(B));
        for id in [A, B] {
            let ticket = store.poll_started();
            store.adopt_polled(ticket, Some(saved(id, two())), true);
        }
        assert_eq!(store.panel(false).as_deref(), Some(B));
        assert_eq!(store.saved_id(false).as_deref(), Some(A));
    }
}
