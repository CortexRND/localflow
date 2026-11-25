import inspect
import json
import subprocess
import sys
from pathlib import Path
from unittest import mock

import pytest

from localflow.diarization import (
    GAP_TOLERANCE_S,
    SpeakerTurn,
    TimedText,
    align,
    assign_speakers,
    merge_runs,
)

GOLDEN = json.loads(
    (Path(__file__).parent / "fixtures" / "diarization_align.json").read_text(
        encoding="utf-8"
    )
)
ASSIGN = GOLDEN["assign_cases"]
MERGE = GOLDEN["merge_cases"]


def _timed(d):
    return TimedText(d["start"], d["end"], d["text"])


def test_fixture_header_and_counts():
    assert GOLDEN["version"] == 1
    assert GOLDEN["gap_tolerance_s"] == GAP_TOLERANCE_S
    assert GOLDEN["tie_eps"] == align._EPS
    assert len(ASSIGN) >= 15 and len(MERGE) >= 5
    names = [c["name"] for c in ASSIGN + MERGE]
    assert len(names) == len(set(names))


@pytest.mark.parametrize("case", ASSIGN, ids=[c["name"] for c in ASSIGN])
def test_assign_golden(case):
    timed = [_timed(t) for t in case["timed"]]
    turns = [
        SpeakerTurn(t["start"], t["end"], t["local_speaker"]) for t in case["turns"]
    ]
    result = assign_speakers(timed, turns)
    assert [t for t, _ in result] == timed
    assert [s for _, s in result] == case["expected"]


@pytest.mark.parametrize("case", MERGE, ids=[c["name"] for c in MERGE])
def test_merge_golden(case):
    assigned = [(_timed(a), a["speaker"]) for a in case["assigned"]]
    runs = merge_runs(assigned, chunk_offset=case["chunk_offset"])
    got = [
        {"start": r.start, "end": r.end, "text": r.text, "speaker": r.speaker}
        for r in runs
    ]
    assert len(got) == len(case["expected"])
    for g, e in zip(got, case["expected"]):
        assert g["start"] == pytest.approx(e["start"])
        assert g["end"] == pytest.approx(e["end"])
        assert (g["text"], g["speaker"]) == (e["text"], e["speaker"])
        assert (g["speaker"] is None) == (e["speaker"] is None)


def _run(code):
    proc = subprocess.run(
        [sys.executable, "-W", "error", "-c", code],
        check=False,
        capture_output=True,
        text=True,
    )
    assert proc.returncode == 0, proc.stdout + proc.stderr


_LAZY_MODS = ("localflow.diarization.align", "localflow.diarization.mock")


@pytest.mark.parametrize(
    ("stmt", "forbidden"),
    [
        (
            "import localflow.diarization",
            ("localflow.meetings", "localflow.server", "sherpa_onnx", *_LAZY_MODS),
        ),
        ("import localflow.diarization.types", _LAZY_MODS),
        ("import localflow.stt", _LAZY_MODS),
    ],
    ids=["package", "types", "stt"],
)
def test_import_is_isolated(stmt, forbidden):
    check = f"bad = [m for m in {forbidden!r} if m in sys.modules]"
    _run(f"import sys\n{stmt}\n{check}\nassert not bad, bad")


_PKG_ATTR = "cluster = localflow.diarization.cluster"


@pytest.mark.parametrize(
    "steps",
    [
        [
            "import localflow.diarization.cluster",
            "from localflow.diarization import cluster",
        ],
        [
            "from localflow.diarization import cluster",
            "import localflow.diarization.cluster",
            _PKG_ATTR,
        ],
        [
            "from localflow.diarization.cluster import cluster",
            "import localflow.diarization",
            _PKG_ATTR,
        ],
    ],
    ids=["submodule_first", "name_first", "from_submodule"],
)
def test_cluster_attribute_is_function(steps):
    checks = [
        "assert inspect.isfunction(cluster), cluster",
        "assert cluster([], threshold=0.5) == []",
    ]
    _run("\n".join(["import inspect", *steps, *checks]))


def test_all_names_resolve():
    import localflow.diarization as pkg

    for name in pkg.__all__:
        assert getattr(pkg, name) is not None
    assert set(pkg.__all__) <= set(dir(pkg))
    assert inspect.isfunction(pkg.cluster)


def test_cluster_can_be_patched():
    import localflow.diarization as pkg

    original = pkg.cluster

    def fake(*args, **kwargs):
        return []

    with mock.patch.object(pkg, "cluster", fake):
        from localflow.diarization import cluster

        assert pkg.cluster is fake and cluster is fake
    assert pkg.cluster is original
