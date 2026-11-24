"""Unit tests for the perf-eval footprint suite, adapters and idle bench.

Fast and deterministic: no Rust build, no real Hub; the startup probe is
exercised against a tiny local HTTP stub server.
"""

from __future__ import annotations

import ast
import copy
import http.server
import io
import json
import os
import stat
import subprocess
import sys
import tarfile
import threading
import zipfile
from pathlib import Path
from typing import ClassVar

import pytest

from evals.perf import (
    adapters,
    compare_results,
    load_policy,
    make_result,
    make_run,
    registry,
    validate_result,
)
from evals.perf.adapters import hub_rss, package_size, speech_helper
from evals.perf.benches import py_idle
from evals.perf.registry import RunContext
from evals.perf.suites import footprint

REPO_ROOT = Path(__file__).resolve().parents[1]
PERF_DIR = REPO_ROOT / "evals" / "perf"
STDLIB_OK = set(sys.stdlib_module_names) | {"__future__"}
POSIX_ONLY = pytest.mark.skipif(sys.platform == "win32", reason="uses shebang stub executables")

MiB = 1024**2


def _valid(cases: list[dict]) -> None:
    run = make_run(tier="smoke", runtime="rust", sha="abc", ref="test", os_name="linux", arch="x86_64",
                   runner={"label": "t", "cpu": "c", "cores": 1, "ram_gb": 1})
    errors = validate_result(make_result(run, cases))
    assert errors == [], errors
    assert all(c["suite"] == "footprint" for c in cases)


def _by_case(cases: list[dict]) -> dict[str, dict]:
    return {c["case"]: c for c in cases}


def _value(case: dict, metric: str):
    return case["metrics"][metric]["value"]


# --------------------------------------------------------------------------
# Stdlib-only constraint
# --------------------------------------------------------------------------


def _footprint_harness_files() -> list[Path]:
    return sorted((PERF_DIR / "adapters").glob("*.py")) + [PERF_DIR / "suites" / "footprint.py"]


@pytest.mark.parametrize("path", _footprint_harness_files(), ids=lambda p: p.name)
def test_footprint_modules_import_only_stdlib(path):
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


def test_py_idle_imports_only_stdlib_at_module_level():
    # The bench may import localflow, but only dynamically (importlib), so it
    # runs (``--bare``) without the product installed.
    tree = ast.parse((PERF_DIR / "benches" / "py_idle.py").read_text(encoding="utf-8"))
    for node in tree.body:
        if isinstance(node, ast.Import):
            mods = [a.name for a in node.names]
        elif isinstance(node, ast.ImportFrom) and not node.level:
            mods = [node.module or ""]
        else:
            continue
        for mod in mods:
            assert mod.split(".")[0] in STDLIB_OK, mod


def test_adapters_package_exports():
    for name in adapters.__all__:
        assert callable(getattr(adapters, name)) or name == "SUITE"
    assert adapters.SUITE == "footprint"


def test_suite_registered_via_cli_import():
    from evals.perf import __main__ as cli  # noqa: F401 - import loads built-in suites

    assert registry.SUITES.get("footprint") is footprint.run_footprint
    assert "footprint" in registry.PLANNED_SUITES


def test_policy_has_footprint_rules():
    policy = load_policy(PERF_DIR / "policy.toml")
    rules = policy["metrics"]
    assert rules["helper_pid_cleaned"]["class"] == "exact"
    assert rules["helper_pid_cleaned"]["on_pr"] == "block"
    for name in ("package_size_bytes", "installed_size_bytes", "hwm_bytes", "rss_bytes", "footprint_bytes",
                 "startup_ms", "cpu_seconds"):
        assert rules[name]["class"] == "noisy", name


# --------------------------------------------------------------------------
# speech_helper adapter
# --------------------------------------------------------------------------


def _mem(rss_mib: float, fp_mib: float) -> dict:
    return {"rss": int(rss_mib * MiB), "footprint": int(fp_mib * MiB)}


def speech_result(mode: str = "helper") -> dict:
    rows = []
    pids = {"loaded": 4101, "transcribed": 4101, "reloaded": 4202}
    for stage in speech_helper.STAGES:
        main = _mem(100, 80)
        helper = None
        pid = None
        if mode != "inprocess" and stage in speech_helper.HELPER_STAGES:
            pid = pids[stage]
            helper = _mem(900, 850)
        rows.append({
            "stage": stage,
            "main": main,
            "helper_pid": pid,
            "helper": helper,
            "total_rss": main["rss"] + (helper["rss"] if helper else 0),
            "total_footprint": main["footprint"] + (helper["footprint"] if helper else 0),
        })
    return {
        "rows": rows,
        "transcript": "Well, I don't wish to see it any more.",
        "cold_load_ms": 4580.25,
        "cold_inference_ms": 1219.0,
        "warm_inference_ms": 900.5,
        "reload_ms": 4100.0,
        "reload_inference_ms": 1000.0,
    }


def test_speech_helper_stage_cases_and_metrics():
    cases = adapters.convert_speech_helper(speech_result(), mode="helper")
    _valid(cases)
    by = _by_case(cases)
    expected = {f"speech_helper/helper/{s}" for s in speech_helper.STAGES} | {
        "speech_helper/helper/timings",
        "speech_helper/helper/lifecycle",
    }
    assert set(by) == expected
    loaded = by["speech_helper/helper/loaded"]
    assert loaded["status"] == "ok"
    assert loaded["dims"] == {"component": "speech_helper", "mode": "helper", "stage": "loaded"}
    assert _value(loaded, "main_rss_bytes") == 100 * MiB
    assert _value(loaded, "main_footprint_bytes") == 80 * MiB
    assert _value(loaded, "helper_rss_bytes") == 900 * MiB
    assert _value(loaded, "helper_footprint_bytes") == 850 * MiB
    assert _value(loaded, "total_rss_bytes") == 1000 * MiB
    assert _value(loaded, "total_footprint_bytes") == 930 * MiB
    assert loaded["metrics"]["total_rss_bytes"]["unit"] == "bytes"
    idle = by["speech_helper/helper/idle"]
    assert "helper_rss_bytes" not in idle["metrics"]
    assert idle["evidence"] == {"helper_running": False}


def test_speech_helper_timings_mapping():
    by = _by_case(adapters.convert_speech_helper(speech_result()))
    t = by["speech_helper/helper/timings"]["metrics"]
    assert t["cold_ms"]["value"] == 4580.25
    assert t["cold_inference_ms"]["value"] == 1219.0
    assert t["warm_ms"]["value"] == 900.5
    assert t["reload_ms"]["value"] == 4100.0
    assert t["reload_inference_ms"]["value"] == 1000.0
    assert all(m["unit"] == "ms" and m["better"] == "lower" for m in t.values())


def test_speech_helper_lifecycle_gate_passes():
    by = _by_case(adapters.convert_speech_helper(speech_result()))
    life = by["speech_helper/helper/lifecycle"]
    gate = life["metrics"]["helper_pid_cleaned"]
    assert gate == {"unit": "bool", "better": "exact", "value": True}
    assert life["metrics"]["helper_respawned"]["value"] is True
    assert life["evidence"]["problems"] == []
    assert life["evidence"]["transcript"].startswith("Well")


def test_speech_helper_lifecycle_gate_fails_on_leaked_helper():
    data = speech_result()
    unloaded = next(r for r in data["rows"] if r["stage"] == "unloaded")
    unloaded["helper_pid"] = 4101
    unloaded["helper"] = _mem(900, 850)
    by = _by_case(adapters.convert_speech_helper(data))
    life = by["speech_helper/helper/lifecycle"]
    assert life["metrics"]["helper_pid_cleaned"]["value"] is False
    assert any("unloaded" in p for p in life["evidence"]["problems"])


def test_speech_helper_lifecycle_flags_reused_pid():
    data = speech_result()
    next(r for r in data["rows"] if r["stage"] == "reloaded")["helper_pid"] = 4101
    life = _by_case(adapters.convert_speech_helper(data))["speech_helper/helper/lifecycle"]
    assert life["metrics"]["helper_pid_cleaned"]["value"] is True
    assert life["metrics"]["helper_respawned"]["value"] is False


def test_speech_helper_inprocess_mode():
    cases = adapters.convert_speech_helper(speech_result("inprocess"), mode="inprocess")
    _valid(cases)
    life = _by_case(cases)["speech_helper/inprocess/lifecycle"]
    assert life["metrics"]["helper_pid_cleaned"]["value"] is True
    assert "helper_respawned" not in life["metrics"]
    # A helper in in-process mode is a leak.
    data = speech_result("inprocess")
    data["rows"][1]["helper_pid"] = 77
    life = _by_case(adapters.convert_speech_helper(data, mode="inprocess"))["speech_helper/inprocess/lifecycle"]
    assert life["metrics"]["helper_pid_cleaned"]["value"] is False


def test_speech_helper_missing_stage_and_malformed_row():
    data = speech_result()
    data["rows"] = [r for r in data["rows"] if r["stage"] != "dictation_off"]
    data["rows"][0]["main"] = {"rss": "big"}
    cases = adapters.convert_speech_helper(data)
    _valid(cases)
    by = _by_case(cases)
    assert by["speech_helper/helper/dictation_off"]["status"] == "failed"
    assert by["speech_helper/helper/idle"]["status"] == "failed"
    assert "malformed" in by["speech_helper/helper/idle"]["error"]
    assert by["speech_helper/helper/lifecycle"]["metrics"]["helper_pid_cleaned"]["value"] is False


def test_speech_helper_rejects_non_result():
    cases = adapters.convert_speech_helper({"nope": 1})
    _valid(cases)
    assert [c["status"] for c in cases] == ["failed"]


def test_speech_helper_load_directory_infers_modes(tmp_path):
    for mode in ("helper", "inprocess"):
        (tmp_path / mode).mkdir()
        (tmp_path / mode / "result.json").write_text(json.dumps(speech_result(mode)), encoding="utf-8")
    cases = adapters.load_speech_helper(tmp_path)
    _valid(cases)
    modes = {c["dims"]["mode"] for c in cases}
    assert modes == {"helper", "inprocess"}
    # A single file takes its mode from the parent directory.
    single = adapters.load_speech_helper(tmp_path / "inprocess" / "result.json")
    assert all(c["dims"]["mode"] == "inprocess" for c in single)


def test_speech_helper_load_unreadable(tmp_path):
    bad = tmp_path / "result.json"
    bad.write_text("{not json", encoding="utf-8")
    cases = adapters.load_speech_helper(bad)
    _valid(cases)
    assert cases[0]["status"] == "failed"
    empty = adapters.load_speech_helper(tmp_path / "missing-dir-is-a-file.json")
    assert empty[0]["status"] == "failed"


def test_speech_helper_gate_compares_as_exact_and_blocks():
    policy = load_policy(PERF_DIR / "policy.toml")
    run = make_run(tier="smoke", runtime="rust", sha="a", ref="r", os_name="macos", arch="arm64",
                   runner={"label": "t", "cpu": "c", "cores": 1, "ram_gb": 1})
    base = make_result(run, adapters.convert_speech_helper(speech_result()))
    bad = speech_result()
    next(r for r in bad["rows"] if r["stage"] == "dictation_off")["helper_pid"] = 9
    cand = make_result(run, adapters.convert_speech_helper(bad))
    report = compare_results(base, cand, policy)
    row = next(r for r in report.rows if r.metric == "helper_pid_cleaned")
    assert row.verdict == "regressed" and row.blocking
    assert report.exit_code == 1
    same = compare_results(base, copy.deepcopy(base), policy)
    assert not same.blocked


# --------------------------------------------------------------------------
# parakeet_memory adapter
# --------------------------------------------------------------------------


def parakeet_entry(name: str, **extra) -> dict:
    entry = {
        "name": name,
        "options": {},
        "repo": "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
        "threads": None,
        "provider": "cpu",
        "rss_before": 20 * MiB,
        "rss_after_load": 1100 * MiB,
        "rss_after_transcribe": 1140 * MiB,
        "rss_after_unload": 700 * MiB,
        "rss_after_drop": 600 * MiB,
        "rss_after_relief": 500 * MiB,
        "pressure_relief_bytes": 100 * MiB,
        "load_ms": 4580.2,
        "latency_ms": 1219.0,
        "transcript": "after early nightfall",
        "clip": "test_wavs/0.wav",
        "clip_sample_rate": 16000,
        "footprint_before": 5 * MiB,
        "footprint_load": 1074 * MiB,
        "footprint_transcribe": 1128 * MiB,
        "footprint_unload": 487 * MiB,
        "footprint_drop": None,
        "footprint_relief": 400 * MiB,
        "clip_sha256": "0" * 64,
        "exact_match": True,
    }
    entry.update(extra)
    return entry


def test_parakeet_memory_one_case_per_configuration():
    data = [
        parakeet_entry("int8-default"),
        parakeet_entry("int8-threads1", threads=1, exact_match=False),
        {"name": "int8-arena-off", "options": {"LF_ORT_ARENA": "1"}, "error": "Session-options shim unavailable"},
    ]
    cases = adapters.convert_parakeet_memory(data)
    _valid(cases)
    by = _by_case(cases)
    assert set(by) == {"parakeet_memory/int8-default", "parakeet_memory/int8-threads1", "parakeet_memory/int8-arena-off"}
    ok = by["parakeet_memory/int8-default"]
    assert ok["status"] == "ok"
    assert ok["dims"] == {"component": "parakeet_memory", "config": "int8-default", "provider": "cpu"}
    assert _value(ok, "rss_after_load") == 1100 * MiB
    assert _value(ok, "footprint_load") == 1074 * MiB
    assert "footprint_drop" not in ok["metrics"]  # null without LF_MEMORY_PROBE
    assert ok["metrics"]["pressure_relief_bytes"]["better"] == "higher"
    assert ok["metrics"]["load_ms"] == {"unit": "ms", "better": "lower", "value": 4580.2}
    assert ok["metrics"]["exact_match"] == {"unit": "bool", "better": "exact", "value": True}
    assert ok["evidence"]["transcript"] == "after early nightfall"
    assert by["parakeet_memory/int8-threads1"]["dims"]["threads"] == 1
    assert _value(by["parakeet_memory/int8-threads1"], "exact_match") is False
    err = by["parakeet_memory/int8-arena-off"]
    assert err["status"] == "failed"
    assert err["error"] == "Session-options shim unavailable"
    assert err["metrics"] == {}
    assert err["evidence"] == {"options": {"LF_ORT_ARENA": "1"}}


def test_parakeet_memory_malformed_and_duplicates():
    broken = parakeet_entry("int8-graph-off")
    del broken["rss_after_load"]
    cases = adapters.convert_parakeet_memory([broken, parakeet_entry("x"), parakeet_entry("x"), "junk"])
    _valid(cases)
    by = _by_case(cases)
    assert by["parakeet_memory/int8-graph-off"]["status"] == "failed"
    assert "rss_after_load" in by["parakeet_memory/int8-graph-off"]["error"]
    assert "parakeet_memory/x" in by and "parakeet_memory/x#2" in by


def test_parakeet_memory_not_a_list_and_empty():
    for data in ({"name": "x"}, []):
        cases = adapters.convert_parakeet_memory(data)
        _valid(cases)
        assert cases[0]["status"] == "failed"


def test_parakeet_memory_load_file_and_dir(tmp_path):
    (tmp_path / "results.json").write_text(json.dumps([parakeet_entry("int8-default")]), encoding="utf-8")
    assert adapters.load_parakeet_memory(tmp_path)[0]["status"] == "ok"
    assert adapters.load_parakeet_memory(tmp_path / "results.json")[0]["status"] == "ok"
    assert adapters.load_parakeet_memory(tmp_path / "nope.json")[0]["status"] == "failed"


# --------------------------------------------------------------------------
# hub_pages adapter
# --------------------------------------------------------------------------

PAGES_CSV = "Home,51200,40960\nHistory,52224,41984\nHome_after_History,51712,41472\nClosed,40000,30000\n"


def test_hub_pages_converts_kib_to_bytes():
    cases = adapters.convert_hub_pages(PAGES_CSV)
    _valid(cases)
    by = _by_case(cases)
    assert list(by) == ["hub_pages/Home", "hub_pages/History", "hub_pages/Home_after_History", "hub_pages/Closed"]
    home = by["hub_pages/Home"]
    assert home["dims"] == {"component": "hub_pages", "state": "Home"}
    assert home["metrics"]["rss_bytes"] == {"unit": "bytes", "better": "lower", "value": 51200 * 1024}
    assert _value(home, "footprint_bytes") == 40960 * 1024


def test_hub_pages_header_label_bad_rows_and_duplicates():
    text = "state,rss_kib,footprint_kib\r\nHome,1,2\r\nHome,3,4\r\nBroken,abc,5\r\nShort,1\r\n\r\n"
    cases = adapters.convert_hub_pages(text, label="candidate")
    _valid(cases)
    by = _by_case(cases)
    assert by["hub_pages/candidate/Home"]["dims"]["label"] == "candidate"
    assert _value(by["hub_pages/candidate/Home#2"], "rss_bytes") == 3 * 1024
    assert by["hub_pages/candidate/Broken"]["status"] == "failed"
    assert by["hub_pages/candidate/Short"]["status"] == "failed"


def test_hub_pages_empty_and_load(tmp_path):
    assert adapters.convert_hub_pages("")[0]["status"] == "failed"
    (tmp_path / "pages.csv").write_bytes(("﻿" + PAGES_CSV).encode("utf-8"))
    cases = adapters.load_hub_pages(tmp_path)  # directory → pages.csv, BOM tolerated
    _valid(cases)
    assert cases[0]["case"] == "hub_pages/Home"
    assert adapters.load_hub_pages(tmp_path / "missing.csv")[0]["status"] == "failed"


# --------------------------------------------------------------------------
# hub_rss adapter
# --------------------------------------------------------------------------

HUB_RSS_TXT = """\
### Rust Hub idle RSS (single process, KiB)

| Seconds after launch | RSS (KiB) |
| --- | --- |
| 10 | 51234 |
| 20 | 51240 |
| 30 | 51260 |
Measured executable: /x/rust/target/release/lf-hub
Build profile: release

### Rust vmmap --summary

| Region | Size |
| --- | --- |
| 10 | 99999 |
"""


def test_hub_rss_parses_only_the_idle_table():
    assert adapters.parse_hub_rss(HUB_RSS_TXT) == [("Rust", "10", 51234), ("Rust", "20", 51240), ("Rust", "30", 51260)]
    cases = adapters.convert_hub_rss(HUB_RSS_TXT)
    _valid(cases)
    by = _by_case(cases)
    assert set(by) == {"hub_rss/rust/10s", "hub_rss/rust/20s", "hub_rss/rust/30s"}
    c = by["hub_rss/rust/30s"]
    assert c["dims"] == {"component": "hub_rss", "label": "rust", "seconds": 30}
    assert c["metrics"]["rss_bytes"] == {"unit": "bytes", "better": "lower", "value": 51260 * 1024}


def test_hub_rss_multiple_sections_and_label_override():
    text = HUB_RSS_TXT + "\n### Python Hub idle RSS (single process, KiB)\n\n" \
        "| Seconds after launch | RSS (KiB) |\n| --- | --- |\n| 10 | 70000 |\n"
    by = _by_case(adapters.convert_hub_rss(text))
    assert _value(by["hub_rss/python/10s"], "rss_bytes") == 70000 * 1024
    assert "hub_rss/rust/10s" in by
    # Overriding the label makes both tables share a slug; names stay unique.
    cases = adapters.convert_hub_rss(text, label="Candidate Build")
    _valid(cases)
    names = [c["case"] for c in cases]
    assert "hub_rss/candidate-build/10s" in names and "hub_rss/candidate-build#2/10s" in names


def test_hub_rss_without_heading_and_without_table(tmp_path):
    bare = "| Seconds after launch | RSS (KiB) |\n| --- | --- |\n| 10 | 5 |\n"
    assert adapters.convert_hub_rss(bare)[0]["case"] == "hub_rss/hub/10s"
    none = adapters.convert_hub_rss("nothing here")
    _valid(none)
    assert none[0]["status"] == "failed"
    path = tmp_path / "idle.txt"
    path.write_text(HUB_RSS_TXT, encoding="utf-8")
    assert len(adapters.load_hub_rss(path)) == 3
    assert adapters.load_hub_rss(tmp_path / "nope.txt")[0]["status"] == "failed"
    assert hub_rss._slug("  ") == "hub"


# --------------------------------------------------------------------------
# package_size adapter
# --------------------------------------------------------------------------


def _write_tar(path: Path, files: dict[str, bytes]) -> None:
    with tarfile.open(path, "w:gz") as tar:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
        d = tarfile.TarInfo("LocalFlow/bin")
        d.type = tarfile.DIRTYPE
        tar.addfile(d)


def test_split_version():
    assert package_size.split_version("LocalFlow-Preview-0.3.1-linux-x86_64.tar.gz") == (
        "LocalFlow-Preview-linux-x86_64.tar.gz", "0.3.1")
    assert package_size.split_version("LocalFlow-1.2.0.dmg") == ("LocalFlow.dmg", "1.2.0")
    assert package_size.split_version("localflow_0.4.0_amd64.deb") == ("localflow_amd64.deb", "0.4.0")
    assert package_size.split_version("LocalFlow.AppImage") == ("LocalFlow.AppImage", None)
    assert package_size.package_suffix("x.sha256") is None
    assert package_size.package_suffix("A.TAR.GZ") == ".tar.gz"


def test_package_sizes(tmp_path):
    tar = tmp_path / "LocalFlow-Preview-0.3.1-linux-x86_64.tar.gz"
    _write_tar(tar, {"LocalFlow/bin/lf-desktop": b"x" * 5000, "LocalFlow/README": b"y" * 100})
    (tmp_path / (tar.name + ".sha256")).write_text("ab" * 32 + f"  {tar.name}\n", encoding="utf-8")
    sub = tmp_path / "gpui-preview-windows-x86_64"
    sub.mkdir()
    zpath = sub / "LocalFlow-Preview-0.3.1-windows-x86_64.zip"
    with zipfile.ZipFile(zpath, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        zf.writestr("LocalFlow/lf-desktop.exe", b"\0" * 20000)
        zf.writestr("LocalFlow/", b"")
    dmg = tmp_path / "LocalFlow-0.3.1.dmg"
    dmg.write_bytes(b"d" * 1234)
    (tmp_path / "notes.txt").write_text("ignored", encoding="utf-8")

    cases = adapters.measure_packages(tmp_path)
    _valid(cases)
    by = _by_case(cases)
    assert set(by) == {
        "package/LocalFlow-Preview-linux-x86_64.tar.gz",
        "package/gpui-preview-windows-x86_64/LocalFlow-Preview-windows-x86_64.zip",
        "package/LocalFlow.dmg",
    }
    t = by["package/LocalFlow-Preview-linux-x86_64.tar.gz"]
    assert _value(t, "package_size_bytes") == tar.stat().st_size
    assert _value(t, "installed_size_bytes") == 5100
    assert t["metrics"]["package_size_bytes"]["unit"] == "bytes"
    assert t["dims"] == {"component": "package", "artifact": "LocalFlow-Preview-linux-x86_64.tar.gz", "format": "tar.gz"}
    assert t["evidence"] == {"file": tar.name, "version": "0.3.1", "sha256": "ab" * 32}
    z = by["package/gpui-preview-windows-x86_64/LocalFlow-Preview-windows-x86_64.zip"]
    assert _value(z, "installed_size_bytes") == 20000
    assert _value(z, "package_size_bytes") == zpath.stat().st_size
    d = by["package/LocalFlow.dmg"]
    assert _value(d, "package_size_bytes") == 1234
    assert "installed_size_bytes" not in d["metrics"]


def test_package_corrupt_archive_and_empty_dir(tmp_path):
    (tmp_path / "LocalFlow-1.0.0-linux.tar.gz").write_bytes(b"not a tarball")
    cases = adapters.measure_packages(tmp_path)
    _valid(cases)
    assert cases[0]["status"] == "failed"
    empty = tmp_path / "empty"
    empty.mkdir()
    assert adapters.measure_packages(empty)[0]["status"] == "not_exercised"
    assert adapters.measure_packages(tmp_path / "missing")[0]["status"] == "failed"


# --------------------------------------------------------------------------
# idle.rs output parsing
# --------------------------------------------------------------------------

IDLE_LINE = "idle 10s: VmRSS 12345 kB | VmHWM 23456 kB | threads 7 | cpu ticks 3"


def test_parse_idle_output():
    data = footprint.parse_idle_output("warming up\n" + IDLE_LINE + "\n", clk_tck=100)
    assert data == {
        "seconds": 10,
        "rss_bytes": 12345 * 1024,
        "hwm_bytes": 23456 * 1024,
        "threads": 7,
        "cpu_seconds": 0.03,
        "cpu_ticks": 3,
        "clk_tck": 100,
    }
    assert footprint.parse_idle_output(IDLE_LINE, clk_tck=250)["cpu_seconds"] == pytest.approx(0.012)


def test_parse_idle_output_uses_sysconf_by_default():
    data = footprint.parse_idle_output(IDLE_LINE)
    assert data["clk_tck"] == footprint.clock_ticks_per_second() > 0


@pytest.mark.parametrize("text", ["", "hello", "idle 10s: VmRSS n/a | VmHWM n/a | threads n/a | cpu ticks 0"])
def test_parse_idle_output_rejects(text):
    with pytest.raises(ValueError):
        footprint.parse_idle_output(text, clk_tck=100)


def test_idle_case_valid():
    case = footprint.idle_case("rust", footprint.parse_idle_output(IDLE_LINE, clk_tck=100))
    _valid([case])
    assert case["case"] == "idle/rust"
    assert case["metrics"]["threads"] == {"unit": "count", "better": "lower", "value": 7}
    assert case["metrics"]["cpu_seconds"]["unit"] == "s"


def _stub_exe(path: Path, body: str) -> Path:
    path.write_text(f"#!{sys.executable}\n{body}", encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


def _ctx(runtime: str, tmp_path: Path, tier: str = "smoke", **options) -> RunContext:
    return RunContext(tier=tier, runtime=runtime, repo_root=tmp_path, options=options)


@POSIX_ONLY
def test_run_idle_rust_with_stub_binary(tmp_path):
    stub = _stub_exe(tmp_path / "idle", (
        "import sys, os\n"
        "assert 'lf-perf-idle-' in os.environ['HOME']\n"
        "print('idle ' + sys.argv[1] + 's: VmRSS 2000 kB | VmHWM 3000 kB | threads 5 | cpu ticks 0')\n"
    ))
    cases = footprint.run_footprint(_ctx("rust", tmp_path, os="linux", probes="idle", idle_bin=str(stub), idle_seconds=1))
    _valid(cases)
    (case,) = cases
    assert case["status"] == "ok", case["error"]
    assert _value(case, "rss_bytes") == 2000 * 1024
    assert _value(case, "cpu_seconds") == 0.0
    assert case["evidence"]["idle_seconds"] == 1


@POSIX_ONLY
def test_run_idle_rust_failures(tmp_path):
    crash = _stub_exe(tmp_path / "crash", "import sys\nsys.stderr.write('boom')\nsys.exit(4)\n")
    (case,) = footprint.run_footprint(_ctx("rust", tmp_path, os="linux", probes=["idle"], idle_bin=str(crash)))
    assert case["status"] == "failed" and "exited 4" in case["error"] and "boom" in case["error"]
    junk = _stub_exe(tmp_path / "junk", "print('nothing useful')\n")
    (case,) = footprint.run_footprint(_ctx("rust", tmp_path, os="linux", probes="idle", idle_bin=str(junk)))
    assert case["status"] == "failed" and "no 'idle" in case["error"]


def test_run_idle_rust_not_exercised(tmp_path, monkeypatch):
    monkeypatch.delenv("LF_PERF_IDLE_BIN", raising=False)
    (mac,) = footprint.run_footprint(_ctx("rust", tmp_path, os="macos", probes="idle"))
    assert mac["status"] == "not_exercised" and "Linux" in mac["error"]
    (missing,) = footprint.run_footprint(_ctx("rust", tmp_path, os="linux", probes="idle"))
    assert missing["status"] == "not_exercised" and "LF_PERF_IDLE_BIN" in missing["error"]
    monkeypatch.setenv("LF_PERF_IDLE_BIN", str(tmp_path / "nope"))
    (env_missing,) = footprint.run_footprint(_ctx("rust", tmp_path, os="linux", probes="idle"))
    assert env_missing["status"] == "not_exercised"
    for case in (mac, missing, env_missing):
        _valid([case])


def test_find_binaries_prefers_release(tmp_path, monkeypatch):
    monkeypatch.delenv("LF_PERF_IDLE_BIN", raising=False)
    monkeypatch.delenv("LF_PERF_LF_RS_BIN", raising=False)
    exe = ".exe" if sys.platform == "win32" else ""
    for profile in ("debug", "release"):
        d = tmp_path / "rust" / "target" / profile / "examples"
        d.mkdir(parents=True)
        (d / f"idle{exe}").write_text("", encoding="utf-8")
        (d.parent / f"lf-rs{exe}").write_text("", encoding="utf-8")
    assert footprint.find_idle_bin(tmp_path).parts[-3] == "release"
    assert footprint.find_lf_rs(tmp_path).parts[-2] == "release"
    explicit = tmp_path / "custom"
    explicit.write_text("", encoding="utf-8")
    monkeypatch.setenv("LF_PERF_LF_RS_BIN", str(explicit))
    assert footprint.find_lf_rs(tmp_path) == explicit.resolve()


def test_idle_seconds_default_per_tier(tmp_path, monkeypatch):
    seen = []
    monkeypatch.setattr(footprint, "run_idle", lambda ctx, os_name, seconds: seen.append(seconds) or
                        footprint._idle_non_ok(ctx.runtime, "not_exercised", "stub"))
    footprint.run_footprint(_ctx("python", tmp_path, "smoke", probes="idle"))
    footprint.run_footprint(_ctx("python", tmp_path, "nightly", probes="idle"))
    footprint.run_footprint(_ctx("python", tmp_path, "nightly", probes="idle", idle_seconds=3))
    assert seen == [10, 30, 3]


# --------------------------------------------------------------------------
# py_idle bench
# --------------------------------------------------------------------------


def test_py_idle_sample_shape():
    s = py_idle.sample()
    assert set(s) == {"rss_bytes", "hwm_bytes", "threads", "source"}
    if sys.platform.startswith("linux"):
        assert s["source"] == "proc"
        assert s["rss_bytes"] > 0 and s["hwm_bytes"] >= s["rss_bytes"]
        assert s["threads"] >= 1


def test_py_idle_bare_cli(tmp_path):
    out = tmp_path / "idle.json"
    proc = subprocess.run(
        [sys.executable, str(PERF_DIR / "benches" / "py_idle.py"), "--seconds", "0.1", "--bare", "--json", str(out)],
        capture_output=True, text=True, encoding="utf-8", timeout=60, check=False,
    )
    assert proc.returncode == 0, proc.stderr
    data = json.loads(out.read_text(encoding="utf-8"))
    assert data["mode"] == "bare" and data["imports"] == []
    assert data["seconds"] == 0.1
    assert data["cpu_seconds"] >= 0
    for key in ("rss_bytes", "hwm_bytes", "threads"):
        assert data[key] is None or data[key] >= 0
    if sys.platform.startswith("linux"):
        assert data["rss_bytes"] > 0


def test_py_idle_stdout_and_bad_args(capsys):
    assert py_idle.main(["--seconds", "0", "--bare"]) == 0
    data = json.loads(capsys.readouterr().out)
    assert data["seconds"] == 0.0
    assert py_idle.main(["--seconds", "-1", "--bare"]) == 2


def test_py_idle_unavailable_module_exits_3(tmp_path, monkeypatch):
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "c.toml"))
    assert py_idle.main(["--seconds", "0", "--imports", "lf_perf_no_such_module_xyz"]) == py_idle.EXIT_UNAVAILABLE


def test_py_idle_headless_fallback(monkeypatch):
    calls = []

    def fake_import(name):
        calls.append(name)
        if name == "localflow.app":
            raise ImportError("this platform is not supported", name="pynput")

    monkeypatch.setattr(py_idle.importlib, "import_module", fake_import)
    monkeypatch.setattr(py_idle.importlib.util, "find_spec", lambda name: object())
    modules, mode = py_idle.import_runtime(None)
    assert mode == "headless" and modules == list(py_idle.HEADLESS_IMPORTS)
    assert calls[0] == "localflow.app"
    assert py_idle.import_runtime(["a", "b"]) == (["a", "b"], "runtime")


@POSIX_ONLY
def test_run_idle_python_with_stub_interpreter(tmp_path):
    # A fake interpreter stands in for `python py_idle.py ...` and writes the JSON.
    stub = _stub_exe(tmp_path / "python", (
        "import json, sys, os\n"
        "args = sys.argv[1:]\n"
        "assert args[0].endswith('py_idle.py')\n"
        "assert os.environ['LOCALFLOW_CONFIG'].startswith(os.environ['XDG_CONFIG_HOME'])\n"
        "out = args[args.index('--json') + 1]\n"
        "open(out, 'w', encoding='utf-8').write(json.dumps({'seconds': float(args[args.index('--seconds') + 1]),"
        " 'rss_bytes': 100, 'hwm_bytes': 200, 'threads': 3, 'cpu_seconds': 0.01, 'mode': 'headless',"
        " 'source': 'proc'}))\n"
    ))
    (case,) = footprint.run_footprint(_ctx("python", tmp_path, probes="idle", python=str(stub), idle_seconds=2))
    _valid([case])
    assert case["status"] == "ok", case["error"]
    assert _value(case, "hwm_bytes") == 200
    assert case["evidence"] == {"idle_seconds": 2.0, "source": "py_idle/proc", "mode": "headless"}

    unavailable = _stub_exe(tmp_path / "python3", "import sys\nsys.stderr.write('no localflow')\nsys.exit(3)\n")
    (case,) = footprint.run_footprint(_ctx("python", tmp_path, probes="idle", python=str(unavailable)))
    assert case["status"] == "not_exercised" and case["error"] == "no localflow"

    partial = _stub_exe(tmp_path / "python4", (
        "import json, sys\nout = sys.argv[sys.argv.index('--json') + 1]\n"
        "open(out, 'w', encoding='utf-8').write(json.dumps({'rss_bytes': None, 'hwm_bytes': 1,"
        " 'threads': 1, 'cpu_seconds': 0}))\n"
    ))
    (case,) = footprint.run_footprint(_ctx("python", tmp_path, probes="idle", python=str(partial)))
    assert case["status"] == "failed" and "rss_bytes" in case["error"]


def test_run_idle_python_real_bench_bare(tmp_path):
    ctx = RunContext(tier="smoke", runtime="python", repo_root=REPO_ROOT,
                     options={"probes": "idle", "idle_seconds": 0, "idle_bare": True})
    (case,) = footprint.run_footprint(ctx)
    _valid([case])
    if sys.platform.startswith("linux") or sys.platform == "win32":
        assert case["status"] == "ok", case["error"]
        assert case["evidence"]["mode"] == "bare"


# --------------------------------------------------------------------------
# Startup probe (tiny local HTTP stub server)
# --------------------------------------------------------------------------

STUB_SERVER = """\
import http.server, json, os, sys, time, tomllib
mode = os.environ.get("STUB_MODE", "ok")
if mode == "exit":
    print("stub crashed"); sys.exit(5)
if "--port" in sys.argv:
    port = int(sys.argv[sys.argv.index("--port") + 1])
else:
    with open(os.environ["LOCALFLOW_CONFIG"], "rb") as f:
        port = tomllib.load(f)["server"]["port"]
time.sleep(float(os.environ.get("STUB_DELAY", "0")))
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"ok": mode != "notok"}).encode() if self.path == "/healthz" else b""
        self.send_response(200 if self.path == "/healthz" else 404)
        self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
"""


@pytest.fixture
def stub_server(tmp_path) -> Path:
    path = tmp_path / "stub_server.py"
    path.write_text(STUB_SERVER, encoding="utf-8")
    return path


def test_free_port_is_bindable():
    port = footprint.free_port()
    assert 0 < port < 65536


class _Handler(http.server.BaseHTTPRequestHandler):
    responses: ClassVar[dict] = {}

    def do_GET(self):
        status, body = self.responses.get(self.path, (404, b""))
        self.send_response(status)
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def test_health_ok_variants():
    _Handler.responses = {
        "/ok": (200, b'{"ok": true}'),
        "/notok": (200, b'{"ok": false}'),
        "/plain": (200, b"fine"),
        "/other": (200, b'{"status": "up"}'),
        "/err": (503, b'{"ok": true}'),
    }
    server = http.server.HTTPServer(("127.0.0.1", 0), _Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        base = f"http://127.0.0.1:{server.server_address[1]}"
        assert footprint.health_ok(base + "/ok")
        assert not footprint.health_ok(base + "/notok")
        assert footprint.health_ok(base + "/plain")
        assert footprint.health_ok(base + "/other")
        assert not footprint.health_ok(base + "/err")
    finally:
        server.shutdown()
        server.server_close()
    assert not footprint.health_ok(f"http://127.0.0.1:{footprint.free_port()}/healthz", timeout=0.2)


def test_measure_startup_against_stub(stub_server):
    procs = []
    samples = footprint.measure_startup(
        lambda port: procs.append(port) or [sys.executable, str(stub_server), "--port", str(port)],
        runs=3, warmup=1, timeout=30,
    )
    assert len(samples) == 3 and len(procs) == 4
    assert all(s > 0 for s in samples)
    for port in procs:  # every server was terminated
        assert not footprint.health_ok(f"http://127.0.0.1:{port}/healthz", timeout=0.2)


def test_measure_startup_reads_port_from_config(stub_server):
    samples = footprint.measure_startup(lambda port: [sys.executable, str(stub_server)], runs=1, warmup=0, timeout=30)
    assert len(samples) == 1


def test_measure_startup_reason_passthrough():
    assert footprint.measure_startup(lambda port: "no binary", runs=3, warmup=1, timeout=1) == "no binary"


def test_measure_startup_server_exits(stub_server, monkeypatch):
    monkeypatch.setenv("STUB_MODE", "exit")
    with pytest.raises(footprint.StartupError) as err:
        footprint.measure_startup(lambda port: [sys.executable, str(stub_server)], runs=1, warmup=0, timeout=30)
    assert "exited 5" in str(err.value) and "stub crashed" in str(err.value)


def test_measure_startup_never_healthy_times_out_and_kills(stub_server, monkeypatch):
    monkeypatch.setenv("STUB_MODE", "notok")
    ports = []
    with pytest.raises(footprint.StartupError) as err:
        footprint.measure_startup(
            lambda port: ports.append(port) or [sys.executable, str(stub_server), "--port", str(port)],
            runs=1, warmup=0, timeout=1.5,
        )
    assert "not ok after" in str(err.value)
    assert not footprint.health_ok(f"http://127.0.0.1:{ports[0]}/healthz", timeout=0.2)


@POSIX_ONLY
def test_run_startup_rust_stub_binary(tmp_path, stub_server):
    lf_rs = _stub_exe(tmp_path / "lf-rs", (
        "import runpy, sys\n"
        "assert sys.argv[1:4] == ['serve', '--host', '127.0.0.1'], sys.argv\n"
        f"runpy.run_path({str(stub_server)!r}, run_name='__main__')\n"
    ))
    cases = footprint.run_footprint(_ctx("rust", tmp_path, probes="startup", lf_rs_bin=str(lf_rs)))
    _valid(cases)
    (case,) = cases
    assert case["status"] == "ok", case["error"]
    assert case["case"] == "startup/rust"
    m = case["metrics"]["startup_ms"]
    assert m["unit"] == "ms" and m["better"] == "lower" and len(m["samples"]) == 3
    assert case["evidence"]["server"] == "lf-rs serve"


@POSIX_ONLY
def test_run_startup_python_stub_interpreter(tmp_path, stub_server):
    python = _stub_exe(tmp_path / "python", (
        "import runpy, sys\n"
        "assert sys.argv[1:] == ['-m', 'localflow.cli', 'serve'], sys.argv\n"
        f"runpy.run_path({str(stub_server)!r}, run_name='__main__')\n"
    ))
    (case,) = footprint.run_footprint(_ctx("python", tmp_path, "nightly", probes="startup", python=str(python),
                                           startup_warmup=0))
    assert case["status"] == "ok", case["error"]
    assert len(case["metrics"]["startup_ms"]["samples"]) == 5  # nightly default
    assert case["evidence"]["warmup"] == 0


def test_run_startup_rust_missing_binary(tmp_path, monkeypatch):
    monkeypatch.delenv("LF_PERF_LF_RS_BIN", raising=False)
    (case,) = footprint.run_footprint(_ctx("rust", tmp_path, probes="startup"))
    _valid([case])
    assert case["status"] == "not_exercised" and "LF_PERF_LF_RS_BIN" in case["error"]


def test_run_startup_failure_is_failed_case(tmp_path, stub_server, monkeypatch):
    monkeypatch.setenv("STUB_MODE", "exit")
    monkeypatch.setattr(footprint, "startup_command", lambda ctx, port: [sys.executable, str(stub_server)])
    (case,) = footprint.run_footprint(_ctx("python", tmp_path, probes="startup"))
    _valid([case])
    assert case["status"] == "failed" and "exited 5" in case["error"]


def test_isolated_env(tmp_path):
    env = footprint.isolated_env(tmp_path, repo_root=REPO_ROOT)
    for key in ("HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "APPDATA"):
        assert Path(env[key]).is_relative_to(tmp_path) and Path(env[key]).is_dir()
    assert Path(env["LOCALFLOW_CONFIG"]).is_relative_to(tmp_path)
    assert env["PYTHONPATH"].split(os.pathsep)[0] == str(REPO_ROOT)
    assert "127.0.0.1" in env["NO_PROXY"]
    assert env["PYTHONIOENCODING"] == "utf-8"


# --------------------------------------------------------------------------
# Artifact wiring + whole suite
# --------------------------------------------------------------------------


def _artifact_env_clear(monkeypatch):
    for _, env_var, _, _ in footprint.ARTIFACTS:
        monkeypatch.delenv(env_var, raising=False)
    monkeypatch.delenv("LF_PERF_EXPECT_ARTIFACTS", raising=False)


def test_artifacts_omitted_or_not_exercised(tmp_path, monkeypatch):
    _artifact_env_clear(monkeypatch)
    assert footprint.run_footprint(_ctx("rust", tmp_path, probes="artifacts")) == []
    cases = footprint.run_footprint(_ctx("rust", tmp_path, "nightly", probes="artifacts", expect_artifacts=True))
    _valid(cases)
    assert [c["case"] for c in cases] == ["speech_helper/*", "parakeet_memory/*", "hub_pages/*", "hub_rss/*", "package/*"]
    assert all(c["status"] == "not_exercised" for c in cases)
    monkeypatch.setenv("LF_PERF_EXPECT_ARTIFACTS", "1")
    assert len(footprint.run_footprint(_ctx("rust", tmp_path, "nightly", probes="artifacts"))) == 5


def test_artifacts_from_options_and_env(tmp_path, monkeypatch):
    _artifact_env_clear(monkeypatch)
    (tmp_path / "helper").mkdir()
    speech = tmp_path / "helper" / "result.json"
    speech.write_text(json.dumps(speech_result()), encoding="utf-8")
    parakeet = tmp_path / "results.json"
    parakeet.write_text(json.dumps([parakeet_entry("int8-default")]), encoding="utf-8")
    pages = tmp_path / "pages.csv"
    pages.write_text(PAGES_CSV, encoding="utf-8")
    rss = tmp_path / "rss.txt"
    rss.write_text(HUB_RSS_TXT, encoding="utf-8")
    pkg = tmp_path / "pkg"
    pkg.mkdir()
    (pkg / "LocalFlow-0.1.0.AppImage").write_bytes(b"a" * 10)

    monkeypatch.setenv("LF_PERF_HUB_RSS_TXT", str(rss))
    monkeypatch.setenv("LF_PERF_PACKAGE_DIR", str(pkg))
    cases = footprint.run_footprint(_ctx(
        "rust", tmp_path, probes="artifacts",
        speech_helper_json=str(speech), parakeet_memory_json=str(parakeet), hub_pages_csv=str(pages),
    ))
    _valid(cases)
    names = {c["case"] for c in cases}
    assert "speech_helper/helper/lifecycle" in names
    assert "parakeet_memory/int8-default" in names
    assert "hub_pages/Home" in names
    assert "hub_rss/rust/30s" in names
    assert "package/LocalFlow.AppImage" in names
    assert all(c["status"] == "ok" for c in cases)


def test_artifact_adapter_crash_is_reported(tmp_path, monkeypatch):
    _artifact_env_clear(monkeypatch)

    def boom(path):
        raise RuntimeError("kaput")

    monkeypatch.setattr(footprint, "ARTIFACTS", (("x_json", "LF_PERF_X", boom, "x"),))
    (case,) = footprint.run_footprint(_ctx("rust", tmp_path, probes="artifacts", x_json="/whatever"))
    assert case["status"] == "failed" and "kaput" in case["error"]


def test_unknown_probe_rejected(tmp_path):
    with pytest.raises(ValueError):
        footprint.run_footprint(_ctx("rust", tmp_path, probes="idle,bogus"))


def test_full_suite_without_binaries_is_valid(tmp_path, monkeypatch):
    _artifact_env_clear(monkeypatch)
    monkeypatch.delenv("LF_PERF_IDLE_BIN", raising=False)
    monkeypatch.delenv("LF_PERF_LF_RS_BIN", raising=False)
    cases = footprint.run_footprint(_ctx("rust", tmp_path, os="linux"))
    _valid(cases)
    assert [c["case"] for c in cases] == ["idle/rust", "startup/rust"]
    assert all(c["status"] == "not_exercised" for c in cases)


def test_cli_run_footprint_rust_without_binaries(tmp_path, monkeypatch):
    from evals.perf import __main__ as cli

    _artifact_env_clear(monkeypatch)
    monkeypatch.setenv("LF_PERF_IDLE_BIN", str(tmp_path / "missing-idle"))
    monkeypatch.setenv("LF_PERF_LF_RS_BIN", str(tmp_path / "missing-lf-rs"))
    out = tmp_path / "result.json"
    code = cli.main(["run", "--tier", "smoke", "--runtime", "rust", "--suite", "footprint", "--output", str(out)])
    assert code == 0
    data = json.loads(out.read_text(encoding="utf-8"))
    assert validate_result(data) == []
    assert {c["case"] for c in data["cases"]} == {"idle/rust", "startup/rust"}
