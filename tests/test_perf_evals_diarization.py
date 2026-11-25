"""Tests for the perf-eval ``diarization`` suite: DER scorer, corpus mixes, bench and suite.

No network, no sherpa-onnx, no Ollama: the bench runs with the mock backend on
the synthetic tone mix, and the suite's sherpa path is driven by a fake bench.
"""

from __future__ import annotations

import ast
import copy
import itertools
import json
import math
import random
import subprocess
import sys
import wave
from pathlib import Path

import pytest

from evals.perf import (
    compare_results,
    der,
    load_policy,
    make_result,
    make_run,
    registry,
    validate_result,
)
from evals.perf.benches import py_diarization
from evals.perf.corpus import download, mixes
from evals.perf.registry import RunContext
from evals.perf.schema import make_case, make_metric
from evals.perf.suites import BUILTIN_SUITES, diarization

REPO_ROOT = Path(__file__).resolve().parents[1]
PERF_DIR = REPO_ROOT / "evals" / "perf"
STDLIB_OK = set(sys.stdlib_module_names) | {"__future__"}
SYNTHETIC_REFERENCE = [
    {"start": 0.0, "end": 4.0, "speaker": "A"}, {"start": 5.0, "end": 9.0, "speaker": "B"},
    {"start": 10.0, "end": 14.0, "speaker": "C"}, {"start": 15.0, "end": 19.0, "speaker": "A"},
    {"start": 20.0, "end": 24.0, "speaker": "B"}, {"start": 25.0, "end": 33.0, "speaker": "C"},
    {"start": 34.0, "end": 38.0, "speaker": "A"},
]


@pytest.fixture(autouse=True)
def _clean_env(monkeypatch):
    monkeypatch.delenv("LF_PERF_PYTHON", raising=False)


def _der(ref, hyp, **kw):
    return der.compute_der(ref, hyp, **{"collar": 0.0, **kw})


# --------------------------------------------------------------------------
# DER
# --------------------------------------------------------------------------

AB = [(0, 10, "A"), (10, 20, "B")]


def test_der_exact():
    r = _der(AB, AB)
    assert r["der"] == 0.0 and r["speaker_count_exact"] is True and r["speaker_count_error"] == 0
    assert r["scored_s"] == pytest.approx(20.0) and r["mapping"] == {"A": "A", "B": "B"}
    assert set(r) == {"der", "missed_speech", "false_alarm", "speaker_confusion", "scored_s", "missed_s",
                      "false_alarm_s", "confusion_s", "ref_speakers", "hyp_speakers", "speaker_count_error",
                      "speaker_count_exact", "mapping", "collar", "frame_s"}


def test_der_permutation():
    r = _der(AB, [(10, 20, "x"), (0, 10, "y")])
    assert r["der"] == 0.0 and r["mapping"] == {"x": "B", "y": "A"}


def test_der_missed():
    r = _der(AB, [(0, 10, "A"), (10, 15, "B")])
    assert r["missed_speech"] == pytest.approx(0.25) and r["der"] == pytest.approx(0.25)
    assert r["missed_s"] == pytest.approx(5.0)


def test_der_false_alarm():
    r = _der([(0, 10, "A")], [(0, 10, "A"), (12, 14, "A")])
    assert r["false_alarm"] == pytest.approx(0.2) and r["der"] == pytest.approx(0.2)


def test_der_confusion():
    r = _der(AB, [(0, 15, "x"), (15, 20, "y")])
    assert r["speaker_confusion"] == pytest.approx(0.25) and r["der"] == pytest.approx(0.25)
    assert r["mapping"] == {"x": "A", "y": "B"}


def test_der_collar():
    hyp = [(0, 10.2, "x"), (10.2, 20, "y")]
    assert der.compute_der(AB, hyp, collar=0.25)["der"] == 0.0
    assert der.compute_der(AB, hyp, collar=0.0)["der"] == pytest.approx(0.01)
    assert der.compute_der(AB, hyp)["collar"] == der.DEFAULT_COLLAR_S == 0.25


def test_der_overlap():
    ref = [(0, 10, "A"), (5, 15, "B")]
    r = _der(ref, [(0, 10, "x"), (5, 15, "y")])
    assert r["der"] == 0.0 and r["scored_s"] == pytest.approx(20.0)
    r = _der(ref, [(0, 15, "x")])
    assert r["missed_speech"] == pytest.approx(0.25)
    assert r["speaker_confusion"] == pytest.approx(0.25)
    assert r["der"] == pytest.approx(0.5)


def test_der_speaker_counts():
    r = _der(AB, [(0, 5, "x"), (5, 10, "y"), (10, 20, "z")])
    assert r["ref_speakers"] == 2 and r["hyp_speakers"] == 3
    assert r["speaker_count_error"] == 1 and r["speaker_count_exact"] is False
    assert len(r["mapping"]) == 2  # one hyp speaker stays unmapped


def test_der_empty_rules():
    r = _der([], [])
    assert r["der"] == 0.0 and r["missed_speech"] == 0.0 and r["false_alarm"] == 0.0
    r = _der([(0, 5, "A")], [])
    assert r["der"] == 1.0 and r["missed_speech"] == 1.0
    r = _der([], [(0, 5, "x")])
    assert r["der"] == 1.0 and r["false_alarm"] == 1.0 and r["missed_speech"] == 0.0


@pytest.mark.parametrize(("collar", "frame_s"), [(-0.1, 0.01), (0.0, 0.0), (0.0, -1.0)])
def test_der_rejects_bad_parameters(collar, frame_s):
    with pytest.raises(ValueError):
        der.compute_der(AB, AB, collar=collar, frame_s=frame_s)


def test_hungarian_known_and_edge_cases():
    assert der.hungarian([[4, 1, 3], [2, 0, 5], [3, 2, 2]]) == [(0, 1), (1, 0), (2, 2)]
    assert der.hungarian([]) == []
    assert der.hungarian([[1.0, 2.0]]) == [(0, 0)]
    assert der.hungarian([[5.0], [1.0], [3.0]]) == [(1, 0)]
    with pytest.raises(ValueError):
        der.hungarian([[1, 2], [3]])


def _brute(cost):
    rows, cols = len(cost), len(cost[0])
    if rows <= cols:
        return min(sum(cost[r][c] for r, c in enumerate(p)) for p in itertools.permutations(range(cols), rows))
    return min(sum(cost[r][c] for c, r in enumerate(p)) for p in itertools.permutations(range(rows), cols))


def test_hungarian_matches_brute_force():
    rng = random.Random(20261125)
    shapes = set()
    for _ in range(50):
        rows, cols = rng.randint(1, 5), rng.randint(1, 5)
        shapes.add((rows > cols, rows < cols))
        cost = [[rng.randint(-20, 20) / 2 for _ in range(cols)] for _ in range(rows)]
        pairs = der.hungarian(cost)
        assert len(pairs) == min(rows, cols) and pairs == sorted(pairs)
        assert len({r for r, _ in pairs}) == len({c for _, c in pairs}) == len(pairs)
        assert sum(cost[r][c] for r, c in pairs) == pytest.approx(_brute(cost))
    assert {(True, False), (False, True), (False, False)} <= shapes


RTTM = """\
;; a comment
# another comment

SPKR-INFO mix 1 <NA> <NA> <NA> unknown A <NA> <NA>
SPEAKER mix 1 0.000 10.000 <NA> <NA> A <NA> <NA>
SPEAKER other 1 3.0 1.0 <NA> <NA> Z <NA> <NA>
SPEAKER mix 1 10.000 10.000 <NA> <NA> B
"""


def test_parse_rttm_and_filters():
    turns = der.parse_rttm(RTTM)
    assert [(t.start, t.end, t.speaker) for t in turns] == [(0.0, 10.0, "A"), (3.0, 4.0, "Z"), (10.0, 20.0, "B")]
    assert [t.speaker for t in der.parse_rttm(RTTM, file_id="mix")] == ["A", "B"]


@pytest.mark.parametrize("line", [
    "SPEAKER mix 1 0.0 1.0 <NA> <NA>",
    "SPEAKER mix 1 zero 1.0 <NA> <NA> A",
    "SPEAKER mix 1 0.0 -1.0 <NA> <NA> A",
    "SPEAKER mix 1 -2 1.0 <NA> <NA> A",
])
def test_parse_rttm_bad_line_names_line_number(line):
    with pytest.raises(ValueError, match="line 3"):
        der.parse_rttm(f"; header\nSPEAKER mix 1 0 1 <NA> <NA> A\n{line}\n")


def test_rttm_round_trip_and_files(tmp_path):
    turns = [(0.1234, 1.5, "A"), (2.0, 3.0004, "B")]
    text = der.to_rttm(turns, "m1")
    assert text.endswith("\n") and text.splitlines()[0] == "SPEAKER m1 1 0.123 1.377 <NA> <NA> A <NA> <NA>"
    back = der.parse_rttm(text)
    for got, (s, e, spk) in zip(back, turns):
        assert got.speaker == spk
        assert got.start == pytest.approx(s, abs=1e-3) and got.end == pytest.approx(e, abs=2e-3)
    (tmp_path / "r.rttm").write_text(text)
    assert der.load_rttm(tmp_path / "r.rttm", file_id="m1") == back
    r = der.compute_der_rttm(der.to_rttm(AB), der.to_rttm([(0, 10, "x"), (10, 20, "y")]), collar=0.0)
    assert r["der"] == 0.0 and r["mapping"] == {"x": "A", "y": "B"}


def test_as_turns_input_forms():
    turns = der.as_turns([der.Turn(5, 6, "c"), {"start": 2, "end": 3, "speaker": 7}, (0, 1, "a"), [1, 1, "empty"],
                          (4, 3, "reversed")])
    assert turns == [der.Turn(0.0, 1.0, "a"), der.Turn(2.0, 3.0, "7"), der.Turn(5.0, 6.0, "c")]


@pytest.mark.parametrize("item", [
    (-1, 2, "a"), (0, math.inf, "a"), (math.nan, 1, "a"), (True, 2, "a"), (None, 2, "a"), ("0", 2, "a"),
    {"start": 0, "end": 1}, "SPEAKER", (0, 1),
])
def test_as_turns_rejections(item):
    with pytest.raises(ValueError):
        der.as_turns([item])


def test_aggregate_pools_seconds_and_averages_counts():
    a = _der(AB, [(0, 10, "x"), (10, 15, "y")])              # 5 s missed of 20
    b = _der([(0, 10, "A")], [(0, 10, "x"), (10, 20, "y")])  # 10 s FA of 10, one extra speaker
    agg = der.aggregate([a, b])
    assert agg["n"] == 2
    assert agg["scored_s"] == pytest.approx(30.0)
    assert agg["der"] == pytest.approx(15.0 / 30.0)  # pooled, not mean(0.25, 1.0)
    assert agg["missed_speech"] == pytest.approx(5 / 30) and agg["false_alarm"] == pytest.approx(10 / 30)
    assert agg["speaker_count_error"] == pytest.approx(0.5) and agg["speaker_count_exact"] == pytest.approx(0.5)
    assert der.aggregate([_der([], [])])["der"] == 0.0
    with pytest.raises(ValueError):
        der.aggregate([])


@pytest.mark.parametrize("path", [PERF_DIR / "der.py", PERF_DIR / "corpus" / "mixes.py",
                                  PERF_DIR / "suites" / "diarization.py"], ids=lambda p: p.name)
def test_modules_import_only_stdlib(path):
    tree = ast.parse(path.read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            mods = [a.name for a in node.names]
        elif isinstance(node, ast.ImportFrom) and not node.level:
            mods = [node.module or ""]
        else:
            continue
        for mod in mods:
            top = mod.split(".")[0]
            assert top != "localflow", f"{path} imports localflow"
            assert top in STDLIB_OK, f"{path} imports non-stdlib {mod}"


# --------------------------------------------------------------------------
# Corpus
# --------------------------------------------------------------------------


@pytest.fixture(scope="module")
def manifest() -> dict:
    return mixes.load_diarization_manifest()


def test_manifest_loads_with_four_speakers(manifest):
    clips = manifest["clips"]
    speakers = {c["speaker"] for c in clips}
    assert len(speakers) >= 4
    genders = {c["speaker"]: c["gender"] for c in clips}
    pairs = list(itertools.combinations(sorted(speakers), 2))
    assert any(genders[a] == genders[b] for a, b in pairs) and any(genders[a] != genders[b] for a, b in pairs)
    for c in clips:
        assert c["archive_member"].split("/")[-3] == c["speaker"]
        assert c["id"].startswith(c["speaker"] + "-")
    by_id = {c["id"]: c for c in clips}
    sizes = {len({by_id[cid]["speaker"] for cid in m["clips"]}) for m in manifest["mixes"]}
    assert 3 in sizes and 4 in sizes
    assert any(m["smoke"] for m in manifest["mixes"])
    smoke_clips = {cid for m in manifest["mixes"] if m["smoke"] for cid in m["clips"]}
    assert {c["id"] for c in clips if c["smoke"]} == smoke_clips
    used = {cid for m in manifest["mixes"] for cid in m["clips"]}
    assert used == set(by_id)  # every pinned clip is used by a mix


def test_manifest_shares_digests_with_stt_corpus(manifest):
    stt = download.load_stt_manifest()
    assert len(stt["clips"]) == 11
    stt_by_id = {c["id"]: c for c in stt["clips"]}
    shared = [c for c in manifest["clips"] if c["id"] in stt_by_id]
    assert shared and all(c["speaker"] == "6930" for c in shared)
    for c in shared:
        s = stt_by_id[c["id"]]
        assert (c["sha256"], c["archive_member"], c["duration_s"], c["reference"]) == (
            s["sha256"], s["archive_member"], s["duration_s"], s["reference"])
        assert c["archive_url"] == s["archive_url"]


def test_long_mixes_cross_the_chunk_boundary(manifest):
    by_id = {c["id"]: c for c in manifest["clips"]}
    for m in manifest["mixes"]:
        total = sum(by_id[c]["duration_s"] for c in m["clips"]) + m["gap_s"] * (len(m["clips"]) - 1)
        if not m["smoke"]:
            assert total > 60, m["id"]


def _bad(mutate):
    def apply(data):
        mutate(data)
        return data
    return apply


def _mix(data, mid):
    return next(m for m in data["mixes"] if m["id"] == mid)


BAD_MANIFESTS = {
    "speaker-missing": (_bad(lambda d: d["clips"][0].pop("speaker")), "6930-75918-0000"),
    "speaker-empty": (_bad(lambda d: d["clips"][0].update(speaker=" ")), "6930-75918-0000"),
    "no-mixes": (_bad(lambda d: d.pop("mixes")), "mixes"),
    "empty-mixes": (_bad(lambda d: d.update(mixes=[])), "mixes"),
    "bad-id": (_bad(lambda d: _mix(d, "mix3").update(id="-mix")), "-mix"),
    "dup-id": (_bad(lambda d: _mix(d, "mix3").update(id="mix4")), "mix4"),
    "one-clip": (_bad(lambda d: _mix(d, "mix3").update(clips=["1089-134686-0000"])), "mix3"),
    "unknown-clip": (_bad(lambda d: _mix(d, "mix3")["clips"].append("nope-1")), "nope-1"),
    "repeat-clip": (_bad(lambda d: _mix(d, "mix3")["clips"].append("1089-134686-0000")), "mix3"),
    "same-speaker": (_bad(lambda d: _mix(d, "mix3").update(clips=["1089-134686-0000", "1089-134686-0002"])),
                     "mix3"),
    "gap-bool": (_bad(lambda d: _mix(d, "mix3").update(gap_s=True)), "mix3"),
    "gap-string": (_bad(lambda d: _mix(d, "mix3").update(gap_s="0.5")), "mix3"),
    "gap-negative": (_bad(lambda d: _mix(d, "mix3").update(gap_s=-0.1)), "mix3"),
    "gap-large": (_bad(lambda d: _mix(d, "mix3").update(gap_s=5.5)), "mix3"),
    "smoke-not-bool": (_bad(lambda d: _mix(d, "mix3").update(smoke=1)), "mix3"),
    "smoke-clip-not-smoke": (_bad(lambda d: _mix(d, "mix3").update(smoke=True)), "1089-134686-0000"),
}


@pytest.mark.parametrize("name", sorted(BAD_MANIFESTS))
def test_manifest_loader_rejects(name, tmp_path):
    data = json.loads(mixes.DEFAULT_DIARIZATION_MANIFEST_PATH.read_text(encoding="utf-8"))
    mutate, needle = BAD_MANIFESTS[name]
    path = tmp_path / "bad.json"
    path.write_text(json.dumps(mutate(data)))
    with pytest.raises(ValueError) as info:
        mixes.load_diarization_manifest(path)
    assert str(path) in str(info.value) and needle in str(info.value)


def _tone(seconds: float, rate: int = 16000, amp: float = 8000.0) -> list[int]:
    return [round(amp * math.sin(2 * math.pi * 440 * i / rate)) for i in range(round(seconds * rate))]


def _silence(seconds: float, rate: int = 16000) -> list[int]:
    return [0] * round(seconds * rate)


def _write_wav(path: Path, samples: list[int], rate: int = 16000, channels: int = 1) -> Path:
    import array

    data = array.array("h", samples)
    if sys.byteorder == "big":
        data.byteswap()
    with wave.open(str(path), "wb") as w:
        w.setnchannels(channels)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(data.tobytes())
    return path


def test_speech_regions_merges_short_pauses():
    import array

    pcm = array.array("h", _silence(0.5) + _tone(2.0) + _silence(0.2) + _tone(1.0) + _silence(1.0))
    (region,) = mixes.speech_regions(pcm, 16000)
    assert region[0] == pytest.approx(0.5, abs=0.02) and region[1] == pytest.approx(3.7, abs=0.02)
    pcm = array.array("h", _silence(0.5) + _tone(2.0) + _silence(0.8) + _tone(1.0) + _silence(1.0))
    first, second = mixes.speech_regions(pcm, 16000)
    assert first == pytest.approx((0.5, 2.5), abs=0.02) and second == pytest.approx((3.3, 4.3), abs=0.02)
    assert mixes.speech_regions(array.array("h", _silence(1.5)), 16000) == [(0.0, 1.5)]
    assert mixes.speech_regions(array.array("h"), 16000) == [(0.0, 0.0)]


def test_read_pcm16_rejects_non_mono(tmp_path):
    path = _write_wav(tmp_path / "st.wav", [0, 0, 1, 1], channels=2)
    with pytest.raises(ValueError):
        mixes.read_pcm16(path)
    samples, rate = mixes.read_pcm16(_write_wav(tmp_path / "m.wav", [1, -2, 3]))
    assert list(samples) == [1, -2, 3] and rate == 16000


def test_build_mix_offsets_and_gap(tmp_path):
    a = _write_wav(tmp_path / "a.wav", _silence(0.2) + _tone(1.0) + _silence(0.2))
    b = _write_wav(tmp_path / "b.wav", _tone(1.0))
    clips = {"a": {"path": str(a), "speaker": "SA"}, "b": {"path": str(b), "speaker": "SB"}}
    out = mixes.build_mix({"id": "m", "gap_s": 0.5, "clips": ["a", "b"]}, clips, tmp_path / "m.wav")
    assert out["id"] == "m" and out["duration_s"] == pytest.approx(2.9)
    assert out["speakers"] == ["SA", "SB"]
    assert set(out["vad"]) == {"frame_s", "rel_db", "abs_floor", "min_gap_s", "min_speech_s"}
    (ra, rb) = out["reference"]
    assert ra["speaker"] == "SA" and (ra["start"], ra["end"]) == pytest.approx((0.2, 1.2), abs=0.02)
    assert rb["speaker"] == "SB" and (rb["start"], rb["end"]) == pytest.approx((1.9, 2.9), abs=0.02)
    samples, rate = mixes.read_pcm16(Path(out["path"]))
    assert rate == 16000 and len(samples) == round(2.9 * 16000)
    assert not any(samples[round(1.4 * 16000):round(1.9 * 16000)])  # the gap is silent
    slow = _write_wav(tmp_path / "s.wav", _tone(1.0, rate=8000), rate=8000)
    clips["b"]["path"] = str(slow)
    with pytest.raises(ValueError, match="rate"):
        mixes.build_mix({"id": "m", "gap_s": 0.5, "clips": ["a", "b"]}, clips, tmp_path / "m2.wav")


def test_write_synthetic_mix(tmp_path):
    out = mixes.write_synthetic_mix(tmp_path / "s.wav")
    assert out["id"] == diarization.SYNTHETIC_MIX_ID == "synthetic-tones"
    assert out["duration_s"] == 38.0 and out["speakers"] == ["A", "B", "C"]
    assert out["reference"] == SYNTHETIC_REFERENCE
    samples, rate = mixes.read_pcm16(Path(out["path"]))
    assert len(samples) == 38 * rate
    assert samples[0] == round(0.1 * 32768) and samples[5 * rate] == round(0.101 * 32768)
    assert samples[4 * rate + 10] == 0 and samples[26 * rate] == round(0.102 * 32768)


# --------------------------------------------------------------------------
# Bench (in-process, real mock backend)
# --------------------------------------------------------------------------


@pytest.fixture
def synthetic(tmp_path) -> tuple[dict, Path]:
    mix = mixes.write_synthetic_mix(tmp_path / "syn.wav")
    manifest = tmp_path / "m.json"
    manifest.write_text(json.dumps({"mixes": [{"id": mix["id"], "path": "syn.wav"}]}))
    return mix, manifest


def _bench(manifest: Path, tmp_path: Path, *extra: str, loader=None) -> tuple[int, dict | None]:
    out = tmp_path / "out.json"
    out.unlink(missing_ok=True)
    rc = py_diarization.main(["--manifest", str(manifest), "--backend", "mock", "--runs", "2", "--warmup", "1",
                              "--json", str(out), *extra], loader=loader)
    return rc, json.loads(out.read_text()) if out.exists() else None


@pytest.mark.parametrize("pipeline", ["whole", "chunked"])
def test_bench_mock_scores_perfectly(synthetic, tmp_path, capsys, pipeline):
    sherpa_absent = "sherpa_onnx" not in sys.modules
    mix, manifest = synthetic
    rc, report = _bench(manifest, tmp_path, "--pipeline", pipeline)
    assert rc == 0
    assert report["backend"] == "mock" and report["pipeline"] == pipeline
    assert report["segmentation_model"] == report["embedding_model"] == "mock"
    assert report["runs"] == 2 and report["warmup"] == 1 and report["max_speakers"] == 10
    (m,) = report["mixes"]
    assert m["id"] == "synthetic-tones" and m["audio_seconds"] == pytest.approx(38.0)
    assert len(m["latencies_ms"]) == 2 and m["first_ms"] == m["latencies_ms"][0]
    assert m["unlabelled_turns"] == 0
    r = der.compute_der(mix["reference"], m["turns"])
    assert r["der"] == 0.0 and r["hyp_speakers"] == 3
    r0 = der.compute_der(mix["reference"], m["turns"], collar=0.0)
    assert r0["der"] == 0.0
    for key in ("rss_before_bytes", "rss_loaded_bytes", "rss_peak_bytes", "rss_delta_bytes"):
        assert isinstance(report[key], int) and report[key] >= 0
    assert report["rss_delta_bytes"] == max(0, report["rss_peak_bytes"] - report["rss_before_bytes"])
    captured = capsys.readouterr()
    assert json.loads(captured.out) == report and "| synthetic-tones |" in captured.err
    if sherpa_absent:
        assert "sherpa_onnx" not in sys.modules


def test_bench_chunked_links_turn_across_chunk_boundary(synthetic, tmp_path):
    _, manifest = synthetic
    rc, report = _bench(manifest, tmp_path, "--pipeline", "chunked", "--chunk-seconds", "30")
    assert rc == 0 and report["chunk_seconds"] == 30.0
    turns = report["mixes"][0]["turns"]
    straddle = [t for t in turns if t["start"] >= 25.0 and t["end"] <= 33.0]
    assert [(t["start"], t["end"]) for t in straddle] == [(25.0, 30.0), (30.0, 33.0)]
    assert len({t["speaker"] for t in straddle}) == 1
    first_c = next(t for t in turns if t["start"] == 10.0)
    assert straddle[0]["speaker"] == first_c["speaker"]


def test_bench_threshold_flags_are_reported(synthetic, tmp_path):
    _, manifest = synthetic
    rc, report = _bench(manifest, tmp_path, "--cluster-threshold", "0.4", "--threshold", "0.7",
                        "--max-speakers", "3", "--table", str(tmp_path / "t.md"))
    assert rc == 0
    assert report["cluster_threshold"] == 0.4 and report["threshold"] == 0.7 and report["max_speakers"] == 3
    assert (tmp_path / "t.md").read_text().startswith("mock / chunked")
    rc, report = _bench(manifest, tmp_path, "--max-speakers", "2")
    assert rc == 0 and len({t["speaker"] for t in report["mixes"][0]["turns"]}) == 2


def test_bench_loader_unavailable_exits_3(synthetic, tmp_path, capsys):
    _, manifest = synthetic

    def loader(_args):
        raise py_diarization.Unavailable("no runtime here")

    rc, report = _bench(manifest, tmp_path, loader=loader)
    assert rc == py_diarization.EXIT_UNAVAILABLE == 3 and report is None
    assert "unavailable: no runtime here" in capsys.readouterr().err


def test_bench_probe(capsys):
    assert py_diarization.main(["--probe", "--backend", "mock"]) == 0
    assert json.loads(capsys.readouterr().out) == {"available": True, "backend": "mock"}


def test_bench_default_loader_maps_missing_runtime_and_models(monkeypatch, capsys):
    from localflow.diarization import models

    monkeypatch.setattr(models, "runtime_installed", lambda: False)
    assert py_diarization.main(["--probe", "--backend", "sherpa-onnx"]) == 3
    assert "localflow[diarization]" in capsys.readouterr().err
    monkeypatch.setattr(models, "runtime_installed", lambda: True)
    monkeypatch.setattr(models, "models_ready", lambda cache=None: (False, models.NOT_DOWNLOADED))
    assert py_diarization.main(["--probe", "--backend", "sherpa-onnx"]) == 3
    err = capsys.readouterr().err
    assert "unavailable:" in err and "not downloaded" in err


@pytest.mark.parametrize("content", ['{"mixes": []}', "not json", '{"mixes": [{"id": "x"}]}',
                                     '{"mixes": [{"id": "x", "path": "missing.wav"}]}'])
def test_bench_bad_manifest_exits_1(tmp_path, capsys, content):
    manifest = tmp_path / "bad.json"
    manifest.write_text(content)
    rc, _ = _bench(manifest, tmp_path)
    assert rc == 1 and "error:" in capsys.readouterr().err


def test_bench_without_manifest_is_usage_error(capsys):
    assert py_diarization.main(["--backend", "mock"]) == 2
    with pytest.raises(SystemExit) as info:
        py_diarization.main(["--runs", "0"])
    assert info.value.code == 2


# --------------------------------------------------------------------------
# Suite
# --------------------------------------------------------------------------


def _ctx(runtime: str = "python", tier: str = "smoke", **options) -> RunContext:
    return RunContext(tier=tier, runtime=runtime, repo_root=REPO_ROOT, options=options)


def _valid(cases: list[dict], runtime: str = "python") -> dict:
    run = make_run(tier="smoke", runtime=runtime, sha="abc", ref="t", os_name="linux", arch="x86_64",
                   runner={"label": "t", "cpu": "c", "cores": 1, "ram_gb": 1})
    result = make_result(run, cases)
    assert validate_result(result) == []
    assert all(c["suite"] == "diarization" for c in cases)
    return result


def _by_case(cases: list[dict]) -> dict[str, dict]:
    return {c["case"]: c for c in cases}


def test_suite_is_registered():
    assert "diarization" in BUILTIN_SUITES and "diarization" in registry.PLANNED_SUITES
    assert registry.get_suite("diarization") is diarization.run_diarization


def _plan(tier, runtime="python"):
    return [(e["backend"], e["pipeline"], e["mixes"], e["runs"], e["warmup"])
            for e in diarization.plan_diarization_matrix(tier, runtime)]


def test_plan_matrix():
    mock = [("mock", "whole", "synthetic", 1, 0), ("mock", "chunked", "synthetic", 1, 0)]
    assert _plan("smoke") == mock + [("sherpa-onnx", "chunked", "smoke", 3, 1)]
    full = mock + [("sherpa-onnx", "chunked", "all", 5, 1), ("sherpa-onnx", "whole", "all", 5, 1)]
    assert _plan("nightly") == _plan("deep") == full
    for tier in ("smoke", "nightly"):
        assert all(e["not_exercised"] is None for e in diarization.plan_diarization_matrix(tier, "python"))
        rust = diarization.plan_diarization_matrix(tier, "rust")
        assert _plan(tier, "rust") == _plan(tier)
        assert all(e["not_exercised"] == diarization.RUST_REASON for e in rust)
    with pytest.raises(ValueError):
        diarization.plan_diarization_matrix("weekly", "python")
    with pytest.raises(ValueError):
        diarization.plan_diarization_matrix("smoke", "go")
    assert diarization.case_name({"backend": "mock", "pipeline": "whole"}, "x") == "mock-whole/x"


def test_rust_is_not_exercised(monkeypatch):
    def boom(*a, **k):
        raise AssertionError("no subprocess for rust")

    monkeypatch.setattr(diarization.subprocess, "run", boom)
    cases = diarization.run_diarization(_ctx("rust", "nightly"))
    _valid(cases, "rust")
    assert len(cases) == 2 * 2 + 2 * (4 + 1)
    assert all(c["status"] == "not_exercised" and c["error"] == diarization.RUST_REASON for c in cases)


class FakeBench:
    """Stands in for ``subprocess.run`` of ``py_diarization.py`` (probe and manifest runs)."""

    def __init__(self, refs: dict[str, list], *, probe_rc: int = 0, probe_stderr: str = "",
                 returncode: int = 0, stderr: str = "", drop: frozenset = frozenset(),
                 malformed: frozenset = frozenset(), timeout: bool = False):
        self.refs, self.probe_rc, self.probe_stderr = refs, probe_rc, probe_stderr
        self.returncode, self.stderr, self.drop, self.malformed = returncode, stderr, drop, malformed
        self.timeout = timeout
        self.commands: list[list[str]] = []

    def __call__(self, cmd, **kwargs):
        self.commands.append(cmd)
        if "--probe" in cmd:
            return subprocess.CompletedProcess(cmd, self.probe_rc, '{"available": true}', self.probe_stderr)
        if self.timeout:
            raise subprocess.TimeoutExpired(cmd, kwargs.get("timeout"), stderr="slow")
        if self.returncode != 0:
            return subprocess.CompletedProcess(cmd, self.returncode, "", self.stderr)
        arg = lambda flag: cmd[cmd.index(flag) + 1]
        listed = json.loads(Path(arg("--manifest")).read_text())["mixes"]
        backend, runs = arg("--backend"), int(arg("--runs"))
        lat = [200.0 + i for i in range(runs)]
        results = []
        for m in listed:
            if m["id"] in self.drop:
                continue
            names: dict[str, str] = {}
            turns = [{"start": t["start"], "end": t["end"],
                      "speaker": names.setdefault(t["speaker"], f"S{len(names) + 1}")} for t in self.refs[m["id"]]]
            if m["id"] in self.malformed:
                turns = [{"start": -1.0, "end": 2.0, "speaker": "S1"}]
            results.append({"id": m["id"], "audio_seconds": 40.0, "latencies_ms": lat, "first_ms": lat[0],
                            "median_ms": lat[len(lat) // 2], "p95_ms": lat[-1],
                            "real_time_factor": lat[len(lat) // 2] / 40000.0, "turns": turns,
                            "unlabelled_turns": 0})
        report = {"mode": "manifest", "backend": backend, "pipeline": arg("--pipeline"), "runs": runs,
                  "warmup": int(arg("--warmup")), "os": "linux", "arch": "x86_64", "chunk_seconds": 30.0,
                  "threshold": 0.5, "cluster_threshold": 0.5, "max_speakers": 10, "num_threads": 1,
                  "segmentation_model": "mock" if backend == "mock" else "pyannote-segmentation-3.0",
                  "embedding_model": "mock" if backend == "mock" else "campplus",
                  "load_ms": 150.0, "rss_before_bytes": 100_000_000, "rss_loaded_bytes": 150_000_000,
                  "rss_peak_bytes": 180_000_000, "rss_delta_bytes": 80_000_000, "mixes": results}
        Path(arg("--json")).write_text(json.dumps(report))
        return subprocess.CompletedProcess(cmd, 0, json.dumps(report), "")


@pytest.fixture
def suite_env(monkeypatch, tmp_path, manifest):
    calls: dict[str, list] = {"ensure": [], "build": []}
    refs: dict[str, list] = {diarization.SYNTHETIC_MIX_ID: SYNTHETIC_REFERENCE}

    def fake_ensure(man, cache_dir, **kw):
        calls["ensure"].append(kw.get("clip_ids"))
        by_id = {c["id"]: c for c in man["clips"]}
        return [{"id": cid, "path": str(tmp_path / f"{cid}.wav"), "reference": by_id[cid]["reference"],
                 "duration_s": by_id[cid]["duration_s"], "smoke": by_id[cid]["smoke"]} for cid in kw["clip_ids"]]

    def fake_build(mix, clips, out_wav):
        calls["build"].append(mix["id"])
        ref = [{"start": 6.0 * i, "end": 6.0 * i + 5.0, "speaker": clips[cid]["speaker"]}
               for i, cid in enumerate(mix["clips"])]
        refs[mix["id"]] = ref
        return {"id": mix["id"], "path": str(out_wav), "duration_s": 6.0 * len(mix["clips"]),
                "speakers": sorted({r["speaker"] for r in ref}), "reference": ref, "vad": mixes.vad_params()}

    monkeypatch.setattr(diarization, "ensure_stt_clips", fake_ensure)
    monkeypatch.setattr(diarization, "build_mix", fake_build)

    def run(bench_kw: dict | None = None, tier: str = "smoke", **options):
        bench = FakeBench(refs, **(bench_kw or {}))
        monkeypatch.setattr(diarization.subprocess, "run", bench)
        cases = diarization.run_diarization(_ctx("python", tier, cache_dir=str(tmp_path / "cache"), **options))
        return cases, bench

    run.calls = calls
    return run


MOCK_QUALITY = {"der", "missed_speech", "false_alarm", "speaker_confusion", "speaker_count_error",
                "speaker_count_exact"}


def test_python_smoke_with_fake_bench(suite_env):
    cases, bench = suite_env()
    _valid(cases)
    by = _by_case(cases)
    assert list(by) == ["mock-whole/synthetic-tones", "mock-whole/all", "mock-chunked/synthetic-tones",
                        "mock-chunked/all", "sherpa-onnx-chunked/mix2-mf", "sherpa-onnx-chunked/all"]
    assert all(c["status"] == "ok" for c in cases), [c for c in cases if c["status"] != "ok"]
    mock = by["mock-chunked/synthetic-tones"]
    assert mock["dims"] == {"backend": "mock", "pipeline": "chunked", "mix": "synthetic-tones", "kind": "mix"}
    assert set(mock["metrics"]) == MOCK_QUALITY
    assert mock["metrics"]["der"] == {"unit": "ratio", "better": "lower", "value": 0.0}
    assert mock["metrics"]["speaker_count_exact"]["value"] == 1.0
    assert type(mock["metrics"]["speaker_count_exact"]["value"]) is float
    assert mock["metrics"]["speaker_count_error"]["unit"] == "count"
    ev = mock["evidence"]
    assert ev["median_ms"] == 200.0 and ev["ref_speakers"] == ev["hyp_speakers"] == 3
    assert ev["reference_rttm"].startswith("SPEAKER synthetic-tones 1 0.000 4.000 <NA> <NA> A")
    assert ev["hypothesis_rttm"].count("\n") == 7 and ev["collar"] == 0.25 and ev["unlabelled_turns"] == 0
    assert set(by["mock-whole/all"]["metrics"]) == MOCK_QUALITY
    assert by["mock-whole/all"]["dims"]["kind"] == "aggregate" and by["mock-whole/all"]["dims"]["mix"] == "all"
    sherpa = by["sherpa-onnx-chunked/mix2-mf"]
    assert set(sherpa["metrics"]) == MOCK_QUALITY | {"latency_ms", "first_ms", "rtf"}
    assert sherpa["metrics"]["latency_ms"]["samples"] == [200.0, 201.0, 202.0]
    agg = by["sherpa-onnx-chunked/all"]
    assert set(agg["metrics"]) == MOCK_QUALITY | {"rtf", "load_ms", "rss_before", "rss_loaded", "rss_peak",
                                                  "rss_delta_bytes"}
    assert agg["metrics"]["rss_delta_bytes"] == {"unit": "bytes", "better": "lower", "value": 80_000_000}
    assert agg["metrics"]["rtf"]["value"] == pytest.approx(201.0 / 40000.0)
    assert agg["evidence"]["mixes"] == ["mix2-mf"]
    assert agg["evidence"]["segmentation_model"] == "pyannote-segmentation-3.0"
    probes = [c for c in bench.commands if "--probe" in c]
    assert len(probes) == 1 and probes[0][probes[0].index("--backend") + 1] == "sherpa-onnx"
    mix2 = next(m for m in diarization.load_diarization_manifest()["mixes"] if m["id"] == "mix2-mf")
    assert suite_env.calls["ensure"] == [mix2["clips"]] and suite_env.calls["build"] == ["mix2-mf"]


def test_nightly_plans_all_mixes_and_passes_bench_options(suite_env, manifest):
    cases, bench = suite_env(tier="nightly", cluster_threshold=0.4, num_threads=2)
    _valid(cases)
    ids = [m["id"] for m in manifest["mixes"]]
    names = {c["case"] for c in cases}
    for prefix in ("sherpa-onnx-chunked", "sherpa-onnx-whole"):
        assert {f"{prefix}/{m}" for m in ids + ["all"]} <= names
    assert all(c["status"] == "ok" for c in cases)
    runs = [c for c in bench.commands if "--probe" not in c]
    sherpa_runs = [c for c in runs if "sherpa-onnx" in c]
    assert len(sherpa_runs) == 2
    for cmd in sherpa_runs:
        assert cmd[cmd.index("--cluster-threshold") + 1] == "0.4" and cmd[cmd.index("--num-threads") + 1] == "2"
        assert cmd[cmd.index("--runs") + 1] == "5"
    assert all("--cluster-threshold" not in c for c in runs if "mock" in c)
    assert len([c for c in bench.commands if "--probe" in c]) == 1
    assert sorted(suite_env.calls["build"]) == sorted(ids)


def test_probe_unavailable_skips_download(suite_env):
    reason = "unavailable: Speaker models are not downloaded. Run `lf speakers download`."
    cases, _ = suite_env({"probe_rc": 3, "probe_stderr": reason})
    _valid(cases)
    by = _by_case(cases)
    for name in ("sherpa-onnx-chunked/mix2-mf", "sherpa-onnx-chunked/all"):
        assert by[name]["status"] == "not_exercised" and by[name]["error"] == reason
    assert all(by[n]["status"] == "ok" for n in by if n.startswith("mock-"))
    assert suite_env.calls["ensure"] == [] and suite_env.calls["build"] == []


def test_probe_failure_fails_sherpa_cases(suite_env):
    cases, _ = suite_env({"probe_rc": 1, "probe_stderr": "Traceback: boom"})
    by = _by_case(cases)
    assert by["sherpa-onnx-chunked/all"]["status"] == "failed" and "boom" in by["sherpa-onnx-chunked/all"]["error"]
    assert suite_env.calls["ensure"] == []


def test_corpus_download_failure_fails_sherpa_cases(suite_env, monkeypatch):
    def broken(*a, **k):
        raise OSError("network down")

    monkeypatch.setattr(diarization, "ensure_stt_clips", broken)
    cases, _ = suite_env()
    _valid(cases)
    by = _by_case(cases)
    for name in ("sherpa-onnx-chunked/mix2-mf", "sherpa-onnx-chunked/all"):
        assert by[name]["status"] == "failed"
        assert by[name]["error"] == "diarization corpus unavailable: OSError: network down"
    assert by["mock-whole/all"]["status"] == "ok"


@pytest.mark.parametrize(("bench_kw", "needle"), [
    ({"returncode": 1, "stderr": "Traceback: crash"}, "bench exited 1: Traceback: crash"),
    ({"timeout": True}, "bench timed out after"),
])
def test_bench_crash_and_timeout_fail(suite_env, bench_kw, needle):
    cases, _ = suite_env(bench_kw)
    _valid(cases)
    assert cases and all(c["status"] == "failed" and needle in c["error"] for c in cases)


def test_bench_exit_3_is_not_exercised(suite_env):
    cases, _ = suite_env({"returncode": 3, "stderr": "unavailable: numpy is not installed"})
    assert all(c["status"] == "not_exercised" and "numpy" in c["error"] for c in cases)


def test_missing_and_malformed_mix_results(suite_env):
    cases, _ = suite_env({"drop": frozenset({"mix2-mf"})})
    by = _by_case(cases)
    assert by["sherpa-onnx-chunked/mix2-mf"]["status"] == "failed"
    assert "no result for this mix" in by["sherpa-onnx-chunked/mix2-mf"]["error"]
    assert by["sherpa-onnx-chunked/all"]["error"] == "aggregate needs every mix"
    cases, _ = suite_env({"malformed": frozenset({diarization.SYNTHETIC_MIX_ID})})
    _valid(cases)
    by = _by_case(cases)
    assert by["mock-whole/synthetic-tones"]["status"] == "failed"
    assert "malformed bench result: ValueError" in by["mock-whole/synthetic-tones"]["error"]
    assert by["mock-whole/all"]["status"] == "failed"
    assert by["sherpa-onnx-chunked/all"]["status"] == "ok"


def test_cases_from_report_rejects_bad_reports():
    entry = diarization.plan_diarization_matrix("smoke", "python")[0]
    mix = {"id": "synthetic-tones", "reference": SYNTHETIC_REFERENCE}
    for report in (None, {}, {"mixes": "x"}):
        cases = diarization.cases_from_report(entry, report, [mix])
        assert [c["status"] for c in cases] == ["failed", "failed"]
    report = {"mixes": [{"id": "synthetic-tones", "turns": "nope", "median_ms": 1.0, "audio_seconds": 38.0}]}
    cases = diarization.cases_from_report(entry, report, [mix])
    assert "TypeError" in cases[0]["error"] and cases[1]["error"] == "aggregate needs every mix"
    report = {"mixes": [{"id": "synthetic-tones", "median_ms": 1.0, "audio_seconds": 38.0}]}
    assert "KeyError" in diarization.cases_from_report(entry, report, [mix])[0]["error"]


def test_real_bench_subprocess_smoke(tmp_path):
    """No fakes: the two mock entries run the real bench; sherpa depends on the machine."""
    status, _reason = diarization.probe_backend(_ctx(), "sherpa-onnx")
    if status == "ok":
        pytest.skip("sherpa-onnx and its models are installed here; the sherpa path would download the corpus")
    cases = diarization.run_diarization(_ctx("python", "smoke", cache_dir=str(tmp_path / "cache")))
    _valid(cases)
    by = _by_case(cases)
    for name in ("mock-whole/synthetic-tones", "mock-chunked/synthetic-tones", "mock-whole/all",
                 "mock-chunked/all"):
        assert by[name]["status"] == "ok", by[name]
        assert by[name]["metrics"]["der"]["value"] == 0.0
        assert by[name]["metrics"]["speaker_count_exact"]["value"] == 1.0
    sherpa = [c for c in cases if c["dims"]["backend"] == "sherpa-onnx"]
    assert sherpa and all(c["status"] == "not_exercised" and c["error"] for c in sherpa)
    assert not (tmp_path / "cache").exists()  # nothing downloaded


def test_bench_command_uses_lf_perf_python(monkeypatch, tmp_path):
    entry = diarization.plan_diarization_matrix("smoke", "python")[2]
    monkeypatch.setenv("LF_PERF_PYTHON", "/opt/diar/bin/python")
    cmd = diarization.bench_command(_ctx(), entry, tmp_path / "m.json", tmp_path / "o.json")
    assert cmd[:2] == ["/opt/diar/bin/python", str(diarization.PY_BENCH)]
    cmd = diarization.bench_command(_ctx(python="/x/py", threshold=0.6), entry, tmp_path / "m.json",
                                    tmp_path / "o.json")
    assert cmd[0] == "/x/py" and cmd[cmd.index("--threshold") + 1] == "0.6"


# --------------------------------------------------------------------------
# Policy
# --------------------------------------------------------------------------


def test_policy_rules_and_caps():
    policy = load_policy()
    m = policy["metrics"]
    assert m["der"] == {"class": "quality", "better": "lower", "abs_tol": 0.03, "on_pr": "block"}
    for name in ("missed_speech", "false_alarm", "speaker_confusion"):
        assert m[name] == {"class": "quality", "better": "lower", "abs_tol": 0.03, "on_pr": "warn"}
    assert m["speaker_count_error"] == {"class": "quality", "better": "lower", "abs_tol": 0.5, "on_pr": "block"}
    assert m["speaker_count_exact"] == {"class": "quality", "better": "higher", "abs_tol": 0.0, "on_pr": "block"}
    assert m["rss_delta_bytes"] == {"class": "noisy", "better": "lower", "rel_tol": 0.10, "mad_k": 3.0,
                                    "block_factor": 2.0, "on_pr": "warn"}
    assert m["M14"] == {"class": "exact", "on_pr": "block"}
    caps = [c for c in policy["caps"] if c["suite"] == "diarization"]
    assert len(caps) == 3
    want = {"backend": "sherpa-onnx", "pipeline": "chunked", "kind": "aggregate"}
    assert all(c["dims_match"] == want for c in caps)
    assert {c["metric"]: c["max_value"] for c in caps} == {"der": 0.20, "rtf": 0.20, "rss_delta_bytes": 314572800}


def _agg_case(backend="sherpa-onnx", pipeline="chunked", kind="aggregate", **values):
    base = {"der": 0.10, "missed_speech": 0.05, "false_alarm": 0.02, "speaker_confusion": 0.03,
            "speaker_count_exact": 1.0, "rtf": 0.05}
    base.update({k: v for k, v in values.items() if k != "rss_delta_bytes"})
    metrics = {k: make_metric(unit="ratio", better="higher" if k == "speaker_count_exact" else "lower", value=v)
               for k, v in base.items()}
    metrics["speaker_count_error"] = make_metric(unit="count", better="lower", value=values.get("count", 0.0))
    metrics.pop("count", None)
    metrics["rss_delta_bytes"] = make_metric(unit="bytes", better="lower",
                                             value=values.get("rss_delta_bytes", 100_000_000))
    mix = "all" if kind == "aggregate" else "mix3"
    return make_case(suite="diarization", case=f"{backend}-{pipeline}/{mix}",
                     dims={"backend": backend, "pipeline": pipeline, "mix": mix, "kind": kind}, metrics=metrics)


def _compare(base_cases, cand_cases):
    report = compare_results(_valid(base_cases), _valid(cand_cases), load_policy())
    return {(r.case, r.metric): r for r in report.rows}, report


@pytest.mark.parametrize(("metric", "value"), [("der", 0.25), ("rtf", 0.3), ("rss_delta_bytes", 400_000_000)])
def test_caps_block_product_path_only(metric, value):
    case = _agg_case(**{metric: value})
    rows, report = _compare([case], [copy.deepcopy(case)])
    assert report.blocked and rows[(case["case"], metric)].blocking
    assert "cap:" in rows[(case["case"], metric)].note
    for other in (_agg_case(pipeline="whole", **{metric: value}), _agg_case(backend="mock", **{metric: value}),
                  _agg_case(kind="mix", **{metric: value})):
        rows, report = _compare([other], [copy.deepcopy(other)])
        assert not report.blocked, [r for r in rows.values() if r.blocking]


def test_der_regression_tolerance_and_components():
    base = _agg_case(der=0.10)
    rows, report = _compare([base], [_agg_case(der=0.12)])
    assert not report.blocked
    rows, report = _compare([base], [_agg_case(der=0.14)])
    assert report.blocked and rows[(base["case"], "der")].verdict == "regressed"
    rows, report = _compare([base], [_agg_case(missed_speech=0.15)])
    row = rows[(base["case"], "missed_speech")]
    assert row.verdict == "regressed" and not row.blocking and not report.blocked
    rows, report = _compare([base], [_agg_case(speaker_count_exact=0.75)])
    assert report.blocked


def test_cli_run_rust_smoke(tmp_path):
    out = tmp_path / "r.json"
    proc = subprocess.run([sys.executable, "-m", "evals.perf", "run", "--suite", "diarization", "--runtime", "rust",
                           "--tier", "smoke", "--output", str(out)], cwd=str(REPO_ROOT), capture_output=True,
                          text=True, check=False, timeout=120)
    assert proc.returncode == 0, proc.stderr
    data = json.loads(out.read_text())
    assert validate_result(data) == [] and len(data["cases"]) == 6
    assert all(c["status"] == "not_exercised" for c in data["cases"])
