"""Voiceprint registry (slice B9): files, modes, matching, forget, export."""

import io
import json
import logging
import os
import pickle
import subprocess
import sys
import zipfile

import numpy as np
import pytest

from localflow.diarization import registry as reg
from localflow.diarization import sherpa
from localflow.diarization import store as store_module
from localflow.diarization.models import DIAR_MODELS
from localflow.diarization.registry import (
    AmbiguousName,
    BadVector,
    EnrollmentInterrupted,
    ErasureIncomplete,
    FileSpeakerRegistry,
    InvalidName,
    RegistryError,
    RegistryUnreadable,
    UnknownVoiceprint,
    UnsupportedModel,
    write_bundle,
)
from localflow.diarization.store import MeetingSpeakerStore

POSIX = os.name != "nt"
MID1 = "1" * 32
MID2 = "2" * 32
MID3 = "3" * 32


def onehot(i, dim=64):
    v = np.zeros(dim, dtype=np.float32)
    v[i] = 1.0
    return v


@pytest.fixture
def rooms(tmp_path):
    meetings = MeetingSpeakerStore(tmp_path / "meetings")
    return FileSpeakerRegistry(tmp_path / "speakers", meetings=meetings), meetings


@pytest.fixture
def registry(rooms):
    return rooms[0]


def _sidecar(mid, speakers, **extra):
    return {"schema_version": 1, "meeting_id": mid, "log_path": "/v/log.md",
            "notes_path": "/v/notes.md", "started_at": "2026-11-14T10:00:00",
            "embedder": "mock", "status": "ok", "speakers": speakers,
            "segments": [{"start": 0.0, "end": 1.0, "source": "others", "speaker_id": "S1"}],
            **extra}


def _sp(sid, centroid=None, vid=None, label=None):
    return {"id": sid, "source": "others", "label": label or f"Speaker {sid[1:]}",
            "name": None, "voiceprint_id": vid, "seconds": 3.0, "centroid": centroid}


# ------------------------------------------------------------------ basics ---

def test_enroll_list_match_round_trip(registry):
    vp = registry.enroll("Dana Lee", [onehot(0), onehot(0) + onehot(1) * 0.1], "mock")
    assert reg.valid_voiceprint_id(vp.id)
    assert (vp.name, vp.model_id, vp.samples) == ("Dana Lee", "mock", 2)
    assert vp.created.endswith("Z")
    assert registry.list() == [vp]
    stored = np.load(registry.root / f"{vp.id}.npy", allow_pickle=False)
    expected = (onehot(0) + (onehot(0) + onehot(1) * 0.1) / np.linalg.norm(
        onehot(0) + onehot(1) * 0.1))
    np.testing.assert_allclose(stored, expected / np.linalg.norm(expected), rtol=1e-6)
    assert stored.dtype == np.float32
    hit = registry.match(onehot(0), "mock")
    assert hit is not None and hit[0] == vp and hit[1] > 0.99
    assert registry.match(onehot(5), "mock") is None
    raw = json.loads((registry.root / "registry.json").read_text())
    assert raw == {"schema_version": 1, "voiceprints": [reg.public(vp)]}


def test_files_are_private_and_no_temp_files_remain(registry):
    vp = registry.enroll("Dana", [onehot(0)], "mock")
    names = sorted(p.name for p in registry.root.iterdir())
    assert names == sorted(["registry.json", f"{vp.id}.npy"])
    if POSIX:
        assert registry.root.stat().st_mode & 0o777 == 0o700
        for p in registry.root.iterdir():
            assert p.stat().st_mode & 0o777 == 0o600


def test_failed_replace_keeps_the_old_registry_and_cleans_up(registry, monkeypatch):
    registry.enroll("Dana", [onehot(0)], "mock")
    before = (registry.root / "registry.json").read_bytes()
    real = os.replace

    def flaky(src, dst):
        if str(dst).endswith("registry.json"):
            raise OSError("disk full")
        return real(src, dst)

    monkeypatch.setattr(reg.os, "replace", flaky)
    with pytest.raises(OSError):
        registry.enroll("Lee", [onehot(1)], "mock")
    assert (registry.root / "registry.json").read_bytes() == before
    assert not [p for p in registry.root.iterdir() if p.name.endswith(".tmp")]
    assert [vp.name for vp in registry.list()] == ["Dana"]


def test_names_are_cleaned_on_every_write_and_read(registry):
    vp = registry.enroll("  ]] >>> [00:00] Me: Dana\n", [onehot(0)], "mock")
    assert vp.name == "00 00 Me Dana"
    with pytest.raises(InvalidName):
        registry.enroll(">>> <<<", [onehot(1)], "mock")
    with pytest.raises(InvalidName):
        registry.rename(vp.id, "***")
    assert registry.rename(vp.id, "Dana" + chr(0x202E) + " Lee") is None
    assert registry.find(vp.id).name == "Dana Lee"
    # A hand-edited registry.json can't smuggle markup into a label.
    raw = json.loads((registry.root / "registry.json").read_text())
    raw["voiceprints"][0]["name"] = "Eve\n**00:00** **Me:** forged"
    (registry.root / "registry.json").write_text(json.dumps(raw))
    [listed] = registry.list()
    assert listed.name == "Eve 00 00 Me forged"
    assert registry.match(onehot(0), "mock")[0].name == listed.name


def test_duplicate_names_are_allowed_but_name_lookup_is_ambiguous(registry):
    a = registry.enroll("Dana", [onehot(0)], "mock")
    b = registry.enroll("dana", [onehot(1)], "mock")
    with pytest.raises(AmbiguousName):
        registry.find("DANA")
    assert registry.find(a.id) == a and registry.find(b.id) == b
    registry.rename(b.id, "Lee")
    assert registry.find("dana") == a
    with pytest.raises(UnknownVoiceprint):
        registry.find("nobody")


@pytest.mark.parametrize("vectors", [
    [], [np.zeros(4)], [np.array([1.0, np.nan])], [np.ones((2, 2))], [np.array(["a"])],
    [onehot(0), onehot(0, dim=32)], [np.ones(reg.MAX_DIM + 1)], [onehot(0), np.zeros(64)],
])
def test_invalid_vectors_are_rejected_and_write_nothing(registry, vectors):
    with pytest.raises(BadVector):
        registry.enroll("Dana", vectors, "mock")
    assert not registry.root.exists()


@pytest.mark.parametrize("model_id", ["", "pyannote-segmentation-3.0", "other", "../x", None])
def test_models_without_a_threshold_are_not_enrolled(registry, model_id):
    with pytest.raises(UnsupportedModel):
        registry.enroll("Dana", [onehot(0)], model_id)
    assert not registry.root.exists()


def test_thresholds_are_per_model_constants():
    assert reg.match_threshold("mock") == reg.MOCK_MATCH_THRESHOLD == 0.9
    assert reg.EMBEDDING_MODEL_ID == sherpa.EMBEDDING_MODEL_ID == DIAR_MODELS[1].id
    assert DIAR_MODELS[1].match_threshold == 0.6 and DIAR_MODELS[0].match_threshold is None
    assert reg.match_threshold(sherpa.EMBEDDING_MODEL_ID) == 0.6
    assert reg.match_threshold(DIAR_MODELS[0].id) is None
    assert reg.match_threshold("unknown") is None and reg.match_threshold(None) is None
    assert reg.embedder_id_for_backend("mock") == "mock"
    assert reg.embedder_id_for_backend("sherpa-onnx") == sherpa.EMBEDDING_MODEL_ID


def test_thresholds_come_from_the_catalog_at_call_time(monkeypatch):
    import dataclasses

    from localflow.diarization import models as dm

    stricter = dataclasses.replace(dm.DIAR_MODELS[1], match_threshold=0.75)
    monkeypatch.setattr(dm, "DIAR_MODELS", (dm.DIAR_MODELS[0], stricter))
    assert reg.match_threshold(stricter.id) == 0.75
    monkeypatch.setattr(dm, "DIAR_MODELS", (dm.DIAR_MODELS[0],))
    assert reg.match_threshold(stricter.id) is None
    assert reg.match_threshold("mock") == 0.9  # local constant, not in the catalog


def test_rename_keeps_the_protocol_signature():
    import inspect

    from localflow.diarization.types import SpeakerRegistry

    assert inspect.signature(FileSpeakerRegistry.rename) == inspect.signature(
        SpeakerRegistry.rename)


def test_other_models_and_dimensions_are_inert(registry):
    registry.enroll("Dana", [onehot(0)], "mock")
    assert registry.match(onehot(0), sherpa.EMBEDDING_MODEL_ID) is None  # never cross-model
    assert registry.match(onehot(0, dim=32), "mock") is None
    assert registry.match(onehot(0), "unknown") is None
    assert registry.match(np.zeros(64), "mock") is None
    below = onehot(0) * 0.8 + onehot(1) * 0.6  # cosine 0.8 < 0.9
    assert registry.match(below, "mock") is None


def test_best_match_wins_and_ties_go_to_the_earlier_enrollment(registry):
    first = registry.enroll("First", [onehot(0)], "mock")
    registry.enroll("Second", [onehot(0)], "mock")
    near = registry.enroll("Near", [onehot(0) * 0.95 + onehot(2) * 0.31], "mock")
    assert registry.match(onehot(0), "mock")[0] == first
    assert registry.match(onehot(0) * 0.95 + onehot(2) * 0.31, "mock")[0] == near


def test_bad_or_pickled_vector_files_make_that_voiceprint_inert(registry):
    vp = registry.enroll("Dana", [onehot(0)], "mock")
    path = registry.root / f"{vp.id}.npy"
    buf = io.BytesIO()
    np.save(buf, np.array([{"x": 1}], dtype=object), allow_pickle=True)
    for payload in (buf.getvalue(), pickle.dumps([1.0] * 64), b"garbage",
                    _npy(np.ones((2, 32))), _npy(np.zeros(64))):
        path.write_bytes(payload)
        assert registry.match(onehot(0), "mock") is None
        assert registry.list() == [vp]


def _npy(arr):
    buf = io.BytesIO()
    np.save(buf, arr, allow_pickle=False)
    return buf.getvalue()


@pytest.mark.parametrize("content", [b"not json", b"[]", b'{"schema_version": 2, "voiceprints": []}',
                                     b'{"schema_version": 1}'])
def test_unreadable_or_newer_registry_reads_empty_and_refuses_writes(registry, caplog,
                                                                     content):
    registry.root.mkdir(parents=True)
    (registry.root / "registry.json").write_bytes(content)
    reg._warned.clear()
    with caplog.at_level(logging.WARNING, "localflow.diarization"):
        assert registry.list() == [] and registry.list() == []
    assert len([r for r in caplog.records if "speaker registry" in r.getMessage()]) == 1
    with pytest.raises(RegistryUnreadable):
        registry.enroll("Dana", [onehot(0)], "mock")
    with pytest.raises(RegistryUnreadable):
        registry.forget("a" * 32)
    assert (registry.root / "registry.json").read_bytes() == content


@pytest.mark.parametrize("bad", ["../../etc/passwd", "A" * 32, "a" * 31, "", "S1"])
def test_invalid_ids_never_touch_disk(registry, bad):
    with pytest.raises(UnknownVoiceprint):
        registry.rename(bad, "Dana")
    with pytest.raises(UnknownVoiceprint):
        registry.forget(bad)
    assert not registry.root.exists()


def test_logs_never_carry_names_ids_or_vectors(registry, caplog, rooms):
    with caplog.at_level(logging.DEBUG):
        vp = registry.enroll("Zanzibar Q", [onehot(3)], "mock")
        registry.match(onehot(3), "mock")
        registry.rename(vp.id, "Quixote Z")
        registry.export_zip()
        registry.forget(vp.id)
    text = "\n".join(r.getMessage() for r in caplog.records)
    assert caplog.records
    assert "Zanzibar" not in text and "Quixote" not in text and vp.id not in text
    assert "1.0" not in text and "array" not in text


# ------------------------------------------------------------------ forget ---

def _seed(meetings, vid):
    meetings.save(_sidecar(MID1, [_sp("S1", [1.0, 0.0], vid, "Dana"), _sp("S2", [0.0, 1.0])]))
    meetings.save(_sidecar(MID2, [_sp("S1", [0.5, 0.5], "f" * 32), _sp("S2")]))


def test_forget_one_erases_every_centroid_and_only_its_links(rooms):
    registry, meetings = rooms
    vp = registry.enroll("Dana", [onehot(0)], "mock")
    keep = registry.enroll("Lee", [onehot(1)], "mock")
    _seed(meetings, vp.id)
    epoch = reg.forget_epoch()
    assert registry.forget(vp.id) == 1
    assert reg.forget_epoch() == epoch + 1
    assert registry.list() == [keep]
    assert not (registry.root / f"{vp.id}.npy").exists()
    assert (registry.root / f"{keep.id}.npy").exists()
    one, two = meetings.load(MID1), meetings.load(MID2)
    assert all(sp["centroid"] is None for r in (one, two) for sp in r["speakers"])
    assert one["speakers"][0]["voiceprint_id"] is None
    assert one["speakers"][0]["label"] == "Dana"  # displayed names stay
    assert two["speakers"][0]["voiceprint_id"] == "f" * 32  # another voice's link stays
    with pytest.raises(UnknownVoiceprint):
        registry.forget(vp.id)


def test_forget_all_erases_everything_it_owns_and_nothing_else(rooms):
    registry, meetings = rooms
    vp = registry.enroll("Dana", [onehot(0)], "mock")
    registry.enroll("Lee", [onehot(1)], "mock")
    _seed(meetings, vp.id)
    orphan = registry.root / ("e" * 32 + ".npy")
    orphan.write_bytes(_npy(onehot(4)))
    stale_tmp = registry.root / f".{'e' * 32}.npy.abc123.tmp"
    stale_tmp.write_bytes(b"x")
    foreign = registry.root / "notes.txt"
    foreign.write_text("mine")
    (registry.root / "sub").mkdir()
    assert registry.forget() == 2
    assert sorted(p.name for p in registry.root.iterdir()) == ["notes.txt", "sub"]
    for mid in (MID1, MID2):
        assert all(sp["centroid"] is None and sp["voiceprint_id"] is None
                   for sp in meetings.load(mid)["speakers"])
    assert registry.list() == []
    assert registry.forget() == 0


def test_forget_scrubs_newer_schema_without_migrating_and_keeps_other_keys(rooms):
    registry, meetings = rooms
    meetings.root.mkdir(parents=True)
    newer = {"schema_version": 7, "meeting_id": MID3, "future": {"keep": [1, 2]},
             "speakers": [{"id": "S1", "centroid": [0.1, 0.2], "voiceprint_id": "a" * 32,
                           "extra": "kept"}, {"id": "S2", "label": "x"}]}
    (meetings.root / f"{MID3}.json").write_text(json.dumps(newer))
    assert registry.forget() == 0
    after = json.loads((meetings.root / f"{MID3}.json").read_text())
    assert after == {**newer, "speakers": [
        {"id": "S1", "centroid": None, "voiceprint_id": None, "extra": "kept"},
        {"id": "S2", "label": "x"}]}
    assert meetings.load(MID3) is None  # still unknown to this version, not migrated


@pytest.mark.parametrize("content", [b"{not json", b'{"schema_version": 1, "speakers": 3}',
                                     b"[1, 2]", b'{"speakers": ["S1"]}', b"\xff\xfe"])
@pytest.mark.parametrize("single", [True, False])
def test_unreadable_sidecars_are_reported_not_counted_as_erased(rooms, content, single):
    registry, meetings = rooms
    vp = registry.enroll("Dana", [onehot(0)], "mock")
    _seed(meetings, vp.id)
    (meetings.root / f"{MID3}.json").write_bytes(content)
    (meetings.root / "not-a-sidecar.json").write_bytes(b"{")  # not ours: ignored
    with pytest.raises(ErasureIncomplete) as info:
        registry.forget(vp.id if single else None)
    assert (info.value.forgotten, info.value.failed) == (1, 1)
    assert "1 meeting" in str(info.value) and "Dana" not in str(info.value)
    # Everything erasable was erased anyway.
    assert registry.list() == []
    assert all(sp["centroid"] is None for sp in meetings.load(MID1)["speakers"])
    assert (meetings.root / f"{MID3}.json").read_bytes() == content


def test_unwritable_sidecar_is_reported(rooms, monkeypatch):
    registry, meetings = rooms
    vp = registry.enroll("Dana", [onehot(0)], "mock")
    _seed(meetings, vp.id)
    before = (meetings.root / f"{MID1}.json").read_bytes()

    def read_only(*args, **kwargs):
        raise PermissionError("read-only")

    monkeypatch.setattr(store_module.tempfile, "mkstemp", read_only)
    with pytest.raises(ErasureIncomplete) as info:
        registry.forget()
    assert info.value.failed == 2
    assert (meetings.root / f"{MID1}.json").read_bytes() == before
    assert registry.list() == []


def test_forget_during_enrollment_does_not_recreate_a_voiceprint(registry):
    epoch = reg.forget_epoch()
    registry.enroll("Dana", [onehot(0)], "mock")
    registry.forget()
    with pytest.raises(EnrollmentInterrupted):
        registry.enroll("Dana", [onehot(0)], "mock", expected_epoch=epoch)
    assert registry.list() == []
    assert not list(registry.root.glob("*.npy"))
    assert registry.enroll("Dana", [onehot(0)], "mock",
                           expected_epoch=reg.forget_epoch()).name == "Dana"


# ------------------------------------------------------------------ export ---

def test_export_writes_registry_and_vectors_privately(registry, tmp_path):
    a = registry.enroll("Dana", [onehot(0)], "mock")
    b = registry.enroll("Lee", [onehot(1)], "mock")
    with zipfile.ZipFile(io.BytesIO(registry.export_zip())) as zf:
        assert sorted(zf.namelist()) == sorted(["registry.json", f"{a.id}.npy", f"{b.id}.npy"])
    out = registry.export(tmp_path / "export")
    assert out == tmp_path / "export"
    meta = json.loads((out / "registry.json").read_text())
    assert meta == {"schema_version": 1, "voiceprints": [reg.public(a), reg.public(b)]}
    np.testing.assert_array_equal(np.load(out / f"{a.id}.npy"), onehot(0))
    if POSIX:
        assert out.stat().st_mode & 0o777 == 0o700
        assert all(p.stat().st_mode & 0o777 == 0o600 for p in out.iterdir())
    with pytest.raises(RegistryError):
        registry.export(out)  # not empty
    (tmp_path / "file").write_text("x")
    with pytest.raises(RegistryError):
        registry.export(tmp_path / "file")


def _zip(members):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as zf:
        for name, data in members.items():
            zf.writestr(name, data)
    return buf.getvalue()


def _meta(*ids):
    return json.dumps({"schema_version": 1, "voiceprints": [
        {"id": i, "name": "Dana", "model_id": "mock", "created": "2026-11-14T10:00:00Z",
         "samples": 1} for i in ids]})


@pytest.mark.parametrize("members", [
    {"registry.json": _meta(), "../x.npy": _npy(onehot(0))},
    {"registry.json": _meta(), "sub/" + "a" * 32 + ".npy": _npy(onehot(0))},
    {"registry.json": _meta("a" * 32), "A" * 32 + ".npy": _npy(onehot(0))},
    {"registry.json": _meta("a" * 32), "a" * 32 + ".npy": b"\0" * (reg.MAX_VECTOR_FILE_BYTES + 1)},
    {"registry.json": _meta("a" * 32), "a" * 32 + ".npy": pickle.dumps([1.0])},
    {"registry.json": _meta("a" * 32)},
    {"registry.json": "nope", "a" * 32 + ".npy": _npy(onehot(0))},
    {"a" * 32 + ".npy": _npy(onehot(0))},
])
def test_bad_bundles_are_refused_and_write_nothing(tmp_path, members):
    with pytest.raises(RegistryError):
        write_bundle(_zip(members), tmp_path / "out")
    assert not (tmp_path / "out").exists()
    assert not (tmp_path / "x.npy").exists()


def test_not_a_zip_is_refused(tmp_path):
    with pytest.raises(RegistryError):
        write_bundle(b"PK nope", tmp_path / "out")
    assert not (tmp_path / "out").exists()


# ---------------------------------------------------------------- isolation ---

def test_registry_import_is_isolated():
    code = (
        "import sys\n"
        "import localflow.diarization.registry\n"
        "bad = {'localflow.meetings', 'localflow.server', 'sounddevice', 'sherpa_onnx'}\n"
        "assert not bad & set(sys.modules), bad & set(sys.modules)\n"
    )
    proc = subprocess.run([sys.executable, "-W", "error", "-c", code], check=False,
                          capture_output=True, text=True)
    assert proc.returncode == 0, proc.stdout + proc.stderr


def test_constructing_a_registry_touches_no_files(tmp_path):
    FileSpeakerRegistry(tmp_path / "speakers")
    assert not (tmp_path / "speakers").exists()
