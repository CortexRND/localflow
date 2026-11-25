import ast
import json
import logging
import os
import stat
from pathlib import Path
from types import SimpleNamespace

import pytest

from localflow import meetings
from localflow.diarization import store as store_module
from localflow.diarization.store import (
    CHANNEL_LABELS,
    MeetingSpeakerStore,
    build_record,
    frontmatter_speakers,
    merge_speakers,
    public_speakers,
    rename_speaker,
    speaker_lines,
)

MID = "0123456789abcdef0123456789abcdef"
SECRET_TEXT = "never-store-this-text"


def _seg(start, source, sid=None, end=None, text=SECRET_TEXT):
    return SimpleNamespace(offset_seconds=start, end_seconds=end, source=source,
                           speaker_id=sid, text=text)


def _speakers():
    return [
        {"id": "S1", "label": "Speaker 1", "name": None, "source": "others", "seconds": 6.0},
        {"id": "S2", "label": "Speaker 2", "name": None, "source": "others", "seconds": 7.0},
        {"id": "S3", "label": "Speaker 3", "name": None, "source": "others", "seconds": 7.04},
    ]


def _segments():
    return [
        _seg(0, "me"),
        _seg(0, "others", "S1", 4),
        _seg(4, "others", "S2", 8),
        _seg(8, "others", "S3", 12),
        _seg(12, "others", "S1", 14),
        _seg(13, "others", None, None),
        _seg(17, "others", "S2", 20),
    ]


def _record(**over):
    kwargs = {"meeting_id": MID, "log_path": "/v/t.md", "notes_path": "/v/n.md",
              "started_at": "2026-11-14T10:00:00", "embedder": "mock", "status": "ok",
              "speakers": _speakers(), "segments": _segments()}
    kwargs.update(over)
    return build_record(**kwargs)


@pytest.fixture
def store(tmp_path):
    return MeetingSpeakerStore(tmp_path / "meetings")


@pytest.fixture(autouse=True)
def _fresh_warnings(monkeypatch):
    monkeypatch.setattr(store_module, "_warned", set())


def test_round_trip(store):
    record = _record()
    path = store.save(record)
    assert path == store.root / f"{MID}.json"
    assert store.load(MID) == record
    raw = path.read_bytes()
    assert raw.endswith(b"\n")
    assert json.loads(raw.decode("utf-8")) == record


def test_build_record_schema_has_no_text_and_null_centroids(store):
    record = _record()
    assert list(record) == ["schema_version", "meeting_id", "log_path", "notes_path",
                            "started_at", "embedder", "status", "speakers", "segments"]
    assert record["schema_version"] == 1
    for sp in record["speakers"]:
        assert set(sp) == {"id", "source", "label", "name", "voiceprint_id", "seconds",
                           "centroid"}
        assert sp["centroid"] is None
    for seg in record["segments"]:
        assert set(seg) == {"start", "end", "source", "speaker_id"}
    assert record["segments"][5] == {"start": 13.0, "end": None, "source": "others",
                                     "speaker_id": None}
    assert SECRET_TEXT.encode() not in store.save(record).read_bytes()


def test_public_speakers_hide_private_fields():
    record = _record()
    record["speakers"][0].update(centroid=[0.1, 0.2], voiceprint_id="vp1", extra="x")
    view = public_speakers(record)
    assert all(set(sp) == {"id", "label", "name", "source", "seconds"} for sp in view)
    assert view[0] == {"id": "S1", "label": "Speaker 1", "name": None, "source": "others",
                       "seconds": 6.0}


def test_higher_schema_version_is_absent_and_logged_once(store, caplog):
    record = _record()
    record["schema_version"] = 2
    store.save(record)
    before = store.path(MID).read_bytes()
    with caplog.at_level(logging.WARNING, logger="localflow.diarization"):
        assert store.load(MID) is None
        assert store.load(MID) is None
    assert len([r for r in caplog.records if r.levelno == logging.WARNING]) == 1
    assert store.path(MID).read_bytes() == before


def _mutate(fn):
    record = _record()
    fn(record)
    return json.dumps(record).encode()


@pytest.mark.parametrize("raw", [
    b"{not json",
    b"\xff\xfe",
    b"[]",
    b"{}",
    _mutate(lambda r: r.update(schema_version="1")),
    _mutate(lambda r: r.update(schema_version=True)),
    _mutate(lambda r: r.update(schema_version=0)),
    _mutate(lambda r: r.update(meeting_id="f" * 32)),
    _mutate(lambda r: r.update(speakers={})),
    _mutate(lambda r: r["speakers"][0].pop("label")),
    _mutate(lambda r: r["speakers"][1].update(id="S1")),
    _mutate(lambda r: r["speakers"][0].update(seconds="3")),
    _mutate(lambda r: r["segments"][0].update(start="0")),
    _mutate(lambda r: r.update(log_path=7)),
    json.dumps(_record()).replace("6.0", "NaN").encode(),
])
def test_corrupt_or_misshapen_files_are_absent(store, raw):
    store.root.mkdir(parents=True)
    store.path(MID).write_bytes(raw)
    assert store.load(MID) is None


@pytest.mark.parametrize("bad", [
    "", "abc", MID.upper(), MID + "0", "../" + MID, "..\\x", "/etc/passwd",
    MID + ".json", None, 42, "../secret",
])
def test_invalid_ids_are_absent_without_touching_disk(store, tmp_path, bad):
    (tmp_path / "secret.json").write_text(json.dumps(_record()), encoding="utf-8")
    assert store.path(bad) is None
    assert store.load(bad) is None
    with pytest.raises(ValueError):
        store.save({**_record(), "meeting_id": bad})
    assert not store.root.exists()


@pytest.mark.skipif(os.name == "nt", reason="POSIX modes")
def test_file_and_directory_modes(store):
    store.root.mkdir(parents=True)
    os.chmod(store.root, 0o755)
    old = os.umask(0)
    try:
        path = store.save(_record())
    finally:
        os.umask(old)
    assert stat.S_IMODE(os.stat(store.root).st_mode) == 0o700
    assert stat.S_IMODE(os.stat(path).st_mode) == 0o600


def test_atomic_write_leaves_no_temp_files(store):
    store.save(_record())
    store.save(_record(status="ok"))
    assert sorted(p.name for p in store.root.iterdir()) == [f"{MID}.json"]


def test_failed_replace_keeps_the_old_file_and_cleans_up(store, monkeypatch):
    store.save(_record())
    before = store.path(MID).read_bytes()

    def boom(src, dst):
        raise OSError("disk full")

    monkeypatch.setattr(store_module.os, "replace", boom)
    with pytest.raises(OSError):
        store.save(_record(embedder=None))
    assert store.path(MID).read_bytes() == before
    assert not list(store.root.glob("*.tmp"))


def test_default_root_follows_home_at_call_time(monkeypatch, tmp_path):
    store = MeetingSpeakerStore()
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    assert store.root == tmp_path / ".localflow" / "meetings"


@pytest.mark.parametrize("seconds", [0, 0.9, 59.99, 60, 61.5, 3599, 6005.2])
def test_stamp_matches_segment_stamp(seconds):
    assert store_module.stamp(seconds) == meetings.Segment(seconds, "").stamp
    assert CHANNEL_LABELS == meetings._SOURCE_LABELS


def test_frontmatter_speakers_first_appearance():
    segs = [_seg(0, "me"), _seg(1, "others", "S1"), _seg(2, "others", "S2"),
            _seg(3, "others"), _seg(4, "others", "S1")]
    record = _record(segments=segs)
    assert frontmatter_speakers(record) == ["Me", "Speaker 1", "Speaker 2"]
    labels = {"S1": "Speaker 1", "S2": "Speaker 2"}
    equivalent = [meetings.Segment(s.offset_seconds, "x", s.source, None, s.speaker_id,
                                   labels.get(s.speaker_id)) for s in segs]
    assert meetings._diarized_speakers(equivalent) == frontmatter_speakers(record)


def test_rename_changes_one_identity_only():
    record = _record()
    snapshot = json.dumps(record)
    renamed = rename_speaker(record, "S2", "Dana")
    assert json.dumps(record) == snapshot
    s2 = next(sp for sp in renamed["speakers"] if sp["id"] == "S2")
    assert s2["label"] == s2["name"] == "Dana"
    assert [sp for sp in renamed["speakers"] if sp["id"] != "S2"] == [
        sp for sp in record["speakers"] if sp["id"] != "S2"]
    assert renamed["segments"] == record["segments"]
    assert frontmatter_speakers(renamed) == ["Me", "Speaker 1", "Dana", "Speaker 3"]
    with pytest.raises(KeyError):
        rename_speaker(record, "S9", "x")


def test_merge_moves_segments_sums_seconds_and_drops_identity():
    record = _record()
    snapshot = json.dumps(record)
    merged = merge_speakers(record, "S3", "S1")
    assert json.dumps(record) == snapshot
    assert all(seg["speaker_id"] != "S3" for seg in merged["segments"])
    assert [sp["id"] for sp in merged["speakers"]] == ["S1", "S2"]
    assert merged["speakers"][0]["seconds"] == 13.0
    assert frontmatter_speakers(record) == ["Me", "Speaker 1", "Speaker 2", "Speaker 3"]
    assert frontmatter_speakers(merged) == ["Me", "Speaker 1", "Speaker 2"]
    # `into` may move earlier when `from` appeared first.
    assert frontmatter_speakers(merge_speakers(record, "S1", "S3")) == [
        "Me", "Speaker 3", "Speaker 2"]
    with pytest.raises(ValueError):
        merge_speakers(record, "S1", "S1")
    with pytest.raises(KeyError):
        merge_speakers(record, "S1", "S9")
    with pytest.raises(KeyError):
        merge_speakers(record, "S9", "S1")


def test_speaker_lines_flag_each_same_label_occurrence():
    segs = [_seg(0, "me"), _seg(0.5, "others", "S1"), _seg(5, "others", "S1"),
            _seg(5.5, "others", "S2"), _seg(9, "others", "S2")]
    record = _record(segments=segs)
    assert speaker_lines(record, "S1") == {"00:00": [True], "00:05": [True]}
    both = rename_speaker(rename_speaker(record, "S1", "Dana"), "S2", "Dana")
    assert speaker_lines(both, "S1") == {"00:00": [True], "00:05": [True, False]}
    assert speaker_lines(both, "S2") == {"00:05": [False, True], "00:09": [True]}
    me = rename_speaker(record, "S1", "Me")
    assert speaker_lines(me, "S1") == {"00:00": [False, True], "00:05": [True]}
    assert speaker_lines(record, "S9") == {}


def test_update_is_a_locked_read_modify_write(store):
    store.save(_record())
    out = store.update(MID, lambda r: rename_speaker(r, "S1", "Ann"))
    assert out["speakers"][0]["label"] == "Ann"
    assert store.load(MID)["speakers"][0]["label"] == "Ann"
    calls = []
    assert store.update("f" * 32, lambda r: calls.append(r) or r) is None
    assert calls == []


def test_store_module_stays_dependency_free():
    tree = ast.parse(Path(store_module.__file__).read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if isinstance(node, ast.ImportFrom):
            assert node.level == 0
            names = [node.module or ""]
        elif isinstance(node, ast.Import):
            names = [a.name for a in node.names]
        else:
            continue
        for name in names:
            assert not name.startswith("numpy")
            assert name not in ("localflow.meetings", "localflow.server")


# ------------------------------------------------- voice data (slice B9) ---

def _with_voice(speakers, index, centroid, vid):
    speakers[index] = {**speakers[index], "centroid": centroid, "voiceprint_id": vid}
    return speakers


def test_build_record_carries_supplied_voice_data_only():
    record = _record(speakers=_with_voice(_speakers(), 0, [0.6, 0.8], "a" * 32))
    first = record["speakers"][0]
    assert (first["centroid"], first["voiceprint_id"]) == ([0.6, 0.8], "a" * 32)
    assert all(sp["centroid"] is None and sp["voiceprint_id"] is None
               for sp in record["speakers"][1:])


@pytest.mark.parametrize("field,value", [
    ("centroid", "x"), ("centroid", []), ("centroid", [1.0, "a"]), ("centroid", [True]),
    ("centroid", [float("inf")]), ("centroid", [0.0] * 4097), ("voiceprint_id", 5),
])
def test_bad_voice_data_makes_the_sidecar_unusable(store, field, value):
    record = _record()
    record["speakers"][0][field] = value
    store.root.mkdir(parents=True)
    (store.root / f"{MID}.json").write_text(json.dumps(record))
    assert store.load(MID) is None


def test_strip_voiceprints_nulls_centroids_and_matching_links(store):
    speakers = _with_voice(_speakers(), 0, [1.0, 0.0], "a" * 32)
    speakers = _with_voice(speakers, 1, [0.0, 1.0], "b" * 32)
    store.save(_record(speakers=speakers))
    assert store.strip_voiceprints("a" * 32) == (2, 0)
    stored = store.load(MID)
    assert [(sp["centroid"], sp["voiceprint_id"]) for sp in stored["speakers"]] == [
        (None, None), (None, "b" * 32), (None, None)]
    others = _record()
    assert all(stored[k] == others[k] for k in others if k != "speakers")
    assert store.strip_voiceprints() == (1, 0)
    assert store.load(MID)["speakers"][1]["voiceprint_id"] is None
    before = (store.root / f"{MID}.json").read_bytes()
    assert store.strip_voiceprints() == (0, 0)  # nothing left: the file is not rewritten
    assert (store.root / f"{MID}.json").read_bytes() == before


def test_strip_voiceprints_without_a_store_dir_is_a_noop(store):
    assert store.strip_voiceprints() == (0, 0)
    assert not store.root.exists()


def test_rename_and_merge_keep_b8_semantics_for_voice_data():
    record = _record(speakers=_with_voice(_speakers(), 1, [0.0, 1.0], "b" * 32))
    assert rename_speaker(record, "S2", "Dana")["speakers"][1]["voiceprint_id"] == "b" * 32
    merged = merge_speakers(record, "S2", "S1")
    assert [sp["id"] for sp in merged["speakers"]] == ["S1", "S3"]
    assert all(sp["centroid"] is None for sp in merged["speakers"])
