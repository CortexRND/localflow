from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def test_piped_installer_home_is_defined_at_step_scope() -> None:
    workflow = (ROOT / ".github" / "workflows" / "installers.yml").read_text(
        encoding="utf-8"
    )
    piped_job = workflow.split("\n  piped:\n", 1)[1]
    job_env = piped_job.split("\n    env:\n", 1)[1].split("\n    steps:\n", 1)[0]
    assert "runner." not in job_env

    for name in ("Install from piped shell script", "Install from piped PowerShell script"):
        step = piped_job.split(f"      - name: {name}\n", 1)[1].split("\n      - ", 1)[0]
        assert "\n        env:\n          LOCALFLOW_HOME: ${{ runner.temp }}/lfhome\n" in step


def test_linux_installer_jobs_prepare_headless_audio() -> None:
    workflow = (ROOT / ".github" / "workflows" / "installers.yml").read_text(
        encoding="utf-8"
    )
    for name in ("Prepare Linux audio", "Install Linux system dependencies"):
        step = workflow.split(f"      - name: {name}\n", 1)[1].split("\n      - ", 1)[0]
        assert "if: runner.os == 'Linux'" in step
        packages = next(
            line.split("sudo apt-get install -y ", 1)[1].split()
            for line in step.splitlines()
            if "sudo apt-get install -y " in line
        )
        assert "pulseaudio" in packages
        assert "pulseaudio --start --exit-idle-time=-1" in step


def test_rust_opt_in_job_checks_signature_agent_and_migration_round_trip() -> None:
    workflow = (ROOT / ".github/workflows/installers.yml").read_text(encoding="utf-8")
    job = workflow.split("\n  rust-macos:\n", 1)[1].split("\n  #", 1)[0]
    assert "runs-on: macos-latest" in job
    assert "LOCALFLOW_SKIP_LAUNCHCTL: '1'" in job
    assert job.count("LOCALFLOW_RUNTIME=rust ./install.sh --no-launch") == 2
    assert "LOCALFLOW_RUNTIME=python ./install.sh --no-launch" in job
    assert "codesign --verify --deep --strict" in job
    assert "plutil -extract ProgramArguments.0" in job
    assert 'cmp "$RUNNER_TEMP/python-agent.plist" "$agent"' in job
    assert 'cmp "$RUNNER_TEMP/python-agent.plist" "$agent.python.bak"' in job
    assert "DYLD_PRINT_LIBRARIES=1" in job
    assert 'otool -L "$app/Contents/MacOS/lf-hub"' in job
    for marker in (
        "PYTHON_INITIAL_AGENT",
        "RUST_FIRST_AGENT",
        "RUST_REPEAT_AGENT",
        "PYTHON_RESTORED_AGENT",
        "PYTHON_REPEAT_AGENT",
        "RUST_REINSTATED_AGENT",
    ):
        assert f"echo {marker}\n" in job
    assert "./install.sh --uninstall" in job


def test_installer_runtime_defaults_to_python_and_rejects_unknown_values() -> None:
    script = (ROOT / "install.sh").read_text(encoding="utf-8")
    assert 'RUNTIME="${LOCALFLOW_RUNTIME-python}"' in script
    assert "Invalid LOCALFLOW_RUNTIME" in script
    assert "python|rust)" in script
    assert 'if [[ "$RUNTIME" == python ]]; then' in script
    assert "bash scripts/build_rust_app.sh" in script
