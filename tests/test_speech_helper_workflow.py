"""Guard real-process measurement evidence, without downloading a model."""

import copy
import importlib.util
import json
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "ci_speech_helper", ROOT / "scripts/ci_speech_helper.py"
)
PROBE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE)


def measurements(mode):
    rows = []
    for stage in (
        "idle",
        "loaded",
        "transcribed",
        "unloaded",
        "reloaded",
        "dictation_off",
    ):
        active = mode == "helper" and stage in ("loaded", "transcribed", "reloaded")
        child = {"rss": 1000, "footprint": 900} if active else None
        rows.append(
            {
                "stage": stage,
                "main": {"rss": 100, "footprint": 90},
                "helper_pid": (43 if stage == "reloaded" else 42) if active else None,
                "helper": child,
                "total_rss": 1100 if active else 100,
                "total_footprint": 990 if active else 90,
            }
        )
    return {
        "transcript": PROBE.REFERENCE,
        "rows": rows,
        **dict.fromkeys(
            (
                "cold_load_ms",
                "cold_inference_ms",
                "warm_inference_ms",
                "reload_ms",
                "reload_inference_ms",
            ),
            1.0,
        ),
    }


def test_measurement_report_requires_helper_lifecycle_and_real_reference():
    data = measurements("helper")
    PROBE.validate(data, "helper")
    PROBE.validate(measurements("inprocess"), "inprocess")
    text = PROBE.report({"helper": data, "inprocess": measurements("inprocess")})
    assert "dictation_off" in text and "486.9" in text and PROBE.REFERENCE in text
    for bad in ({}, {**data, "transcript": "fake"}, {**data, "rows": []}):
        with pytest.raises((AssertionError, KeyError)):
            PROBE.validate(bad, "helper")
    bad = copy.deepcopy(data)
    bad["rows"][3]["helper_pid"] = 42
    with pytest.raises(AssertionError):
        PROBE.validate(bad, "helper")


def test_native_workflow_runs_measurements_after_fixed_clip_and_uploads_evidence():
    workflow = (ROOT / ".github/workflows/rust.yml").read_text()
    assert (
        workflow.index("scripts/ci_parakeet_memory.py")
        < workflow.index("scripts/ci_speech_helper.py")
        < workflow.index("name: memory-measurements")
    )
    assert "--pcm-output" in (ROOT / "scripts/ci_parakeet_memory.py").read_text()
    assert (
        "LF_HUB_SPEECH_PROFILE"
        in (ROOT / "rust/lf-ui/src/speech_profile.rs").read_text()
    )


def test_memory_baseline_uses_separate_cargo_artifacts():
    workflow = (ROOT / ".github/workflows/rust.yml").read_text()
    assert 'CARGO_TARGET_DIR="$baseline/target" cargo build' in workflow
    assert '"$baseline/target/release/lf-hub" --page Home' in workflow
    assert '"$PWD/artifacts/memory/after" rust/target/release/lf-hub --page Home' in workflow


def test_whisper_smoke_selects_auto_tiny_and_validates_a_real_helper(
    tmp_path, monkeypatch
):
    monkeypatch.setattr(PROBE.os, "environ", {"LOCALFLOW_SPEECH_IN_PROCESS": "1"})
    data = measurements("helper")
    data["transcript"] = "A real Whisper transcript, not the Parakeet reference."

    class Process:
        def __init__(self, command, env, **kwargs):
            assert command[-2:] == ["--page", "Home"]
            config = Path(env["LOCALFLOW_CONFIG"]).read_text()
            assert 'provider = "auto"' in config
            assert 'model = "tiny.en"' in config
            assert "LOCALFLOW_SPEECH_IN_PROCESS" not in env
            Path(env["LF_HUB_SPEECH_PROFILE"]).write_text(json.dumps(data))

        def terminate(self):
            pass

        def wait(self, timeout=None):
            return 0

    monkeypatch.setattr(PROBE.subprocess, "Popen", Process)
    assert PROBE.run(tmp_path, tmp_path / "whisper", "whisper") == data
    for bad in ({**data, "transcript": ""}, {**data, "rows": []}):
        with pytest.raises(AssertionError):
            PROBE.validate(bad, "whisper")
    assert "--whisper" in (ROOT / ".github/workflows/rust.yml").read_text()
    assert data["transcript"] in PROBE.report_whisper(data)
