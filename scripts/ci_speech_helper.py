"""Measure the actual AppKit Hub, runtime and same-binary speech helper."""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

REFERENCE = (
    "Well, I don't wish to see it any more, observed Phebe, turning away her eyes. "
    "It is certainly very like the old portrait."
)


def validate(result, mode):
    assert result["transcript"] == REFERENCE, result["transcript"]
    rows = {row["stage"]: row for row in result["rows"]}
    assert set(rows) == {
        "idle",
        "loaded",
        "transcribed",
        "unloaded",
        "reloaded",
        "dictation_off",
    }
    for stage, row in rows.items():
        expected = mode == "helper" and stage in {"loaded", "transcribed", "reloaded"}
        assert bool(row["helper_pid"]) == expected, (mode, stage, row)
        assert (row["helper"] is not None) == expected
        for metric in ("rss", "footprint"):
            assert row["main"][metric] > 0
            assert row[f"total_{metric}"] == row["main"][metric] + (
                row["helper"][metric] if expected else 0
            )
    if mode == "helper":
        assert rows["loaded"]["helper_pid"] != rows["reloaded"]["helper_pid"]
        assert (
            rows["unloaded"]["main"]["footprint"] - rows["idle"]["main"]["footprint"]
            < 64 * 1024**2
        )


def run(root, output, mode):
    output.mkdir(parents=True, exist_ok=True)
    config = output / "config.toml"
    config.write_text(
        'config_version = 2\n[setup]\nversion = 1\n[stt]\nprovider = "parakeet-onnx"\nmodel = "parakeet"\ndevice = "cpu"\n'
    )
    config.with_name("runtime.toml").write_text("[stt]\nidle_timeout_seconds = 0\n")
    destination = output / "result.json"
    env = dict(
        os.environ,
        LOCALFLOW_CONFIG=str(config),
        LF_HUB_SPEECH_PROFILE=str(destination),
        LF_HUB_SPEECH_PCM=str(output.parent / "parakeet/clip.f32"),
    )
    env.pop("LOCALFLOW_SPEECH_IN_PROCESS", None)
    if mode == "inprocess":
        env["LOCALFLOW_SPEECH_IN_PROCESS"] = "1"
    with (output / "hub.log").open("w") as log:
        process = subprocess.Popen(
            [str(root / "rust/target/release/lf-hub"), "--page", "Home"],
            env=env,
            stdout=log,
            stderr=log,
        )
        try:
            deadline = time.monotonic() + 240
            while not destination.exists():
                error = destination.with_suffix(".error")
                if error.exists():
                    raise RuntimeError(error.read_text())
                assert process.poll() is None, (mode, "Hub exited", process.returncode)
                assert time.monotonic() < deadline, (mode, "measurement timeout")
                time.sleep(0.1)
            result = json.loads(destination.read_text())
            validate(result, mode)
            return result
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def report(results):
    lines = [
        "Memory is MiB; sums of process RSS double-count shared clean pages.",
        "",
        "| Mode / stage | Main RSS / footprint | Helper RSS / footprint | Total RSS / footprint |",
        "| --- | ---: | ---: | ---: |",
    ]

    def memory(value):
        return (
            "gone"
            if value is None
            else f"{value['rss'] / 1024**2:.1f} / {value['footprint'] / 1024**2:.1f}"
        )

    for mode, data in results.items():
        for row in data["rows"]:
            lines.append(
                f"| {mode} / {row['stage']} | {memory(row['main'])} | {memory(row['helper'])} | "
                f"{row['total_rss'] / 1024**2:.1f} / {row['total_footprint'] / 1024**2:.1f} |"
            )
    lines += [
        "",
        "| Mode | Cold spawn+load ms | Cold inference ms | Warm inference ms | Reload spawn+load ms | Reload inference ms |",
        "| --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    for mode, data in results.items():
        values = " | ".join(
            f"{data[key]:.1f}"
            for key in (
                "cold_load_ms",
                "cold_inference_ms",
                "warm_inference_ms",
                "reload_ms",
                "reload_inference_ms",
            )
        )
        lines.append(f"| {mode} | {values} |")
    lines += [
        "",
        "6c in-process int8 CPU reference (standalone benchmark, not AppKit Hub):",
        "",
        "| Stage | RSS MiB | Footprint MiB |",
        "| --- | ---: | ---: |",
        "| Before load | 13.9 | 4.8 |",
        "| Loaded | 1085.9 | 1074.5 |",
        "| Transcribed | 1140.2 | 1128.2 |",
        "| Unloaded | 704.3 | 486.9 |",
        "",
        "6c load 4580.2 ms; inference 1219.0 ms (single-run, different runner).",
        "",
        "All warm, cold and reload transcripts are identical to the fixed 6c reference:",
        "",
        REFERENCE,
    ]
    return "\n".join(lines) + "\n"


def main():
    root, output = (Path(arg).resolve() for arg in sys.argv[1:])
    results = {mode: run(root, output / mode, mode) for mode in ("helper", "inprocess")}
    text = report(results)
    (output / "speech-helper.md").write_text(text)
    print(text)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
            summary.write(text)


if __name__ == "__main__":
    main()
