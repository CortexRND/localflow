import os
import plistlib
import subprocess
import sys
from pathlib import Path

import pytest

from localflow import launchagent as L

pytestmark = pytest.mark.skipif(
    sys.platform == "win32" or os.environ.get("LOCALFLOW_PLATFORM") == "win32",
    reason="launchd tests are not supported on Windows",
)


@pytest.fixture(autouse=True)
def home(tmp_path, monkeypatch):
    """Point HOME at a scratch dir so plist/log paths never touch the real one."""
    monkeypatch.setenv("HOME", str(tmp_path))
    return tmp_path


# ------------------------------------------------------------- plist_content ---

def test_plist_is_valid_xml_with_expected_keys():
    text = L.plist_content("/opt/venv/bin/localflow")
    data = plistlib.loads(text.encode("utf-8"))

    assert data["Label"] == "com.cortexrnd.localflow"
    assert data["ProgramArguments"] == ["/opt/venv/bin/localflow"]
    assert data["RunAtLoad"] is True
    assert data["KeepAlive"] == {"SuccessfulExit": False}
    assert data["LimitLoadToSessionType"] == "Aqua"


def test_plist_log_paths_are_expanded_absolute(home):
    text = L.plist_content("/opt/venv/bin/localflow")
    data = plistlib.loads(text.encode("utf-8"))

    out_path = Path(data["StandardOutPath"])
    err_path = Path(data["StandardErrorPath"])
    assert out_path.is_absolute() and err_path.is_absolute()
    assert out_path == home / ".localflow" / "agent.out.log"
    assert err_path == home / ".localflow" / "agent.err.log"
    assert "~" not in data["StandardOutPath"]
    assert "~" not in data["StandardErrorPath"]


def test_plist_program_defaults_next_to_sys_executable(monkeypatch):
    monkeypatch.setattr(L.sys, "executable", "/opt/venv/bin/python3")
    text = L.plist_content()
    data = plistlib.loads(text.encode("utf-8"))
    assert data["ProgramArguments"] == ["/opt/venv/bin/localflow"]


# -------------------------------------------------------------------- install ---

def _fake_run(returncode=0, stdout="", stderr=""):
    def run(argv, **kwargs):
        return subprocess.CompletedProcess(argv, returncode, stdout=stdout, stderr=stderr)
    return run


def test_install_writes_plist_at_expected_path(home, monkeypatch):
    monkeypatch.setattr(subprocess, "run", _fake_run())
    path = L.install("/opt/venv/bin/localflow")

    assert path == home / "Library" / "LaunchAgents" / "com.cortexrnd.localflow.plist"
    assert path.exists()
    data = plistlib.loads(path.read_bytes())
    assert data["ProgramArguments"] == ["/opt/venv/bin/localflow"]


def test_install_creates_log_dir(home, monkeypatch):
    monkeypatch.setattr(subprocess, "run", _fake_run())
    L.install("/opt/venv/bin/localflow")
    assert (home / ".localflow").is_dir()


def test_install_calls_bootout_then_bootstrap(home, monkeypatch):
    calls = []

    def run(argv, **kwargs):
        calls.append(argv)
        return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", run)
    L.install("/opt/venv/bin/localflow")

    assert calls[0][:2] == ["launchctl", "bootout"]
    assert calls[0][2] == f"gui/{L.os.getuid()}/com.cortexrnd.localflow"
    assert calls[1][:2] == ["launchctl", "bootstrap"]
    assert calls[1][2] == f"gui/{L.os.getuid()}"
    assert calls[1][3].endswith("com.cortexrnd.localflow.plist")


def test_install_ignores_bootout_failure_when_not_previously_loaded(home, monkeypatch):
    def run(argv, **kwargs):
        if argv[1] == "bootout":
            return subprocess.CompletedProcess(argv, 1, stdout="", stderr="No such process")
        return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", run)
    path = L.install("/opt/venv/bin/localflow")  # must not raise
    assert path.exists()


def test_install_falls_back_to_load_on_any_bootstrap_failure(home, monkeypatch):
    """No stderr-text heuristic: any bootstrap failure tries `load -w` next.

    Uses a permission-denied-style message (still prefixed "Bootstrap
    failed:" on real macOS) to prove the fallback isn't gated on matching a
    specific substring — it fires unconditionally on nonzero exit.
    """
    calls = []

    def run(argv, **kwargs):
        calls.append(argv)
        if argv[1] == "bootstrap":
            return subprocess.CompletedProcess(argv, 1, stdout="", stderr="Bootstrap failed: 5: Input/output error")
        return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", run)
    L.install("/opt/venv/bin/localflow")  # load fallback succeeds, so no raise

    kinds = [c[1] for c in calls]
    assert kinds == ["bootout", "bootstrap", "load"]
    load_call = calls[2]
    assert load_call[:2] == ["launchctl", "load"]
    assert "-w" in load_call


def test_install_falls_back_even_on_generic_bootstrap_error(home, monkeypatch):
    """A permission/TCC-style failure with no special marker still falls back
    to `load -w` rather than raising immediately."""
    calls = []

    def run(argv, **kwargs):
        calls.append(argv)
        if argv[1] == "bootstrap":
            return subprocess.CompletedProcess(argv, 1, stdout="", stderr="Some other launchd error")
        return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", run)
    L.install("/opt/venv/bin/localflow")  # must not raise: load fallback succeeded
    assert [c[1] for c in calls] == ["bootout", "bootstrap", "load"]


def test_install_raises_with_both_errors_when_load_fallback_also_fails(home, monkeypatch):
    def run(argv, **kwargs):
        if argv[1] == "bootstrap":
            return subprocess.CompletedProcess(argv, 1, stdout="", stderr="Bootstrap failed: permission denied")
        if argv[1] == "load":
            return subprocess.CompletedProcess(argv, 1, stdout="", stderr="load also broken")
        return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", run)
    with pytest.raises(RuntimeError) as excinfo:
        L.install("/opt/venv/bin/localflow")
    # Both error outputs must survive so the real cause isn't masked.
    assert "permission denied" in str(excinfo.value)
    assert "load also broken" in str(excinfo.value)


# ------------------------------------------------------------------ uninstall ---

def test_uninstall_boots_out_and_removes_plist(home, monkeypatch):
    monkeypatch.setattr(subprocess, "run", _fake_run())
    L.install("/opt/venv/bin/localflow")
    plist_path = L._plist_path()
    assert plist_path.exists()

    calls = []

    def run(argv, **kwargs):
        calls.append(argv)
        return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", run)
    L.uninstall()

    assert not plist_path.exists()
    assert calls[0][:2] == ["launchctl", "bootout"]
    assert calls[0][2] == f"gui/{L.os.getuid()}/com.cortexrnd.localflow"


def test_uninstall_when_never_installed_does_not_raise(home, monkeypatch):
    monkeypatch.setattr(subprocess, "run", _fake_run(returncode=1, stderr="No such process"))
    L.uninstall()  # must not raise even though there's nothing to remove


# ---------------------------------------------------------------------- status ---

def test_status_running(monkeypatch):
    monkeypatch.setattr(subprocess, "run", _fake_run(returncode=0, stdout="state = running\n"))
    result = L.status()
    assert "running" in result
    assert "com.cortexrnd.localflow" in result


def test_status_loaded_not_running(monkeypatch):
    monkeypatch.setattr(subprocess, "run", _fake_run(returncode=0, stdout="state = not running\n"))
    result = L.status()
    assert "not running" in result


def test_status_not_loaded(monkeypatch):
    monkeypatch.setattr(subprocess, "run", _fake_run(returncode=3, stderr="Could not find service"))
    result = L.status()
    assert "not loaded" in result


# -------------------------------------------------------------- no real launchctl ---

def test_no_test_ever_invokes_real_launchctl(monkeypatch):
    """Guard: if any test above forgot to monkeypatch subprocess.run, fail loudly
    rather than actually touching the real launchd/plist on this machine."""
    def boom(argv, **kwargs):
        raise AssertionError(f"real subprocess.run invoked with {argv!r}")

    monkeypatch.setattr(subprocess, "run", boom)
    with pytest.raises(AssertionError):
        L.install("/opt/venv/bin/localflow")
