import json
import subprocess
import sys
from pathlib import Path

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


def test_import_is_isolated():
    code = (
        "import sys, localflow.diarization; "
        "bad = [m for m in ('localflow.meetings', 'localflow.server', 'sherpa_onnx') "
        "if m in sys.modules]; print(bad); sys.exit(1 if bad else 0)"
    )
    proc = subprocess.run(
        [sys.executable, "-c", code], check=False, capture_output=True, text=True
    )
    assert proc.returncode == 0, proc.stdout + proc.stderr
