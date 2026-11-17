"""Exercise packaging and launchd migration using fake macOS tools off macOS."""

import json
import os
import plistlib
import shutil
import subprocess
import sys
import textwrap
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
    if a[0] == 'kickstart' and os.environ.get('PROCESSES'):
        if os.environ.get('FAIL_KICKSTART'):
            print('kickstart denied', file=sys.stderr)
            sys.exit(5)
        state = Path(os.environ['PROCESSES'])
        processes = json.loads(state.read_text())
        exe = plistlib.loads(p.read_bytes())['ProgramArguments'][0]
        processes['3333'] = exe if not os.environ.get('WRONG_PID_PATH') else exe + '.other'
        state.write_text(json.dumps(processes))
    if a[0] == 'print' and os.environ.get('PROCESSES') and not os.environ.get('NO_PID'):
        print('pid = 3333')
    if os.environ.get('FAIL_BOOTSTRAP_ONLY') and a[0] == 'bootstrap':
        sys.exit(5)
    if os.environ.get('FAIL_LAUNCH') and a[0] in ('bootstrap', 'load'):
        p = Path(a[-1])
        if p.exists() and str(plistlib.loads(p.read_bytes())['ProgramArguments'][0]).endswith('lf-hub'):
            print('denied by test session', file=sys.stderr)
            sys.exit(5)
elif name == 'ps':
    for pid, exe in json.loads(Path(os.environ['PROCESSES']).read_text()).items():
        print(pid, exe)
elif name == 'kill':
    if not os.environ.get('IGNORE_KILL') and not (a[0] == '-TERM' and os.environ.get('IGNORE_TERM')):
        state = Path(os.environ['PROCESSES'])
        processes = json.loads(state.read_text())
        processes.pop(a[1], None)
        state.write_text(json.dumps(processes))
elif name == 'cargo' and a[0] == 'build':
    p = Path(os.environ['CARGO_TARGET_DIR']) / 'release'
    p.mkdir(parents=True, exist_ok=True)
    for item in ('lf-hub', 'libsherpa-onnx-c-api.dylib', 'libonnxruntime.dylib'):
        if item == 'libonnxruntime.dylib' and os.environ.get('MISSING_ORT'):
            continue
        (p / item).write_text('native fixture')
elif name == 'otool':
    if a[0] == '-l':
        print('cmd LC_RPATH\n  path @loader_path (offset 12)\ncmd LC_RPATH\n  path /build/native libs (offset 12)')
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


@pytest.mark.parametrize("entrypoint", ["helper", "installer"])
def test_uninstall_removes_agent_before_launchctl_can_kill_caller(mac, entrypoint):
    home, env = mac
    agent(home).parent.mkdir(parents=True)
    agent(home).write_bytes(plistlib.dumps({"Label": LABEL, "ProgramArguments": ["lf-hub"]}))
    if entrypoint == "helper":
        result = helper(env, "uninstall")
    else:
        result = subprocess.run(
            [BASH, str(ROOT / "install.sh"), "--uninstall"],
            env=env, capture_output=True, text=True, check=False,
        )
    assert result.returncode == 0, result.stderr
    snapshots = [json.loads(line) for line in Path(env["CALLS"] + ".agents").read_text().splitlines()]
    assert snapshots == [["bootout", None]]


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


@pytest.fixture
def restart_mac(mac):
    home, env = mac
    tools = Path(env['PATH'].split(os.pathsep)[0])
    for tool in ('ps', 'kill', 'sleep'):
        path = tools / tool
        path.write_text(f'#!{sys.executable}\n' + TOOLS)
        path.chmod(0o755)
    bash_env = home / 'bash-env'
    bash_env.write_text('kill() { "$KILL_STUB" "$@"; }\n')
    env.update(BASH_ENV=str(bash_env), KILL_STUB=str(tools / 'kill'))
    rust = executable(home / 'Applications/LocalFlow.app/Contents/MacOS/lf-hub')
    state = home / 'processes.json'
    state.write_text(json.dumps({
        '1111': str(rust), '2222': str(rust),
        '4444': str(rust) + '.other',
        '5555': '/Applications/Other.app/Contents/MacOS/lf-hub',
    }))
    env['PROCESSES'] = str(state)
    agent(home).parent.mkdir(parents=True)
    agent(home).write_bytes(plistlib.dumps({'Label': LABEL, 'ProgramArguments': [str(rust)]}))
    return home, env, rust, state


@pytest.mark.parametrize('running,ignore_term', [(True, False), (False, False), (True, True)])
def test_restart_stops_external_main_and_helper_before_launching(restart_mac, running, ignore_term):
    _, env, rust, state = restart_mac
    if not running:
        state.write_text('{}')
    if ignore_term:
        env['IGNORE_TERM'] = '1'
    result = helper(env, 'restart-rust', rust)
    assert result.returncode == 0, result.stderr
    assert 'Restarted Rust LocalFlow (PID 3333)' in result.stdout
    processes = json.loads(state.read_text())
    assert '1111' not in processes and '2222' not in processes
    assert processes['3333'] == str(rust)
    calls = [json.loads(line) for line in Path(env['CALLS']).read_text().splitlines()]
    launches = [call[1] for call in calls if call[0] == 'launchctl']
    assert launches[:3] == ['bootout', 'bootstrap', 'kickstart']
    kills = [call for call in calls if call[0] == 'kill']
    if running:
        assert processes['4444'] == str(rust) + '.other'
        assert '5555' in processes
        assert [call[2] for call in kills if call[1] == '-TERM'] == ['1111', '2222']
        assert all(call[2] in ('1111', '2222') for call in kills)
        if ignore_term:
            first_kill = next(i for i, call in enumerate(calls) if call[:2] == ['kill', '-KILL'])
            assert sum(call == ['sleep', '1'] for call in calls[:first_kill]) == 75
        else:
            assert not any(call[:2] == ['kill', '-KILL'] for call in calls)
        assert calls.index(kills[-1]) < next(i for i, call in enumerate(calls) if call[:2] == ['launchctl', 'bootstrap'])
    else:
        assert not kills
    assert 'restart-rust "$HOME/Applications/LocalFlow.app/Contents/MacOS/lf-hub"' in (ROOT / 'install.sh').read_text(encoding='utf-8')


@pytest.mark.parametrize('mode,skip', [('install-rust', False), ('restart-rust', True)])
def test_restart_is_not_used_for_self_enabling_or_skip_launchctl(restart_mac, mode, skip):
    _, env, rust, state = restart_mac
    before = state.read_bytes()
    if skip:
        env['LOCALFLOW_SKIP_LAUNCHCTL'] = '1'
    result = helper(env, mode, rust)
    assert result.returncode == 0, result.stderr
    assert state.read_bytes() == before
    calls = [json.loads(line) for line in Path(env['CALLS']).read_text().splitlines()]
    assert not any(call[0] in ('ps', 'kill') for call in calls)
    assert not any(call[:2] == ['launchctl', 'kickstart'] for call in calls)


@pytest.mark.parametrize('failure', ['FAIL_KICKSTART', 'NO_PID', 'WRONG_PID_PATH', 'IGNORE_KILL'])
def test_restart_reports_failure_instead_of_claiming_success(restart_mac, failure):
    _, env, rust, _ = restart_mac
    env[failure] = '1'
    result = helper(env, 'restart-rust', rust)
    assert result.returncode != 0
    assert 'Restarted Rust LocalFlow' not in result.stdout
    if failure == 'FAIL_KICKSTART':
        assert 'kickstart denied' in result.stderr
    elif failure == 'IGNORE_KILL':
        assert 'Could not stop running Rust LocalFlow' in result.stderr
    else:
        assert 'did not start a new lf-hub process' in result.stderr


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
    assert plist["CFBundleIconFile"] == "LocalFlow.icns"
    assert (app / "Contents/Resources/LocalFlow.icns").read_bytes() == (ROOT / "packaging/macos/LocalFlow.icns").read_bytes()
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
        call[:3] == ["install_name_tool", "-delete_rpath", "@loader_path"]
        for call in calls
    )
    assert any(
        call[:8] == ["codesign", "--force", "-s", "-", "--identifier", plist["CFBundleIdentifier"],
                     "--requirements", '=designated => identifier "com.cortexrnd.localflow.app"']
        for call in calls
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


@pytest.mark.parametrize("present", [(), ("sherpa",), ("onnx",), ("sherpa", "onnx")])
def test_installer_cache_guard_reextracts_only_incomplete_sdk(tmp_path, present):
    workflow = (ROOT / ".github/workflows/installers.yml").read_text(encoding="utf-8")
    step = workflow.split("      - name: Restore complete sherpa SDK cache\n", 1)[1].split("\n      - ", 1)[0]
    script = textwrap.dedent(step.split("        run: |\n", 1)[1])
    cache = tmp_path / "rust/target/sherpa-onnx-prebuilt"
    sdk = cache / "sherpa-onnx-v2.0.1-osx-arm64-shared-lib"
    lib = sdk / "lib"
    lib.mkdir(parents=True)
    archive = cache / "sdk.tar.bz2"
    archive.touch()
    for kind, name in (("sherpa", "libsherpa-onnx-c-api.dylib"), ("onnx", "libonnxruntime.dylib")):
        if kind in present:
            (lib / name).touch()
    subprocess.run([BASH, "-eu", "-c", script], cwd=tmp_path, check=True)
    assert sdk.exists() == (len(present) == 2)
    assert archive.exists()
