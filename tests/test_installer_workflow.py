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
