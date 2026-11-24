"""Python dictation text-pipeline bench (counterpart to the Rust ``parity`` ``render`` op).

Runs every case of ``tests/fixtures/dictation_corpus.jsonl`` through
``localflow.hotphrases.render_dictation`` with the same steps as
``tests/test_rust_parity.py::test_dictation_corpus_pipeline`` (replacements ->
spoken formatting -> hot-phrase park -> style -> restore; no cleanup LLM, no
symbols), then times ``--passes`` passes of ``--repeat`` corpus iterations
after ``--warmup`` discarded ones and writes::

    {"cases": [{"name": .., "outputs": [str, ...]}],   # one output per pass (+ warmup)
     "pass_ns": [int, ...],                             # wall ns per timed pass
     "repeat": R, "n_cases": N}

``outputs`` holds every distinct rendering seen for the case (normally one);
the harness scores them against ``expected``. Timing covers only the
``render_dictation`` calls, not corpus loading or imports.

Exit codes: 0 ok, 2 bad arguments, 3 ``localflow`` could not be imported.

Benches may import ``localflow``; this file itself only needs the stdlib.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

EXIT_UNAVAILABLE = 3


def load_corpus(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def build_renderers(cases: list[dict]):
    """One zero-argument callable per case (closures built once, outside timing)."""
    from localflow import dictionary, formatting, hotphrases

    renderers = []
    for case in cases:
        overrides = case.get("config_overrides") or {}
        style = overrides.get("style")
        replacements = case.get("replacements") or []
        phrases = [tuple(p) for p in case.get("phrases") or []]
        fmt = bool(overrides.get("spoken_formatting", False))
        text = case["input"]

        def render(
            text=text, phrases=phrases, replacements=replacements, fmt=fmt, style=style,
        ) -> str:
            return hotphrases.render_dictation(
                text,
                phrases,
                replace=(lambda t: dictionary.apply_replacements(t, replacements)) if replacements else None,
                fmt=formatting.apply_spoken_formatting if fmt else None,
                clean=None,
                symbols=None,
                style=(lambda t: formatting.apply_style(t, style)) if style is not None else None,
            )

        renderers.append(render)
    return renderers


def run(cases: list[dict], *, passes: int, warmup: int, repeat: int) -> dict:
    renderers = build_renderers(cases)
    outputs: list[list[str]] = [[] for _ in cases]
    pass_ns: list[int] = []
    clock = time.perf_counter_ns
    for idx in range(warmup + passes):
        last: list[str] = [""] * len(renderers)
        start = clock()
        for _ in range(repeat):
            for i, render in enumerate(renderers):
                last[i] = render()
        elapsed = clock() - start
        for i, out in enumerate(last):
            if out not in outputs[i]:
                outputs[i].append(out)
        if idx >= warmup:
            pass_ns.append(elapsed)
    return {
        "cases": [{"name": c.get("name"), "outputs": outs} for c, outs in zip(cases, outputs)],
        "pass_ns": pass_ns,
        "repeat": repeat,
        "n_cases": len(cases),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--passes", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--repeat", type=int, default=20, help="corpus iterations per timed pass")
    parser.add_argument("--json", type=Path, required=True, help="output path")
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return int(exc.code or 0)
    if args.passes < 1 or args.warmup < 0 or args.repeat < 1:
        print("--passes and --repeat must be >= 1, --warmup >= 0", file=sys.stderr)
        return 2
    try:
        cases = load_corpus(args.corpus)
    except (OSError, ValueError) as exc:
        print(f"cannot read corpus {args.corpus}: {exc}", file=sys.stderr)
        return 2
    try:
        report = run(cases, passes=args.passes, warmup=args.warmup, repeat=args.repeat)
    except ImportError as exc:
        print(f"unavailable: localflow is not importable: {exc}", file=sys.stderr)
        return EXIT_UNAVAILABLE
    args.json.write_text(json.dumps(report, ensure_ascii=False), encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
