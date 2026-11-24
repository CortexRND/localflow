"""Unit tests for the perf-eval STT suite (no network, no models)."""

from __future__ import annotations

import ast
import copy
import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import wave
from pathlib import Path
from typing import ClassVar

import numpy as np
import pytest

from evals.perf import (
    compare_results,
    load_policy,
    make_result,
    make_run,
    registry,
    validate_result,
)
from evals.perf.benches import py_stt
from evals.perf.corpus import download
from evals.perf.registry import RunContext
from evals.perf.suites import stt
from evals.perf.wer import compute_wer, normalize_text, word_edit_distance

REPO_ROOT = Path(__file__).resolve().parents[1]
PERF_DIR = REPO_ROOT / "evals" / "perf"
STDLIB_OK = set(sys.stdlib_module_names) | {"__future__"}


# --------------------------------------------------------------------------
# Stdlib-only constraint (recursive; benches/ is the one exception)
# --------------------------------------------------------------------------


def _harness_files() -> list[Path]:
    return sorted(p for p in PERF_DIR.rglob("*.py") if "benches" not in p.relative_to(PERF_DIR).parts)


@pytest.mark.parametrize("path", _harness_files(), ids=lambda p: str(p.relative_to(PERF_DIR)))
def test_harness_modules_import_only_stdlib(path):
    tree = ast.parse(path.read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            mods = [a.name for a in node.names]
        elif isinstance(node, ast.ImportFrom):
            if node.level:
                continue
            mods = [node.module or ""]
        else:
            continue
        for mod in mods:
            top = mod.split(".")[0]
            assert top != "localflow", f"{path} imports localflow"
            assert top in STDLIB_OK, f"{path} imports non-stdlib {mod}"


def test_suite_registered_via_cli_import():
    from evals.perf import __main__ as cli  # noqa: F401 - import loads built-in suites

    assert registry.SUITES.get("stt") is stt.run_stt


# --------------------------------------------------------------------------
# WER
# --------------------------------------------------------------------------


def test_wer_exact_match():
    assert compute_wer("the cat sat", "the cat sat") == 0.0


def test_wer_ignores_case_punctuation_whitespace():
    assert compute_wer("THE CAT SAT", "  The cat,   sat. ") == 0.0
    ref = "THUS IT IS THAT THE HONOR OF THREE IS SAVED OUR COUNTRY'S OUR MASTER'S AND OUR OWN"
    hyp = "Thus it is that the honor of three is saved: our country's, our master's, and our own."
    assert compute_wer(ref, hyp) == 0.0
    assert normalize_text("Hello, World!") == ["hello", "world"]
    assert normalize_text("country's") == ["country", "s"]


def test_wer_substitution_insertion_deletion():
    assert compute_wer("a b c d", "a x c d") == pytest.approx(0.25)
    assert compute_wer("a b c d", "a b c d e") == pytest.approx(0.25)
    assert compute_wer("a b c d", "a c d") == pytest.approx(0.25)
    assert compute_wer("a b", "c d e f") == pytest.approx(2.0)
    assert word_edit_distance(["a", "b", "c"], ["b", "c", "d"]) == 2
    assert word_edit_distance(list("kitten"), list("sitting")) == 3
    assert word_edit_distance(list("sitting"), list("kitten")) == 3


def test_wer_empty_strings():
    assert compute_wer("", "") == 0.0
    assert compute_wer("  ...  ", "") == 0.0
    assert compute_wer("", "words") == 1.0
    assert compute_wer("two words", "") == 1.0
    assert word_edit_distance([], []) == 0


# --------------------------------------------------------------------------
# Corpus manifest
# --------------------------------------------------------------------------


@pytest.fixture(scope="module")
def corpus():
    return download.load_stt_manifest()


def test_corpus_manifest(corpus):
    clips = corpus["clips"]
    assert len(clips) == 11
    ids = [c["id"] for c in clips]
    assert len(set(ids)) == 11 and "parakeet-0" in ids
    libri = [c for c in clips if c.get("archive_url")]
    assert len(libri) == 10
    for clip in libri:
        assert clip["archive_url"].endswith("/test-clean.tar.gz")
        assert clip["archive_member"] == "LibriSpeech/test-clean/{}/{}/{}.flac".format(
            *clip["id"].split("-")[:2], clip["id"]
        )
    (parakeet,) = [c for c in clips if c.get("url")]
    assert parakeet["id"] == "parakeet-0" and parakeet["url"].endswith("test_wavs/0.wav")
    for clip in clips:
        assert len(clip["sha256"]) == 64 and int(clip["sha256"], 16) >= 0
        assert 3.0 <= clip["duration_s"] <= 15.0
        assert clip["reference"].strip() and clip["reference"] == clip["reference"].upper()
    assert [c["id"] for c in clips if c["smoke"]] == ["6930-75918-0000", "6930-75918-0007", "6930-75918-0010"]
    assert "CC BY 4.0" in corpus["license"]


def test_load_manifest_rejects_bad(tmp_path):
    good = {"id": "a", "sha256": "0" * 64, "url": "http://x/a.wav", "reference": "A"}
    cases = [
        {"clips": []},
        {"clips": [good | {"sha256": "abc"}]},
        {"clips": [good, good]},
        {"clips": [good | {"archive_url": "http://x/a.tgz", "archive_member": "a.flac"}]},
        {"clips": [good | {"reference": " "}]},
        {"clips": [good | {"id": "../evil"}]},
    ]
    for idx, data in enumerate(cases):
        path = tmp_path / f"m{idx}.json"
        path.write_text(json.dumps(data))
        with pytest.raises(ValueError):
            download.load_stt_manifest(path)


# --------------------------------------------------------------------------
# Downloader (mocked HTTP and ffmpeg)
# --------------------------------------------------------------------------


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _wav_bytes(samples: np.ndarray, rate: int = 16000, channels: int = 1, width: int = 2) -> bytes:
    buf = io.BytesIO()
    with wave.open(buf, "wb") as wav:
        wav.setnchannels(channels)
        wav.setsampwidth(width)
        wav.setframerate(rate)
        wav.writeframes(samples.tobytes())
    return buf.getvalue()


class _Opener:
    def __init__(self, payloads: dict[str, bytes]):
        self.payloads = payloads
        self.calls: list[str] = []

    def __call__(self, request, timeout=None):
        url = request.full_url
        self.calls.append(url)
        assert request.get_header("User-agent") == download.USER_AGENT
        return io.BytesIO(self.payloads[url])


def test_direct_download_and_cache_hit(tmp_path):
    data = _wav_bytes(np.zeros(1600, dtype="<i2"))
    manifest = {"clips": [{"id": "clip-a", "sha256": _sha(data), "url": "https://x/a.wav",
                           "reference": "HELLO", "duration_s": 0.1, "smoke": True}]}
    opener = _Opener({"https://x/a.wav": data})
    clips = download.ensure_stt_clips(manifest, tmp_path, urlopen=opener)
    assert clips == [{"id": "clip-a", "path": str(tmp_path / "clip-a.wav"), "reference": "HELLO",
                      "duration_s": 0.1, "smoke": True}]
    assert (tmp_path / "clip-a.wav").read_bytes() == data
    assert (tmp_path / "clip-a.sha256").read_text().strip() == _sha(data)
    assert opener.calls == ["https://x/a.wav"]

    def no_network(*a, **kw):
        raise AssertionError("cache hit must not touch the network")

    assert download.ensure_stt_clips(manifest, tmp_path, urlopen=no_network) == clips
    # A changed pin invalidates the cache.
    stale = copy.deepcopy(manifest)
    stale["clips"][0]["sha256"] = "f" * 64
    with pytest.raises(ValueError, match="sha256 mismatch"):
        download.ensure_stt_clips(stale, tmp_path, urlopen=opener)


def test_direct_download_sha_mismatch(tmp_path):
    manifest = {"clips": [{"id": "clip-a", "sha256": "0" * 64, "url": "https://x/a.wav", "reference": "A"}]}
    with pytest.raises(ValueError, match="sha256 mismatch for clip clip-a"):
        download.ensure_stt_clips(manifest, tmp_path, urlopen=_Opener({"https://x/a.wav": b"tampered"}))
    assert not (tmp_path / "clip-a.wav").exists()
    assert not (tmp_path / "clip-a.sha256").exists()
    assert list(tmp_path.iterdir()) == []  # temp file removed


def _targz(members: dict[str, bytes]) -> bytes:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        for name, data in members.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
    return buf.getvalue()


@pytest.fixture
def fake_ffmpeg(monkeypatch):
    calls: list[list[str]] = []

    def run(cmd, **kwargs):
        calls.append(cmd)
        src = Path(cmd[cmd.index("-i") + 1])
        # "decode": a WAV whose samples encode the source length, so tests can tell clips apart
        n = len(src.read_bytes())
        Path(cmd[-1]).write_bytes(_wav_bytes(np.full(n, 7, dtype="<i2")))
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(download.subprocess, "run", run)
    return calls


def _archive_manifest(flacs: dict[str, bytes], url: str = "https://x/test-clean.tar.gz") -> dict:
    return {"clips": [
        {"id": cid, "sha256": _sha(data), "archive_url": url, "archive_member": f"LibriSpeech/{cid}.flac",
         "reference": cid.upper(), "duration_s": 3.0, "smoke": cid == "a"}
        for cid, data in flacs.items()
    ]}


def test_archive_extraction_and_conversion(tmp_path, fake_ffmpeg):
    flacs = {"a": b"fLaC-a", "b": b"fLaC-bbbb"}
    manifest = _archive_manifest(flacs)
    archive = _targz({"LibriSpeech/README.txt": b"x", "LibriSpeech/a.flac": flacs["a"],
                      "LibriSpeech/b.flac": flacs["b"], "LibriSpeech/zzz.flac": b"unused"})
    opener = _Opener({"https://x/test-clean.tar.gz": archive})
    clips = download.ensure_stt_clips(manifest, tmp_path, urlopen=opener)
    assert [c["id"] for c in clips] == ["a", "b"]
    assert opener.calls == ["https://x/test-clean.tar.gz"]  # one stream for both clips
    assert len(fake_ffmpeg) == 2
    cmd = fake_ffmpeg[0]
    assert cmd[0] == "ffmpeg" and cmd[cmd.index("-ar") + 1] == "16000" and cmd[cmd.index("-ac") + 1] == "1"
    assert cmd[cmd.index("-c:a") + 1] == "pcm_s16le"
    for cid, data in flacs.items():
        with wave.open(str(tmp_path / f"{cid}.wav")) as wav:
            assert wav.getnframes() == len(data)
        assert (tmp_path / f"{cid}.sha256").read_text().strip() == _sha(data)
    # cache hit
    assert download.ensure_stt_clips(manifest, tmp_path, urlopen=lambda *a, **k: 1 / 0) == clips
    # smoke selection and explicit ids
    assert [c["id"] for c in download.ensure_stt_clips(manifest, tmp_path, smoke_only=True)] == ["a"]
    assert [c["id"] for c in download.ensure_stt_clips(manifest, tmp_path, clip_ids=["b"])] == ["b"]
    with pytest.raises(ValueError, match="unknown clip ids"):
        download.ensure_stt_clips(manifest, tmp_path, clip_ids=["nope"])


def test_archive_member_sha_mismatch_and_missing(tmp_path, fake_ffmpeg):
    manifest = _archive_manifest({"a": b"fLaC-a", "b": b"fLaC-b"})
    archive = _targz({"LibriSpeech/a.flac": b"tampered"})
    with pytest.raises(ValueError) as err:
        download.ensure_stt_clips(manifest, tmp_path, urlopen=_Opener({"https://x/test-clean.tar.gz": archive}))
    assert "sha256 mismatch for clip a" in str(err.value)
    assert "not found: LibriSpeech/b.flac" in str(err.value)
    assert not fake_ffmpeg
    assert not list(tmp_path.glob("*.wav"))


def test_flac_to_wav_reports_ffmpeg_errors(tmp_path, monkeypatch):
    def missing(cmd, **kw):
        raise FileNotFoundError(cmd[0])

    monkeypatch.setattr(download.subprocess, "run", missing)
    with pytest.raises(RuntimeError, match="ffmpeg not found"):
        download.flac_to_wav(b"x", tmp_path / "o.wav")
    monkeypatch.setattr(download.subprocess, "run",
                        lambda cmd, **kw: subprocess.CompletedProcess(cmd, 1, "", "Invalid data"))
    with pytest.raises(RuntimeError, match="Invalid data"):
        download.flac_to_wav(b"x", tmp_path / "o.wav")
    assert not (tmp_path / "o.wav").exists()


# --------------------------------------------------------------------------
# Matrix
# --------------------------------------------------------------------------


def _keys(plan):
    return [(e["engine"], e["model"], e["provider"]) for e in plan]


@pytest.mark.parametrize("runtime,engine", [("python", "faster-whisper"), ("rust", "whisper-cpp")])
@pytest.mark.parametrize("os_name", ["linux", "darwin", "win32"])
def test_matrix_smoke(runtime, engine, os_name):
    (entry,) = stt.plan_stt_matrix("smoke", runtime, os_name)
    assert entry == {"engine": engine, "model": "tiny.en", "provider": "cpu", "runs": 3, "warmup": 1,
                     "smoke_only": True, "not_exercised": None}


def test_matrix_nightly_linux():
    models = ["tiny.en", "base.en", "small"]
    assert _keys(stt.plan_stt_matrix("nightly", "python", "linux")) == [
        ("faster-whisper", m, "cpu") for m in models]
    assert _keys(stt.plan_stt_matrix("nightly", "rust", "linux")) == [
        ("whisper-cpp", m, "cpu") for m in models] + [("parakeet-onnx", "base", "cpu")]
    for runtime in ("python", "rust"):
        for e in stt.plan_stt_matrix("nightly", runtime, "linux"):
            assert (e["runs"], e["warmup"], e["smoke_only"], e["not_exercised"]) == (5, 1, False, None)


def test_matrix_nightly_macos():
    models = ["tiny.en", "base.en", "small"]
    assert _keys(stt.plan_stt_matrix("nightly", "rust", "macos")) == (
        [("whisper-cpp", m, "cpu") for m in models] + [("parakeet-onnx", "base", "cpu")]
        + [("whisper-cpp", m, "metal") for m in models] + [("parakeet-onnx", "base", "coreml")])
    assert _keys(stt.plan_stt_matrix("nightly", "python", "darwin")) == (
        [("faster-whisper", m, "cpu") for m in models]
        + [("mlx-whisper", m, "mlx") for m in ["tiny", "base", "small"]])


def test_matrix_nightly_windows():
    models = ["tiny.en", "base.en", "small"]
    assert _keys(stt.plan_stt_matrix("nightly", "python", "windows")) == [("faster-whisper", m, "cpu") for m in models]
    assert _keys(stt.plan_stt_matrix("nightly", "rust", "win32")) == [("whisper-cpp", m, "cpu") for m in models]


def test_matrix_deep():
    models = ["tiny.en", "base.en", "small", "medium", "large-v3"]
    py = stt.plan_stt_matrix("deep", "python", "linux")
    assert _keys(py) == [("faster-whisper", m, "cpu") for m in models] + [
        ("faster-whisper", m, "cuda") for m in models]
    assert all(e["not_exercised"] is None for e in py)
    rs = stt.plan_stt_matrix("deep", "rust", "linux")
    cuda = [e for e in rs if e["provider"] == "cuda"]
    assert _keys(cuda) == [("whisper-cpp", m, "cuda") for m in models]
    assert all(e["not_exercised"] == "Rust has no CUDA STT path" for e in cuda)
    assert ("parakeet-onnx", "base", "cpu") in _keys(rs)
    mac = stt.plan_stt_matrix("deep", "python", "macos")
    assert ("mlx-whisper", "large-v3", "mlx") in _keys(mac)
    assert not [e for e in mac if e["provider"] == "cuda"]


def test_matrix_rejects_bad_input():
    with pytest.raises(ValueError):
        stt.plan_stt_matrix("weekly", "python", "linux")
    with pytest.raises(ValueError):
        stt.plan_stt_matrix("smoke", "go", "linux")
    with pytest.raises(ValueError):
        stt.plan_stt_matrix("smoke", "python", "beos")


# --------------------------------------------------------------------------
# Suite runner (mocked subprocess and corpus)
# --------------------------------------------------------------------------


def _resolved(corpus, tmp_path) -> list[dict]:
    return [{"id": c["id"], "path": str(tmp_path / f"{c['id']}.wav"), "reference": c["reference"],
             "duration_s": c["duration_s"], "smoke": c["smoke"]} for c in corpus["clips"]]


class FakeBench:
    """Stands in for ``subprocess.run`` of stt_bench / py_stt.py."""

    def __init__(self, corpus, *, corrupt: set[str] = frozenset(), returncode: int = 0,
                 stderr: str = "", latency: float = 100.0, drop: set[str] = frozenset()):
        self.refs = {c["id"]: c["reference"] for c in corpus["clips"]}
        self.corrupt, self.returncode, self.stderr = corrupt, returncode, stderr
        self.latency, self.drop = latency, drop
        self.commands: list[list[str]] = []

    def __call__(self, cmd, **kwargs):
        self.commands.append(cmd)
        if self.returncode != 0:
            return subprocess.CompletedProcess(cmd, self.returncode, "", self.stderr)
        arg = lambda flag: cmd[cmd.index(flag) + 1]
        clips = json.loads(Path(arg("--manifest")).read_text())["clips"]
        runs = int(arg("--runs"))
        lat = [self.latency + i for i in range(runs)]
        report = {"mode": "manifest", "runs": runs, "warmup": int(arg("--warmup")), "os": "linux",
                  "arch": "x86_64", "results": [{
                      "engine": arg("--engines"), "model": arg("--model"), "requested_provider": arg("--provider"),
                      "effective_provider": "cpu", "download_ms": 0.5, "load_ms": 250.0,
                      "rss_before_bytes": 100_000_000, "rss_after_bytes": 300_000_000,
                      "rss_after_unload_bytes": 120_000_000,
                      "clips": [{
                          "id": c["id"], "clip": c["path"], "audio_seconds": 4.0, "original_sample_rate": 16000,
                          "latencies_ms": lat, "first_ms": lat[0], "median_ms": float(np.median(lat)),
                          "p95_ms": lat[-1], "real_time_factor": float(np.median(lat)) / 4000.0,
                          "transcript": ("garbled words here" if c["id"] in self.corrupt
                                         else self.refs[c["id"]].lower().capitalize() + "."),
                      } for c in clips if c["id"] not in self.drop],
                  }]}
        Path(arg("--json")).write_text(json.dumps(report))
        return subprocess.CompletedProcess(cmd, 0, json.dumps(report), "")


@pytest.fixture
def suite_env(monkeypatch, tmp_path, corpus):
    monkeypatch.setattr(stt, "ensure_stt_clips", lambda manifest, cache_dir, **kw: [
        c for c in _resolved(corpus, tmp_path) if kw.get("clip_ids") is None or c["id"] in kw["clip_ids"]])
    binary = tmp_path / "stt_bench"
    binary.write_text("#!/bin/sh\n")
    monkeypatch.delenv("LF_PERF_STT_BENCH_BIN", raising=False)

    def run(runtime: str, bench: FakeBench, tier: str = "smoke", **options):
        monkeypatch.setattr(stt.subprocess, "run", bench)
        opts = {"os": "linux", "bench_bin": str(binary), "cache_dir": str(tmp_path / "cache")} | options
        ctx = RunContext(tier=tier, runtime=runtime, repo_root=REPO_ROOT, options=opts)
        return stt.run_stt(ctx)

    return run


def _wrap(runtime: str, cases: list[dict], tier: str = "smoke") -> dict:
    run = make_run(tier=tier, runtime=runtime, sha="abc", ref="main", os_name="linux", arch="x86_64",
                   runner={"label": "test", "cpu": "x", "cores": 1, "ram_gb": 1})
    return make_result(run, cases)


@pytest.mark.parametrize("runtime,engine", [("rust", "whisper-cpp"), ("python", "faster-whisper")])
def test_suite_smoke_produces_valid_result(suite_env, corpus, runtime, engine):
    bench = FakeBench(corpus)
    cases = suite_env(runtime, bench)
    assert validate_result(_wrap(runtime, cases)) == []
    smoke_ids = [c["id"] for c in corpus["clips"] if c["smoke"]]
    assert [c["case"] for c in cases] == [f"{engine}-tiny.en-cpu/{cid}" for cid in smoke_ids]
    (cmd,) = bench.commands
    if runtime == "python":
        assert cmd[:2] == [sys.executable, str(stt.PY_BENCH)]
    else:
        assert cmd[0].endswith("stt_bench")
    for flag, value in [("--engines", engine), ("--model", "tiny.en"), ("--provider", "cpu"),
                        ("--runs", "3"), ("--warmup", "1")]:
        assert cmd[cmd.index(flag) + 1] == value
    case = cases[0]
    assert case["status"] == "ok" and case["error"] is None
    assert case["dims"] == {"engine": engine, "model": "tiny.en", "provider": "cpu", "clip": smoke_ids[0]}
    m = case["metrics"]
    assert set(m) == {"download_ms", "load_ms", "first_ms", "latency_ms", "rtf", "rss_before", "rss_loaded",
                      "rss_after_unload", "wer"}
    assert m["latency_ms"]["samples"] == [100.0, 101.0, 102.0] and m["latency_ms"]["median"] == 101.0
    assert m["rss_loaded"] == {"unit": "bytes", "better": "lower", "value": 300_000_000}
    assert m["rss_after_unload"]["value"] == 120_000_000
    assert m["wer"] == {"unit": "ratio", "better": "lower", "value": 0.0}  # case/punct normalised in harness
    assert m["rtf"]["unit"] == "ratio" and m["load_ms"]["value"] == 250.0
    assert case["evidence"]["effective_provider"] == "cpu"
    assert case["evidence"]["transcript"].endswith(".")


def test_suite_nightly_covers_every_clip(suite_env, corpus):
    cases = suite_env("rust", FakeBench(corpus), tier="nightly")
    assert validate_result(_wrap("rust", cases, "nightly")) == []
    assert len(cases) == 4 * 11 and all(c["status"] == "ok" for c in cases)
    assert "parakeet-onnx-base-cpu/parakeet-0" in {c["case"] for c in cases}


def test_suite_wer_computed_in_harness(suite_env, corpus):
    bad = "6930-75918-0007"
    cases = {c["dims"]["clip"]: c for c in suite_env("python", FakeBench(corpus, corrupt={bad}))}
    expected = compute_wer(corpus["clips"][2]["reference"], "garbled words here")
    assert expected > 0.5
    assert cases[bad]["metrics"]["wer"]["value"] == pytest.approx(expected)
    assert cases["6930-75918-0000"]["metrics"]["wer"]["value"] == 0.0


def test_suite_missing_rust_binary_is_not_exercised(suite_env, corpus, monkeypatch, tmp_path):
    monkeypatch.setattr(stt, "ensure_stt_clips", lambda *a, **k: pytest.fail("must not download"))
    bench = FakeBench(corpus)
    cases = suite_env("rust", bench, bench_bin=str(tmp_path / "missing"))
    assert len(cases) == 3 and not bench.commands
    assert all(c["status"] == "not_exercised" for c in cases)
    assert "cargo build --release -p lf-stt --example stt_bench" in cases[0]["error"]
    assert validate_result(_wrap("rust", cases)) == []


def test_find_rust_bench(tmp_path, monkeypatch):
    monkeypatch.delenv("LF_PERF_STT_BENCH_BIN", raising=False)
    assert stt.find_rust_bench(tmp_path) is None
    debug = tmp_path / "rust" / "target" / "debug" / "examples"
    debug.mkdir(parents=True)
    exe = "stt_bench.exe" if sys.platform == "win32" else "stt_bench"
    (debug / exe).write_text("")
    assert stt.find_rust_bench(tmp_path) == debug / exe
    release = tmp_path / "rust" / "target" / "release" / "examples"
    release.mkdir(parents=True)
    (release / exe).write_text("")
    assert stt.find_rust_bench(tmp_path) == release / exe
    monkeypatch.setenv("LF_PERF_STT_BENCH_BIN", str(tmp_path / "nope"))
    assert stt.find_rust_bench(tmp_path) is None
    monkeypatch.setenv("LF_PERF_STT_BENCH_BIN", str(debug / exe))
    assert stt.find_rust_bench(tmp_path) == debug / exe


def test_suite_deep_rust_cuda_not_exercised(suite_env, corpus):
    cases = suite_env("rust", FakeBench(corpus), tier="deep")
    cuda = [c for c in cases if c["dims"]["provider"] == "cuda"]
    assert len(cuda) == 5 * 11
    assert all(c["status"] == "not_exercised" and c["error"] == "Rust has no CUDA STT path" for c in cuda)
    assert all(c["status"] == "ok" for c in cases if c["dims"]["provider"] != "cuda")
    assert validate_result(_wrap("rust", cases, "deep")) == []


def test_suite_subprocess_error_is_failed(suite_env, corpus):
    stderr = "x" * 5000 + "Error: model download failed"
    cases = suite_env("python", FakeBench(corpus, returncode=1, stderr=stderr))
    assert len(cases) == 3 and all(c["status"] == "failed" for c in cases)
    assert cases[0]["error"].startswith("bench exited 1: ")
    assert cases[0]["error"].endswith("model download failed")
    assert len(cases[0]["error"]) < 1000
    assert validate_result(_wrap("python", cases)) == []


def test_suite_python_unavailable_is_not_exercised(suite_env, corpus):
    cases = suite_env("python", FakeBench(corpus, returncode=3, stderr="unavailable: mlx-whisper is not installed"))
    assert all(c["status"] == "not_exercised" and "not installed" in c["error"] for c in cases)


def test_suite_missing_clip_in_output_is_failed(suite_env, corpus):
    cases = {c["dims"]["clip"]: c for c in suite_env("rust", FakeBench(corpus, drop={"6930-75918-0010"}))}
    assert cases["6930-75918-0010"]["status"] == "failed"
    assert cases["6930-75918-0000"]["status"] == "ok"


def test_suite_timeout_and_bad_json(suite_env, corpus):
    def timeout(cmd, **kw):
        raise subprocess.TimeoutExpired(cmd, 5, stderr=b"stuck")

    cases = suite_env("rust", timeout)
    assert all(c["status"] == "failed" and "timed out" in c["error"] for c in cases)
    cases = suite_env("rust", lambda cmd, **kw: subprocess.CompletedProcess(cmd, 0, "", ""))
    assert all(c["status"] == "failed" and "bench JSON" in c["error"] for c in cases)


def test_suite_corpus_failure_is_failed(suite_env, corpus, monkeypatch):
    def offline(*a, **k):
        raise OSError("network unreachable")

    monkeypatch.setattr(stt, "ensure_stt_clips", offline)
    cases = suite_env("python", FakeBench(corpus))
    assert len(cases) == 3
    assert all(c["status"] == "failed" and "network unreachable" in c["error"] for c in cases)


def test_corrupted_transcript_fails_wer_gate(suite_env, corpus):
    policy = load_policy()
    baseline = _wrap("rust", suite_env("rust", FakeBench(corpus)))
    same = _wrap("rust", suite_env("rust", FakeBench(corpus)))
    assert compare_results(baseline, same, policy).exit_code == 0
    candidate = _wrap("rust", suite_env("rust", FakeBench(corpus, corrupt={"6930-75918-0000"})))
    report = compare_results(baseline, candidate, policy)
    assert report.exit_code == 1
    (row,) = [r for r in report.rows if r.blocking]
    assert row.metric == "wer" and row.verdict == "regressed"
    assert row.case == "whisper-cpp-tiny.en-cpu/6930-75918-0000"


def test_injected_slowdown_is_flagged(suite_env, corpus):
    policy = load_policy()
    baseline = _wrap("rust", suite_env("rust", FakeBench(corpus, latency=100.0)))
    slow = _wrap("rust", suite_env("rust", FakeBench(corpus, latency=250.0)))
    report = compare_results(baseline, slow, policy)
    assert report.exit_code == 1
    assert {r.metric for r in report.rows if r.blocking} >= {"latency_ms"}


def test_cli_runs_stt_suite_without_binary(tmp_path):
    out = tmp_path / "r.json"
    env = dict(os.environ, LF_PERF_STT_BENCH_BIN=str(tmp_path / "missing-bench"))
    proc = subprocess.run(
        [sys.executable, "-m", "evals.perf", "run", "--tier", "nightly", "--runtime", "rust", "--suite", "stt",
         "--output", str(out)], cwd=REPO_ROOT, env=env, capture_output=True, text=True, timeout=60, check=False)
    assert proc.returncode == 0, proc.stderr
    data = json.loads(out.read_text())
    assert validate_result(data) == []
    assert all(c["suite"] == "stt" and c["status"] == "not_exercised" for c in data["cases"])


# --------------------------------------------------------------------------
# py_stt.py bench
# --------------------------------------------------------------------------


def test_load_wav_16k_mono(tmp_path):
    samples = (np.sin(np.arange(1600) / 10) * 16000).astype("<i2")
    path = tmp_path / "a.wav"
    path.write_bytes(_wav_bytes(samples))
    pcm, rate = py_stt.load_wav(path)
    assert rate == 16000 and pcm.dtype == np.float32 and pcm.ndim == 1 and pcm.size == 1600
    np.testing.assert_allclose(pcm, samples / 32768.0, atol=1e-6)


def test_load_wav_stereo_and_32bit(tmp_path):
    frames = np.zeros((800, 2), dtype="<i2")
    frames[:, 0] = 8192
    path = tmp_path / "s.wav"
    path.write_bytes(_wav_bytes(frames.reshape(-1), channels=2))
    pcm, _ = py_stt.load_wav(path)
    assert pcm.size == 800 and np.allclose(pcm, 0.25)  # first channel, like the Rust bench

    ints = np.full(400, 1 << 30, dtype="<i4")
    path32 = tmp_path / "i32.wav"
    path32.write_bytes(_wav_bytes(ints, width=4))
    pcm, _ = py_stt.load_wav(path32)
    assert pcm.size == 400 and np.allclose(pcm, 0.5)


def test_load_wav_resamples(tmp_path):
    path = tmp_path / "r.wav"
    path.write_bytes(_wav_bytes(np.full(4800, 8192, dtype="<i2"), rate=48000))
    pcm, rate = py_stt.load_wav(path)
    assert rate == 48000 and pcm.size == 1600 and np.allclose(pcm, 0.25)
    ramp = np.arange(10, dtype=np.float32)
    np.testing.assert_allclose(py_stt.resample_linear(ramp, 8000), np.arange(0, 10, 0.5, dtype=np.float32)[:20],
                               atol=0.51)
    assert py_stt.resample_linear(ramp, 8000).size == 20


def test_load_wav_rejects_empty(tmp_path):
    path = tmp_path / "e.wav"
    path.write_bytes(_wav_bytes(np.zeros(0, dtype="<i2")))
    with pytest.raises(ValueError, match="empty"):
        py_stt.load_wav(path)


def test_py_stats_match_rust():
    assert py_stt.stats([4.0, 1.0, 2.0, 3.0]) == (2.5, 4.0)
    assert py_stt.stats([1.0]) == (1.0, 1.0)


class FakeTranscriber:
    instances: ClassVar[list[FakeTranscriber]] = []

    def __init__(self, model_size, language, backend, device):
        self.kwargs = {"model_size": model_size, "language": language, "backend": backend, "device": device}
        self.backend = backend
        self.calls = 0
        FakeTranscriber.instances.append(self)

    def transcribe(self, audio, *, prompt=None):
        assert audio.dtype == np.float32 and audio.ndim == 1
        self.calls += 1
        return f"clip of {audio.size} samples"


def _write_manifest(tmp_path) -> Path:
    clips = []
    for idx, n in enumerate((16000, 32000)):
        path = tmp_path / f"c{idx}.wav"
        path.write_bytes(_wav_bytes(np.zeros(n, dtype="<i2")))
        clips.append({"id": f"c{idx}", "path": path.name})  # relative to the manifest
    manifest = tmp_path / "m.json"
    manifest.write_text(json.dumps({"clips": clips}))
    return manifest


def test_py_stt_manifest_json_shape(tmp_path, capsys):
    FakeTranscriber.instances = []
    manifest = _write_manifest(tmp_path)
    out = tmp_path / "out.json"
    code = py_stt.main(["--manifest", str(manifest), "--engines", "faster-whisper", "--model", "tiny.en",
                        "--provider", "cpu", "--runs", "3", "--warmup", "2", "--json", str(out),
                        "--table", str(tmp_path / "t.md")], factory=FakeTranscriber)
    assert code == 0
    report = json.loads(out.read_text())
    assert json.loads(capsys.readouterr().out) == report
    assert set(report) == {"mode", "runs", "warmup", "os", "arch", "results"}
    assert (report["mode"], report["runs"], report["warmup"]) == ("manifest", 3, 2)
    (engine,) = report["results"]
    assert set(engine) == {"engine", "model", "requested_provider", "effective_provider", "download_ms", "load_ms",
                           "rss_before_bytes", "rss_after_bytes", "rss_after_unload_bytes", "clips"}
    assert (engine["engine"], engine["model"], engine["requested_provider"], engine["effective_provider"]) == (
        "faster-whisper", "tiny.en", "cpu", "cpu")
    assert engine["download_ms"] == 0.0 and engine["load_ms"] >= 0.0
    assert all(isinstance(engine[k], int) and engine[k] > 0
               for k in ("rss_before_bytes", "rss_after_bytes", "rss_after_unload_bytes"))
    assert [c["id"] for c in engine["clips"]] == ["c0", "c1"]
    for clip, seconds in zip(engine["clips"], (1.0, 2.0), strict=True):
        assert set(clip) == {"id", "clip", "audio_seconds", "original_sample_rate", "latencies_ms", "first_ms",
                             "median_ms", "p95_ms", "real_time_factor", "transcript"}
        assert clip["audio_seconds"] == seconds and clip["original_sample_rate"] == 16000
        assert len(clip["latencies_ms"]) == 3 and clip["first_ms"] == clip["latencies_ms"][0]
        assert clip["transcript"] == f"clip of {int(seconds * 16000)} samples"
        assert Path(clip["clip"]).is_absolute()
    (inst,) = FakeTranscriber.instances
    assert inst.kwargs == {"model_size": "tiny.en", "language": None, "backend": "faster-whisper", "device": "cpu"}
    assert inst.calls == 2 * (2 + 3)  # warm-up + timed runs per clip
    # The harness converts this report like a Rust one.
    entry = stt.plan_stt_matrix("smoke", "python", "linux")[0]
    refs = [{"id": "c0", "reference": "clip of 16000 samples"}, {"id": "c1", "reference": "x"}]
    cases = stt.cases_from_report(entry, report, refs)
    assert [c["status"] for c in cases] == ["ok", "ok"]
    assert cases[0]["metrics"]["wer"]["value"] == 0.0


def test_py_stt_single_clip_json_shape(tmp_path):
    clip = tmp_path / "one.wav"
    clip.write_bytes(_wav_bytes(np.zeros(16000, dtype="<i2")))
    out = tmp_path / "out.json"
    code = py_stt.main(["--clip", str(clip), "--runs", "2", "--json", str(out)], factory=FakeTranscriber)
    assert code == 0
    report = json.loads(out.read_text())
    assert set(report) == {"clip", "fixed_clip", "audio_seconds", "original_sample_rate", "sample_rate", "runs",
                           "os", "arch", "results"}
    assert report["fixed_clip"] == "test_wavs/0.wav" and report["sample_rate"] == 16000
    (res,) = report["results"]
    assert set(res) == {"engine", "model", "requested_provider", "effective_provider", "download_ms", "load_ms",
                        "rss_before_bytes", "rss_after_bytes", "rss_after_unload_bytes", "latencies_ms",
                        "first_ms", "median_ms", "p95_ms", "real_time_factor", "transcript"}
    assert res["requested_provider"] == "auto" and res["model"] == "base.en"


def test_py_stt_unavailable_and_bad_args(tmp_path):
    manifest = _write_manifest(tmp_path)

    def missing(**kwargs):
        raise ImportError("No module named 'mlx_whisper'")

    assert py_stt.main(["--manifest", str(manifest), "--engines", "mlx-whisper", "--model", "tiny",
                        "--provider", "mlx"], factory=missing) == py_stt.EXIT_UNAVAILABLE
    with pytest.raises(SystemExit):
        py_stt.main(["--runs", "0"])
    with pytest.raises(SystemExit):
        py_stt.main(["--warmup", "-1"])
    with pytest.raises(SystemExit):
        py_stt.main(["--clip", "a.wav", "--manifest", "m.json"])

    class Empty(FakeTranscriber):
        def transcribe(self, audio, *, prompt=None):
            return "  "

    with pytest.raises(RuntimeError, match="empty transcript"):
        py_stt.main(["--manifest", str(manifest)], factory=Empty)

    class Fallback(FakeTranscriber):
        def __init__(self, **kwargs):
            super().__init__(**kwargs)
            self.backend = "faster-whisper"

    with pytest.raises(ValueError, match="no build"):
        py_stt.main(["--manifest", str(manifest), "--engines", "mlx-whisper", "--model", "tiny.en",
                     "--provider", "mlx"], factory=Fallback)


def test_py_stt_parse_manifest_forms(tmp_path):
    arr = py_stt.parse_manifest(json.dumps([{"id": "a", "path": "/x/a.wav"}]), tmp_path)
    assert arr == [{"id": "a", "path": Path("/x/a.wav")}]
    obj = py_stt.parse_manifest(json.dumps({"clips": [{"id": "b", "path": "b.wav"}]}), tmp_path)
    assert obj == [{"id": "b", "path": tmp_path / "b.wav"}]
    for bad in ("[]", '{"clips": []}', '[{"id": "a"}]', '[{"id": "", "path": "a"}]',
                '[{"id": "a", "path": "1"}, {"id": "a", "path": "2"}]'):
        with pytest.raises((TypeError, ValueError)):
            py_stt.parse_manifest(bad, tmp_path)


def test_rss_bytes_positive():
    assert py_stt.rss_bytes() > 0
