"""People (speaker recognition) model: consent copy, helpers, state machine, layout."""

import os

import pytest

from localflow.hub_model import SPEAKER_ERROR_MAX, speaker_row_frames
from localflow.meeting_client import MeetingClientError
from localflow.people_model import (
    CONSENT_DRAFT_NOTE,
    CONSENT_POINTS,
    ENROLL_REMOTE,
    EXPORT_IN_VAULT,
    EXPORT_NOT_EMPTY,
    EXPORT_REMINDER,
    MIC_ERROR,
    PEOPLE_GENERIC_ERROR,
    PEOPLE_ROWS_MAX,
    PEOPLE_TOGGLE_H,
    SAVE_FAILED,
    TOGGLE_NEEDS_DIARIZATION,
    TOGGLE_OFF,
    TOGGLE_ON,
    PeopleState,
    can_remember,
    consent_text,
    export_notice,
    is_generic_label,
    is_local_url,
    local_base_url,
    needs_reenroll,
    path_inside_vault,
    people_error_text,
    people_footer_frames,
    people_layout,
    people_status_frames,
    person_rename_frames,
    person_row_frames,
    person_rows,
    recognition_update,
)

LOCAL = "http://127.0.0.1:8765"
A = "a" * 32
B = "b" * 32
EMBEDDER = "wespeaker-v1"


def _payload(*people, embedder=EMBEDDER):
    return {"speakers": list(people), "embedder": embedder}


def _vp(vid=A, name="Dana", model_id=EMBEDDER, samples=3, created="2026-11-12T09:30:00Z"):
    return {"id": vid, "name": name, "model_id": model_id, "samples": samples,
            "created": created}


def _ready(*people, diarization=True, recognition=True):
    state = PeopleState()
    request = state.set_flags(diarization, recognition)
    if request is not None:
        _op, _args, token = request
        state.loaded(token, _payload(*people), None)
    return state


# ------------------------------------------------------------------- copy ---

def test_consent_copy_says_what_the_brief_requires():
    assert len(CONSENT_POINTS) == 4
    text = " ".join(CONSENT_POINTS)
    for phrase in ("biometric", "~/.localflow/speakers/", "Obsidian vault", "LLM",
                   "permission", "Forget all", "`lf speakers forget --all`"):
        assert phrase in text, phrase
    assert "does not delete" in CONSENT_POINTS[3] or "not delete" in CONSENT_POINTS[3]
    assert CONSENT_DRAFT_NOTE and "draft" in CONSENT_DRAFT_NOTE.lower()
    assert consent_text().endswith(CONSENT_DRAFT_NOTE)
    assert all(point in consent_text() for point in CONSENT_POINTS)


def test_recognition_update_is_a_meetings_bool():
    assert recognition_update(True) == {"meetings": {"speaker_recognition": True}}
    assert recognition_update(0) == {"meetings": {"speaker_recognition": False}}


# ----------------------------------------------------------- local server ---

@pytest.mark.parametrize("url,expected", [
    ("http://127.0.0.1:8765", "http://127.0.0.1:8765"),
    ("http://127.0.0.1:8765/", "http://127.0.0.1:8765"),
    ("http://127.4.5.6:1", "http://127.4.5.6:1"),
    ("http://[::1]:8765", "http://[::1]:8765"),
    ("http://localhost:8765", "http://127.0.0.1:8765"),
    ("http://LOCALHOST", "http://127.0.0.1"),
])
def test_local_base_url_accepts_loopback_literals(url, expected):
    assert local_base_url(url) == expected
    assert is_local_url(url)


@pytest.mark.parametrize("url", [
    "http://0.0.0.0:8765", "http://192.168.1.20:8765", "http://10.0.0.2",
    "http://myhost:8765", "https://127.0.0.1:8765", "http://localhost.evil.com",
    "http://127.0.0.1.evil.com", "http://user@127.0.0.1:8765", "ftp://127.0.0.1",
    "127.0.0.1:8765", "", None, "http://[::1", "http://127.0.0.1:notaport",
])
def test_local_base_url_refuses_everything_else(url):
    assert local_base_url(url) is None
    assert not is_local_url(url)


# -------------------------------------------------------------- the vault ---

def test_path_inside_vault(tmp_path, monkeypatch):
    vault = tmp_path / "vault"
    (vault / "notes").mkdir(parents=True)
    outside = tmp_path / "exports"
    assert path_inside_vault(vault, vault)
    assert path_inside_vault(vault / "notes" / "new", vault)
    assert not path_inside_vault(outside, vault)
    assert not path_inside_vault(outside, "")
    assert not path_inside_vault(outside, None)
    link = tmp_path / "sneaky"
    os.symlink(vault / "notes", link)
    assert path_inside_vault(link / "new", vault)

    def broken(self, *args, **kwargs):
        raise OSError("loop")

    monkeypatch.setattr("pathlib.Path.resolve", broken)
    assert path_inside_vault(outside, vault)  # can't tell: refuse


# ------------------------------------------------------------------- rows ---

def test_needs_reenroll():
    assert needs_reenroll({"model_id": "old"}, EMBEDDER)
    assert not needs_reenroll({"model_id": EMBEDDER}, EMBEDDER)
    assert not needs_reenroll({"model_id": "old"}, None)  # older server: no claim
    assert needs_reenroll({}, EMBEDDER)


def test_person_rows_filters_escapes_and_formats():
    rows = person_rows(_payload(
        _vp(A, "Dana"),
        _vp(A, "Duplicate"),
        _vp("NOT-AN-ID", "Bad"),
        _vp(B, "\u202eEvil", model_id="old", samples=1, created="junk"),
        "junk",
        {"id": "c" * 32, "name": None},
    ))
    assert [row["id"] for row in rows] == [A, B, "c" * 32]
    assert rows[0] == {"id": A, "name": "Dana", "meta": "3 samples · Nov 12, 2026",
                       "stale": False}
    assert "\u202e" not in rows[1]["name"] and "\\u202e" in rows[1]["name"]
    assert rows[1]["meta"] == "1 sample" and rows[1]["stale"] is True
    assert rows[2]["name"] == "Unnamed speaker" and rows[2]["meta"] == ""
    assert person_rows(None) == [] and person_rows({"speakers": "x"}) == []


def test_person_rows_are_capped():
    many = [_vp(f"{index:032x}", f"P{index}") for index in range(PEOPLE_ROWS_MAX + 20)]
    assert len(person_rows(_payload(*many))) == PEOPLE_ROWS_MAX


def test_generic_labels_and_can_remember():
    for label in ("Speaker 1", "Speaker 12", "Others", "Unnamed speaker", " Speaker 3 "):
        assert is_generic_label(label), label
    for label in ("Sarah", "Speaker", "Speaker one", "Me", "", None):
        assert not is_generic_label(label), label
    assert can_remember(True, True)
    assert not can_remember(True, False) and not can_remember(False, True)


# ----------------------------------------------------------------- errors ---

@pytest.mark.parametrize("detail", [
    "speaker recognition is off", "no speech detected", "unknown speaker",
    "invalid speaker name", "could not decode the audio", "decoded audio is empty",
])
def test_people_error_text_maps_fixed_server_details(detail):
    text = people_error_text("enroll", MeetingClientError(detail, 400))
    assert text.startswith("Couldn't enroll the voice: ")
    assert detail not in text  # plain copy instead


def test_people_error_text_keeps_client_text_and_hides_everything_else():
    assert people_error_text("rename", MeetingClientError("name taken", 409)) == (
        "Couldn't rename: name taken"
    )
    leak = "http://127.0.0.1:8000/?token=abc"
    assert people_error_text("load", Exception(leak)) == (
        f"Couldn't load people: {PEOPLE_GENERIC_ERROR}"
    )
    assert leak not in people_error_text("load", MeetingClientError(f"bad {leak}", 500))
    assert PEOPLE_GENERIC_ERROR in people_error_text("load", MeetingClientError(leak))
    assert MIC_ERROR in people_error_text("record", OSError("PortAudio /dev/snd"))
    assert "/dev/snd" not in people_error_text("record", OSError("PortAudio /dev/snd"))
    long = people_error_text("export", MeetingClientError("x" * 1000, 500))
    assert len(long) <= len("Couldn't export: ") + SPEAKER_ERROR_MAX + 1


def test_export_notice():
    notice = export_notice(2, "Voices\u202e")
    assert notice.startswith("Exported 2 voiceprints to “Voices\\u202e”.")
    assert notice.endswith(EXPORT_REMINDER)
    assert export_notice(1, "x").startswith("Exported 1 voiceprint to")


# ------------------------------------------------------------------ flags ---

def test_both_flags_off_is_invisible_and_quiet():
    state = PeopleState()
    assert state.set_flags(False, False) is None
    assert not state.visible and state.revision == 0
    assert state.refresh() is None
    assert state.request_recognition(True) is None


def test_diarization_only_shows_the_off_card_without_a_load():
    state = PeopleState()
    assert state.set_flags(True, False) is None
    assert state.visible and not state.active
    assert state.toggle_enabled and state.toggle_detail == TOGGLE_OFF
    assert state.refresh() is None


def test_recognition_without_diarization_disables_the_toggle_with_the_reason():
    state = PeopleState()
    assert state.set_flags(False, True) is None
    assert state.visible and not state.active
    assert not state.toggle_enabled
    assert state.toggle_detail == TOGGLE_NEEDS_DIARIZATION
    assert state.request_recognition(False) is None


def test_consent_path():
    state = PeopleState()
    state.set_flags(True, False)
    assert state.request_recognition(True) == "consent" and state.consent_open
    assert state.request_recognition(True) is None  # the sheet is already open
    assert state.consent_result(False) is False and not state.consent_open
    assert not state.recognition  # declined: nothing to write
    assert state.request_recognition(True) == "consent"
    assert state.consent_result(True) is True
    request = state.recognition_saved(True)
    assert request == ("enrolled", (), state.seq)
    assert state.grace == 3 and state.phase == "loading" and state.toggle_detail == TOGGLE_ON
    assert state.consent_result(True) is False  # no sheet open: no write


def test_turning_off_needs_no_consent_and_clears_the_list():
    state = _ready(_vp())
    assert state.rows
    assert state.request_recognition(False) == "write"
    assert not state.consent_open
    seq, epoch = state.seq, state.epoch
    assert state.recognition_saved(False) is None
    assert state.rows == [] and state.phase == "" and not state.active
    assert state.seq == seq + 1 and state.epoch == epoch + 1


def test_save_failure_is_shown():
    state = PeopleState()
    state.set_flags(True, False)
    state.recognition_save_failed()
    assert state.error == SAVE_FAILED and state.message == SAVE_FAILED


def test_403_after_consent_retries_three_times_then_errors():
    state = PeopleState()
    state.set_flags(True, False)
    state.request_recognition(True)
    state.consent_result(True)
    _op, _args, token = state.recognition_saved(True)
    off = MeetingClientError("speaker recognition is off", 403)
    for _ in range(3):
        assert state.loaded(token, None, off) == "retry"
        assert state.phase == "loading"
    assert state.loaded(token, None, off) is True
    assert state.phase == "error" and "Speaker recognition is off" in state.load_error
    request = state.retry()
    assert request == ("enrolled", (), state.seq) and state.phase == "loading"
    assert state.loaded(state.seq, None, off) is True  # no grace left: no retry


def test_stale_load_is_dropped():
    state = PeopleState()
    _op, _args, old = state.set_flags(True, True)
    _op, _args, token = state.load()
    assert state.loaded(old, _payload(_vp()), None) is False
    assert state.phase == "loading"
    assert state.loaded(token, _payload(_vp()), None) is True
    assert state.phase == "ready" and state.embedder == EMBEDDER
    assert state.loaded(token, _payload(), None) is False  # already applied


def test_bad_payload_is_an_error():
    state = PeopleState()
    _op, _args, token = state.set_flags(True, True)
    assert state.loaded(token, {"speakers": None}, None) is True
    assert state.phase == "error" and PEOPLE_GENERIC_ERROR in state.load_error


# ----------------------------------------------------------------- rename ---

def test_rename_validation_unchanged_and_stale_finish():
    state = _ready(_vp(A, "Dana"), _vp(B, "Ari"))
    state.begin_rename(A)
    assert state.mode == "rename" and state.draft == "Dana"
    assert state.submit_rename("!!!") is None and state.error
    assert state.mode == "rename" and state.draft == "!!!"
    assert state.submit_rename("  Dana ") is None and state.mode == ""  # unchanged: closes
    state.begin_rename(A)
    request = state.submit_rename("  Dana   Lee ")
    assert request == ("rename_enrolled", (A, "Dana Lee"), state.epoch)
    assert state.busy == "rename"
    assert state.submit_rename("Again") is None  # busy
    assert state.finish(request[2] - 1, {}, None) is False and state.busy == "rename"
    assert state.finish(request[2], {"id": A, "name": "Dana Lee"}, None) is True
    assert state.busy == "" and state.mode == "" and state.reload is True


def test_rename_error_keeps_the_editor():
    state = _ready(_vp(A, "Dana"))
    state.begin_rename(A)
    _op, _args, token = state.submit_rename("Bo")
    assert state.finish(token, None, MeetingClientError("invalid speaker name", 400))
    assert state.mode == "rename" and state.draft == "Bo"
    assert state.error == "Couldn't rename: That name can't be used. Use letters or numbers."
    assert state.reload is False


def test_draft_from_another_field_is_ignored():
    state = _ready(_vp(A, "Dana"), _vp(B, "Ari"))
    state.begin_rename(A)
    state.set_draft("typed", A)
    state.set_draft("stale", B)
    state.set_draft("stale", "")
    assert state.draft == "typed"


def test_a_renamed_person_who_vanished_closes_the_edit():
    state = _ready(_vp(A, "Dana"), _vp(B, "Ari"))
    state.begin_rename(B)
    _op, _args, token = state.load()
    state.loaded(token, _payload(_vp(A, "Dana")), None)
    assert state.mode == ""


# ----------------------------------------------------------------- forget ---

def test_forget_and_forget_all_requests():
    state = _ready(_vp(A, "Dana"))
    assert state.submit_forget(B) is None
    request = state.submit_forget(A)
    assert request == ("forget", (A,), state.epoch) and state.busy == "forget"
    assert state.submit_forget_all() is None  # one action at a time
    state.finish(request[2], {"forgotten": 1}, None)
    assert state.reload is True
    request = state.submit_forget_all()
    assert request == ("forget", (None,), state.epoch) and state.busy == "forget_all"


# ----------------------------------------------------------------- export ---

def test_export_refusals_and_notice(tmp_path):
    vault = tmp_path / "vault"
    vault.mkdir()
    state = _ready(_vp())
    assert state.submit_export(str(vault / "voices"), str(vault), nonempty=False) is None
    assert state.error == EXPORT_IN_VAULT
    target = tmp_path / "out"
    assert state.submit_export(str(target), str(vault), nonempty=True) is None
    assert state.error == EXPORT_NOT_EMPTY
    assert state.submit_export(
        str(target), str(vault), nonempty=False, base_url="http://10.0.0.5:8765",
    ) is None
    assert state.error == ENROLL_REMOTE
    request = state.submit_export(str(target), str(vault), nonempty=False, base_url=LOCAL)
    assert request == ("export_enrolled", (str(target),), state.epoch)
    assert state.error == "" and state.busy == "export"
    assert state.finish(request[2], 2, None)
    assert state.notice == export_notice(2, "out") and state.error == ""
    assert state.reload is False and state.message == state.notice


# ----------------------------------------------------------------- enroll ---

def test_enroll_refuses_a_remote_server():
    state = _ready()
    state.begin_enroll("http://192.168.1.4:8765")
    assert state.error == ENROLL_REMOTE and state.mode == ""


def test_enroll_default_name():
    state = _ready(_vp(A, "Dana"))
    state.begin_enroll(LOCAL)
    assert state.mode == "enroll" and state.draft == "Me"
    taken = _ready(_vp(A, "me"))
    taken.begin_enroll(LOCAL)
    assert taken.mode == "enroll" and taken.draft == ""


def test_enroll_recording_lifecycle():
    state = _ready()
    state.begin_enroll(LOCAL)
    assert state.start_recording("   ") is None and state.error == "Type a name first."
    start = state.start_recording(" Me ")
    assert start == ("capture_start", (), state.rec_seq) and state.rec == "starting"
    assert not state.toggle_enabled
    assert state.recording_started(start[2], None, 100.0) is True
    assert state.rec == "recording"
    assert state.remaining(100.0) == 30 and state.remaining(100.2) == 30
    assert state.remaining(112.5) == 18 and state.remaining(500) == 0
    assert not state.can_stop(102.9) and state.can_stop(103.0)
    assert state.stop_recording(101.0) is None  # too short
    assert not state.due(129.9) and state.due(130.0)
    finish = state.stop_recording(110.0)
    assert finish == ("capture_finish", ("Me",), state.epoch)
    assert state.rec == "uploading" and state.busy == "enroll"
    assert state.cancel_recording() is False  # an upload can't be cancelled
    assert state.finish(finish[2], {"id": A, "name": "Me"}, None) is True
    assert state.mode == "" and state.rec == "" and state.reload is True
    assert state.notice


def test_cancel_drops_a_late_microphone_start():
    state = _ready()
    state.begin_enroll(LOCAL)
    _op, _args, token = state.start_recording("Me")
    assert state.cancel_recording() is True
    assert state.rec_seq == token + 1 and state.mode == "" and state.rec == ""
    assert state.recording_started(token, None, 1.0) is False
    assert state.rec == ""


def test_microphone_failure_returns_to_naming():
    state = _ready()
    state.begin_enroll(LOCAL)
    _op, _args, token = state.start_recording("Me")
    assert state.recording_started(token, OSError("no device"), 1.0)
    assert state.rec == "" and state.mode == "enroll" and state.draft == "Me"
    assert MIC_ERROR in state.error


def test_enroll_failure_returns_to_naming_with_the_draft():
    state = _ready()
    state.begin_enroll(LOCAL)
    _op, _args, token = state.start_recording("Dana")
    state.recording_started(token, None, 0.0)
    _op, _args, token = state.stop_recording(5.0)
    assert state.finish(token, None, MeetingClientError("no speech detected", 400))
    assert state.mode == "enroll" and state.rec == "" and state.draft == "Dana"
    assert "No speech was heard" in state.error and state.busy == ""


# ----------------------------------------------------------------- layout ---

def _overlaps(a, b):
    ax, ay, aw, ah = a
    bx, by, bw, bh = b
    return ax < bx + bw and bx < ax + aw and ay < by + bh and by < ay + ah


def _assert_no_overlaps(frames, skip=("divider",)):
    keys = [key for key in frames if key not in skip]
    for i, first in enumerate(keys):
        assert frames[first][2] > 0, first
        for second in keys[i + 1:]:
            assert not _overlaps(frames[first], frames[second]), (first, second)


def test_off_card_is_one_toggle_row():
    state = PeopleState()
    state.set_flags(True, False)
    frames = people_layout(580, 100, state)
    assert frames["card"] == (0, 128, 580, PEOPLE_TOGGLE_H) and PEOPLE_TOGGLE_H == 60
    assert "list" not in frames and "footer" not in frames and "message" not in frames
    assert frames["bottom"] == 128 + 60 + 12
    with_message = people_layout(580, 100, state, message_height=34)
    assert with_message["message"] == (0, 128 + 60 + 6, 580, 34)
    assert with_message["bottom"] == 128 + 60 + 6 + 34 + 12


@pytest.mark.parametrize("width", [580, 1200])
def test_people_layout_has_no_overlaps(width):
    state = _ready(_vp(A, "Dana"), _vp(B, "Ari", model_id="old"))
    frames = people_layout(width, 0, state)
    assert frames["list"] == (0, 60, width, 88)
    assert frames["footer"] == (0, 148, width, 44)
    assert frames["card"][3] == 60 + 88 + 44
    _assert_no_overlaps({key: frames[key] for key in ("toggle_title", "switch")})
    _assert_no_overlaps({key: frames[key] for key in ("toggle_detail", "switch")})
    for pill in (0, 140):
        _assert_no_overlaps(person_row_frames(width, 1, pill_width=pill))
    _assert_no_overlaps(person_rename_frames(width, 1))
    assert person_rename_frames(width, 1) == speaker_row_frames(width, 1, "rename")
    _assert_no_overlaps(people_footer_frames(width, state))
    state.begin_enroll(LOCAL)
    enroll = people_footer_frames(width, state)
    assert set(enroll) == {"field", "record", "cancel"}
    _assert_no_overlaps(enroll)
    _op, _args, _token = state.start_recording("Me")
    recording = people_footer_frames(width, state)
    assert set(recording) == {"status", "stop", "cancel"}
    _assert_no_overlaps(recording)
    status = people_status_frames(width)
    _assert_no_overlaps({"error": status["error"], "retry": status["retry"]})


def test_list_height_for_loading_error_and_empty_is_one_row():
    state = PeopleState()
    state.set_flags(True, True)
    assert people_layout(600, 0, state)["list"][3] == 44  # loading
    state.loaded(state.seq, _payload(), None)
    assert people_layout(600, 0, state)["list"][3] == 44  # empty
