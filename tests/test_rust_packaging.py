"""Exercise packaging and launchd migration using fake macOS tools off macOS."""

import json
import os
import plistlib
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
BASH = shutil.which("bash") if os.name != "nt" else None
pytestmark = pytest.mark.skipif(BASH is None, reason="requires Bash")
LABEL = "com.cortexrnd.localflow"

TOOLS = r"""
import json, os, plistlib, sys
from pathlib import Path
name = Path(sys.argv[0]).name
a = sys.argv[1:]
with open(os.environ['CALLS'], 'a') as log:
    log.write(json.dumps([name, *a]) + '\n')
if name == 'uname':
    print('Darwin')
elif name == 'plutil':
    p = Path(a[-1])
    if a[0] == '-create':
        p.write_bytes(plistlib.dumps({}))
    elif a[0] == '-insert':
        v = plistlib.loads(p.read_bytes())
        v[a[1]] = (a[3] == 'YES') if a[2] == '-bool' else a[3]
        p.write_bytes(plistlib.dumps(v))
    else:
        v = plistlib.loads(p.read_bytes())
        if a[0] == '-extract':
            for key in a[1].split('.'):
                v = v[int(key)] if isinstance(v, list) else v[key]
            print(v)
elif name == 'launchctl':
    p = Path(os.environ['HOME']) / 'Library/LaunchAgents/com.cortexrnd.localflow.plist'
    with open(os.environ['CALLS'] + '.agents', 'a') as log:
        log.write(json.dumps([a[0], plistlib.loads(p.read_bytes())['ProgramArguments'][0] if p.exists() else None]) + '\n')
    if os.environ.get('FAIL_BOOTSTRAP_ONLY') and a[0] == 'bootstrap':
        sys.exit(5)
    if os.environ.get('FAIL_LAUNCH') and a[0] in ('bootstrap', 'load'):
        p = Path(a[-1])
        if p.exists() and str(plistlib.loads(p.read_bytes())['ProgramArguments'][0]).endswith('lf-hub'):
            print('denied by test session', file=sys.stderr)
            sys.exit(5)
elif name == 'cargo' and a[0] == 'build':
    p = Path(os.environ['CARGO_TARGET_DIR']) / 'release'
    p.mkdir(parents=True, exist_ok=True)
    for item in ('lf-hub', 'libsherpa-onnx-c-api.dylib', 'libonnxruntime.dylib'):
        if item == 'libonnxruntime.dylib' and os.environ.get('MISSING_ORT'):
            continue
        (p / item).write_text('native fixture')
elif name == 'otool':
    if a[0] == '-l':
        print('cmd LC_RPATH\n  path /build/native libs (offset 12)')
    else:
        dep = '/build/libsherpa-onnx-c-api.dylib' if Path(a[-1]).name == 'lf-hub' else '@rpath/libonnxruntime.dylib'
        if os.environ.get('BAD_DEP'):
            dep = '/opt/unbundled/libexternal.dylib'
        print(a[-1] + ':\n\t' + dep + ' (compatibility version 1.0.0, current version 1.0.0)')
elif name == 'codesign' and '--verify' in a and os.environ.get('FAIL_VERIFY'):
    print('invalid signature', file=sys.stderr)
    sys.exit(1)
"""


@pytest.fixture
def mac(tmp_path: Path):
    tools = tmp_path / "tools"
    tools.mkdir()
    for tool in (
        "uname",
        "plutil",
        "launchctl",
        "cargo",
        "otool",
        "install_name_tool",
        "codesign",
    ):
        p = tools / tool
        p.write_text(f"#!{sys.executable}\n" + TOOLS)
        p.chmod(0o755)
    home = tmp_path / "home & <test>"
    home.mkdir()
    env = dict(
        os.environ,
        HOME=str(home),
        CALLS=str(tmp_path / "calls"),
        PATH=str(tools) + os.pathsep + os.environ["PATH"],
        CARGO_TARGET_DIR=str(tmp_path / "target"),
    )
    for key in ("LOCALFLOW_RUNTIME", "LOCALFLOW_SKIP_LAUNCHCTL", "CARGO_BUILD_TARGET"):
        env.pop(key, None)
    return home, env


def executable(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("#!/bin/sh\nexit 0\n")
    path.chmod(0o755)
    return path


def helper(env, *args):
    return subprocess.run(
        [BASH, str(ROOT / "scripts/macos_launchagent.sh"), *map(str, args)],
        check=False,
        env=env,
        text=True,
        capture_output=True,
    )


def agent(home):
    return home / "Library/LaunchAgents" / f"{LABEL}.plist"


def read_agent(home):
    return plistlib.loads(agent(home).read_bytes())


def test_python_rust_python_round_trip_idempotent_and_ordered(mac):
    home, env = mac
    py = executable(home / "venv/bin/localflow")
    rust = executable(home / "Applications/LocalFlow.app/Contents/MacOS/lf-hub")
    assert helper(env, "restore-python", py).returncode == 0
    assert not agent(home).exists()  # Python's default still does not enable autostart.
    original = {
        "Label": LABEL,
        "ProgramArguments": [str(py)],
        "RunAtLoad": True,
        "EnvironmentVariables": {"USER_OPTION": "keep me"},
    }
    agent(home).parent.mkdir(parents=True)
    agent(home).write_bytes(plistlib.dumps(original))
    for _ in range(2):
        result = helper(env, "install-rust", rust)
        assert result.returncode == 0, result.stderr
        assert read_agent(home)["ProgramArguments"] == [str(rust)]
        assert read_agent(home)["KeepAlive"] == {"SuccessfulExit": False}
        assert (
            plistlib.loads(Path(str(agent(home)) + ".python.bak").read_bytes())
            == original
        )
    for _ in range(2):
        result = helper(env, "restore-python", py)
        assert result.returncode == 0, result.stderr
        assert read_agent(home) == original
    calls = [json.loads(line) for line in Path(env["CALLS"]).read_text().splitlines()]
    launches = [
        call[1] for call in calls if call[0] == "launchctl" and call[1] != "print"
    ]
    assert launches == ["bootout", "bootstrap"] * 2
    snapshots = [
        json.loads(line)
        for line in Path(env["CALLS"] + ".agents").read_text().splitlines()
    ]
    assert snapshots[0] == ["bootout", str(py)]
    assert snapshots[1] == ["bootstrap", str(rust)]
    assert snapshots[-2:] == [["bootout", str(rust)], ["bootstrap", str(py)]]
    assert Path(str(agent(home)) + ".rust.bak").is_file()


def test_fresh_rust_restore_without_backup_and_uninstall(mac):
    home, env = mac
    rust = executable(home / "Applications/LocalFlow.app/Contents/MacOS/lf-hub")
    py = executable(home / "venv/bin/localflow")
    assert helper(env, "install-rust", rust).returncode == 0
    assert helper(env, "restore-python", py).returncode == 0
    assert read_agent(home)["ProgramArguments"] == [str(py)]
    for _ in range(2):
        assert helper(env, "uninstall").returncode == 0
        assert not agent(home).exists()


def test_failed_load_reports_both_errors_and_rolls_back(mac):
    home, env = mac
    rust = executable(home / "Applications/LocalFlow.app/Contents/MacOS/lf-hub")
    py = executable(home / "venv/bin/localflow")
    agent(home).parent.mkdir(parents=True)
    original = plistlib.dumps({"Label": LABEL, "ProgramArguments": [str(py)]})
    agent(home).write_bytes(original)
    env["FAIL_LAUNCH"] = "1"
    result = helper(env, "install-rust", rust)
    assert result.returncode == 1
    assert "bootstrap failed: denied by test session" in result.stderr
    assert "load fallback also failed: denied by test session" in result.stderr
    assert agent(home).read_bytes() == original


def test_missing_executable_does_not_replace_agent(mac):
    home, env = mac
    result = helper(env, "install-rust", home / "missing")
    assert result.returncode != 0
    assert "Runtime executable is missing" in result.stderr
    assert not agent(home).exists()


def test_bootstrap_failure_falls_back_to_load(mac):
    home, env = mac
    env["FAIL_BOOTSTRAP_ONLY"] = "1"
    rust = executable(home / "Applications/LocalFlow.app/Contents/MacOS/lf-hub")
    result = helper(env, "install-rust", rust)
    assert result.returncode == 0, result.stderr
    assert read_agent(home)["ProgramArguments"] == [str(rust)]


def test_failed_fresh_install_removes_unloadable_agent(mac):
    home, env = mac
    env["FAIL_LAUNCH"] = "1"
    rust = executable(home / "Applications/LocalFlow.app/Contents/MacOS/lf-hub")
    result = helper(env, "install-rust", rust)
    assert result.returncode == 1
    assert not agent(home).exists()


@pytest.mark.parametrize("failure", [None, "MISSING_ORT", "BAD_DEP", "FAIL_VERIFY"])
def test_builder_bundles_rewrites_and_verifies_before_replacing(mac, failure):
    home, env = mac
    app = home / "Applications/LocalFlow.app"
    app.mkdir(parents=True)
    sentinel = app / "previous-app"
    sentinel.touch()
    if failure:
        env[failure] = "1"
    result = subprocess.run(
        [BASH, str(ROOT / "scripts/build_rust_app.sh"), str(app)],
        check=False,
        env=env,
        text=True,
        capture_output=True,
    )
    if failure:
        assert result.returncode != 0
        assert sentinel.exists()
        return
    assert result.returncode == 0, result.stderr
    assert not sentinel.exists()
    plist = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    assert plist["CFBundleIdentifier"] == "com.cortexrnd.localflow.app"
    assert plist["CFBundleExecutable"] == "lf-hub"
    assert plist["LSUIElement"] is False
    for key in (
        "NSMicrophoneUsageDescription",
        "NSAccessibilityUsageDescription",
        "NSInputMonitoringUsageDescription",
    ):
        assert plist[key]
    assert (app / "Contents/Frameworks/libonnxruntime.dylib").exists()
    assert (app / "Contents/Resources/macos_launchagent.sh").exists()
    calls = [json.loads(line) for line in Path(env["CALLS"]).read_text().splitlines()]
    assert any(
        call[:3]
        == ["install_name_tool", "-add_rpath", "@executable_path/../Frameworks"]
        for call in calls
    )
    assert any(
        call[:3] == ["install_name_tool", "-delete_rpath", "/build/native libs"]
        for call in calls
    )
    assert any(
        call[:5] == ["codesign", "--force", "--deep", "-s", "-"] for call in calls
    )
    assert any(call[:2] == ["codesign", "--verify"] for call in calls)


@pytest.mark.parametrize("runtime", ["rust", "python"])
def test_installer_uninstalls_both_app_layouts_without_deleting_settings(mac, runtime):
    home, env = mac
    exe = executable(
        home
        / "Applications/LocalFlow.app/Contents/MacOS"
        / ("lf-hub" if runtime == "rust" else "launcher")
    )
    agent(home).parent.mkdir(parents=True)
    agent(home).write_bytes(
        plistlib.dumps({"Label": LABEL, "ProgramArguments": [str(exe)]})
    )
    config = home / ".localflow/config.toml"
    config.parent.mkdir()
    config.write_text("user settings")
    env["LOCALFLOW_RUNTIME"] = runtime
    result = subprocess.run(
        [BASH, str(ROOT / "install.sh"), "--uninstall"],
        check=False,
        env=env,
        text=True,
        capture_output=True,
    )
    assert result.returncode == 0, result.stderr
    assert not exe.exists()
    assert not agent(home).exists()
    assert config.read_text() == "user settings"


@pytest.mark.parametrize("value", ["typo", "", "RUST"])
def test_invalid_runtime_rejected_before_any_install(mac, value):
    _home, env = mac
    env["LOCALFLOW_RUNTIME"] = value
    result = subprocess.run(
        [BASH, str(ROOT / "install.sh")], check=False, env=env, text=True, capture_output=True
    )
    assert result.returncode == 2
    assert "Invalid LOCALFLOW_RUNTIME" in result.stderr
    assert not Path(env["CALLS"]).exists()
