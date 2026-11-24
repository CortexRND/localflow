"""``meetings`` suite: meeting notes/chat quality gates and latency (``evals/meeting_sim``).

Stdlib only; never imports ``localflow`` (``docs/plans/PLAN-perf-evals.md`` §5.3).
Cases come from :mod:`evals.perf.adapters.meeting_gates`: ``meetings/gate/M1``
… ``M13`` (pooled rate + Wilson interval) and ``meetings/timings/<runtime>``
(``stop_to_notes_ms``, ``chat_ms``, ``llm_call_ms``, ``llm_calls_per_meeting``).

* **Rust**, existing run: ``options["rust_run_dir"]`` (env
  ``LF_PERF_MEETING_RUST_RUN_DIR``) is adapted like a Python run dir.
* **Rust**, live: the scripted-transcript seam is the eval-only example binary
  ``rust/lf-meetings-backend/examples/meeting_sim`` (nothing test-only ships in
  ``lf-rs``/``lf-hub``). It takes the same flags as ``driver.py`` and writes
  the same result JSON, so it is run through the same matrix and scored by
  the same ``gates.py``. The binary is ``options["meeting_sim_bin"]`` (env
  ``LF_PERF_MEETING_SIM_BIN``), else ``rust/target/{release,debug}/examples/
  meeting_sim``; build it with ``cargo build -p lf-meetings-backend --example
  meeting_sim``. A missing binary, Ollama/model, or an interpreter that cannot
  import ``gates.py``'s ``localflow`` helpers makes every case ``not_exercised``.
* **Python**, existing run: with ``options["run_dir"]`` (env
  ``LF_PERF_MEETING_RUN_DIR``) the run directory's ``gates.json`` and
  ``rK/results/*.json`` are adapted. A run dir without ``gates.json`` is
  scored with ``gates.py`` first.
* **Python**, live: needs Ollama at ``options["ollama_url"]`` (env
  ``LF_PERF_OLLAMA_URL``, default ``http://127.0.0.1:11434``) serving
  ``options["llm_model"]`` (env ``LF_PERF_OLLAMA_MODEL``, default
  ``llama3.2:3b``) and an interpreter with the product dependencies. Without
  them every case is ``not_exercised`` (CI without Ollama never fails). With
  them ``driver.py`` runs for every scenario × repeat, then ``gates.py``.

``RunContext.options`` (all optional): ``run_dir``, ``ollama_url``,
``llm_model``, ``driver_cmd`` (list or shell-style string run instead of
``python driver.py``; it gets the same ``--scenario/--out/--sut/--scenarios/
--llm-url/--llm-model/--cleanup`` flags and skips the Ollama check),
``python`` (env ``LF_PERF_PYTHON``), ``sut`` (env ``SIM_SUT``, default the
repo root), ``scenarios`` (list or comma string of ids; default all),
``repeats`` (default 1 on smoke, 3 on nightly/deep), ``output_dir`` (env
``LF_PERF_MEETING_OUT``; default a new temp dir, kept for inspection),
``timeout_s`` (per driver run, default 1800), ``ollama_timeout_s``; Rust
only: ``rust_run_dir``, ``meeting_sim_bin``.
"""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

from ..adapters.meeting_gates import failed_cases, load_run_dir, not_exercised_cases
from ..registry import RunContext, register_suite

SUITE = "meetings"
SIM_DIR = Path(__file__).resolve().parents[2] / "meeting_sim"
DRIVER = SIM_DIR / "driver.py"
GATES = SIM_DIR / "gates.py"
SCENARIOS = SIM_DIR / "scenarios.json"
DEFAULT_OLLAMA_URL = "http://127.0.0.1:11434"
DEFAULT_MODEL = "llama3.2:3b"
TIER_REPEATS = {"smoke": 1, "nightly": 3, "deep": 3}
DEFAULT_TIMEOUT_S = 1800
GATES_TIMEOUT_S = 300
OLLAMA_TIMEOUT_S = 2.0
ERROR_TAIL = 800
RUST_BIN_ENV = "LF_PERF_MEETING_SIM_BIN"
RUST_RUN_DIR_ENV = "LF_PERF_MEETING_RUST_RUN_DIR"
RUST_BUILD_HINT = "cargo build --manifest-path rust/Cargo.toml -p lf-meetings-backend --example meeting_sim"
RUST_REASON = f"Rust meeting_sim example binary not found (build it: `{RUST_BUILD_HINT}`, or set {RUST_BIN_ENV})"
#: Imported by driver.py / gates.py (checked with find_spec, nothing is executed).
PRODUCT_DEPS = ("numpy", "fastapi", "requests", "sounddevice", "platformdirs")
#: What gates.py imports from the checkout under test (all a Rust run needs from Python).
GATES_IMPORTS = ("localflow.meetings", "localflow.meeting_templates")


# --------------------------------------------------------------------------
# Environment checks
# --------------------------------------------------------------------------


def model_available(names: list[str], model: str) -> bool:
    """True if ``model`` is among Ollama's tag names (``x`` matches ``x:latest``)."""
    if model in names:
        return True
    if ":" not in model:
        return f"{model}:latest" in names
    base, _, tag = model.partition(":")
    return tag == "latest" and base in names


def ollama_status(url: str, model: str, *, timeout: float = OLLAMA_TIMEOUT_S) -> tuple[bool, str]:
    """``(ok, reason)``: is Ollama at ``url`` up and serving ``model``?"""
    tags = url.rstrip("/") + "/api/tags"
    try:
        with urllib.request.urlopen(tags, timeout=timeout) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except (OSError, ValueError, urllib.error.URLError) as exc:
        return False, f"Ollama not reachable at {url} ({type(exc).__name__}: {exc})"
    models = data.get("models") if isinstance(data, dict) else None
    if not isinstance(models, list):
        return False, f"Ollama at {url}: unexpected /api/tags response"
    names = [str(m.get("name") or m.get("model") or "") for m in models if isinstance(m, dict)]
    if not model_available(names, model):
        return False, f"Ollama model {model!r} not available at {url} (`ollama pull {model}`)"
    return True, "ok"


def missing_deps(python: str, sut: Path) -> list[str] | None:
    """Product modules ``python`` cannot find or initialize (``None`` if it cannot even run)."""
    code = (
        "import importlib.util as u\n"
        f"m = [n for n in {PRODUCT_DEPS!r} if u.find_spec(n) is None]\n"
        "if 'sounddevice' not in m:\n"
        "    try:\n"
        "        import sounddevice  # noqa: F401\n"
        "    except Exception:\n"
        "        m.append('sounddevice')\n"
        "print(','.join(m))\n"
    )
    try:
        proc = subprocess.run([python, "-c", code], capture_output=True, text=True, timeout=60,
                              cwd=str(sut), check=False)
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    return [m for m in proc.stdout.strip().split(",") if m]


def gates_import_error(python: str, sut: Path) -> str | None:
    """Why ``python`` cannot import ``gates.py``'s helpers from ``sut`` (``None`` if it can)."""
    code = f"import sys; sys.path.insert(0, {str(sut)!r})\nfor m in {GATES_IMPORTS!r}: __import__(m)\n"
    try:
        proc = subprocess.run([python, "-c", code], capture_output=True, text=True, timeout=60,
                              cwd=str(sut), check=False)
    except (OSError, subprocess.SubprocessError) as exc:
        return f"cannot run interpreter {python} ({exc})"
    if proc.returncode != 0:
        lines = (proc.stderr or proc.stdout).strip().splitlines()
        return f"{python} cannot import gates.py helpers from {sut}: {lines[-1] if lines else proc.returncode}"
    return None


def rust_sim_binary(ctx: RunContext) -> Path | None:
    """The ``meeting_sim`` example binary (option, env, then SUT release/debug build), or ``None``."""
    chosen = ctx.options.get("meeting_sim_bin") or os.environ.get(RUST_BIN_ENV)
    if chosen:
        path = Path(chosen).expanduser()
        return path.resolve() if path.is_file() else None
    exe = "meeting_sim.exe" if sys.platform == "win32" else "meeting_sim"
    sut = _sut(ctx)
    for profile in ("release", "debug"):
        path = sut / "rust" / "target" / profile / "examples" / exe
        if path.is_file():
            return path.resolve()
    return None


# --------------------------------------------------------------------------
# Matrix
# --------------------------------------------------------------------------


def plan_repeats(tier: str, override: object = None) -> int:
    if override not in (None, ""):
        n = int(override)  # type: ignore[call-overload]
        if n < 1:
            raise ValueError(f"meetings repeats must be >= 1, got {override!r}")
        return n
    if tier not in TIER_REPEATS:
        raise ValueError(f"unknown tier {tier!r}")
    return TIER_REPEATS[tier]


def load_scenarios(path: Path = SCENARIOS) -> list[dict]:
    with Path(path).open(encoding="utf-8") as fh:
        return list(json.load(fh)["scenarios"])


def plan_scenarios(all_scenarios: list[dict], override: object = None) -> list[dict]:
    """Scenarios to run (``override``: list or comma string of ids, default all)."""
    if override in (None, "", []):
        return list(all_scenarios)
    ids = override.split(",") if isinstance(override, str) else list(override)  # type: ignore[arg-type]
    ids = [str(x).strip() for x in ids if str(x).strip()]
    by_id = {s["id"]: s for s in all_scenarios}
    unknown = [x for x in ids if x not in by_id]
    if unknown:
        raise ValueError(f"unknown meeting scenario(s): {', '.join(unknown)}")
    return [by_id[x] for x in dict.fromkeys(ids)]


def _python(ctx: RunContext) -> str:
    return str(ctx.options.get("python") or os.environ.get("LF_PERF_PYTHON") or sys.executable)


def _sut(ctx: RunContext) -> Path:
    return Path(ctx.options.get("sut") or os.environ.get("SIM_SUT") or ctx.repo_root).expanduser().resolve()


def driver_command(ctx: RunContext) -> list[str]:
    custom = ctx.options.get("driver_cmd")
    if custom:
        return shlex.split(custom) if isinstance(custom, str) else [str(x) for x in custom]
    return [_python(ctx), str(DRIVER)]


def _tail(text: str | bytes | None) -> str:
    if isinstance(text, bytes):
        text = text.decode("utf-8", errors="replace")
    return (text or "").strip()[-ERROR_TAIL:]


def _env(sut: Path, url: str | None = None, model: str | None = None) -> dict:
    env = dict(os.environ)
    env.update(SIM_SUT=str(sut), PYTHONUTF8="1")
    if url:
        env["SIM_LLM_URL"] = url
    if model:
        env["SIM_LLM_MODEL"] = model
    return env


def run_drivers(ctx: RunContext, run_dir: Path, scenarios_file: Path, ids: list[str], repeats: int,
                *, sut: Path, url: str, model: str, timeout: float, base: list[str] | None = None) -> list[dict]:
    """Run the driver (``base``, default :func:`driver_command`) for every repeat × scenario.

    Returns the failed runs. A failed run leaves no result file, which
    ``gates.py`` counts against M1.
    """
    base = base or driver_command(ctx)
    env = _env(sut, url, model)
    failures: list[dict] = []
    with (run_dir / "matrix.log").open("a", encoding="utf-8") as log:
        for k in range(1, repeats + 1):
            # Created up front so gates.py sees the repeat even if every driver run in it fails.
            (run_dir / f"r{k}" / "results").mkdir(parents=True, exist_ok=True)
            for sid in ids:
                out = run_dir / f"r{k}" / "results" / f"{sid}.json"
                cmd = [*base, "--scenario", sid, "--out", str(out), "--sut", str(sut),
                       "--scenarios", str(scenarios_file), "--llm-url", url, "--llm-model", model, "--cleanup"]
                try:
                    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout,
                                          cwd=str(SIM_DIR), env=env, check=False)
                except subprocess.TimeoutExpired as exc:
                    log.write(f"NOT_EXERCISED r{k} {sid} timeout {timeout:g}s\n")
                    failures.append({"repeat": f"r{k}", "scenario": sid, "error": f"timeout after {timeout:g}s",
                                     "tail": _tail(exc.stderr)})
                    continue
                except OSError as exc:
                    failures.append({"repeat": f"r{k}", "scenario": sid, "error": f"cannot run driver: {exc}"})
                    continue
                log.write(proc.stdout + proc.stderr)
                if proc.returncode != 0 or not out.is_file():
                    log.write(f"NOT_EXERCISED r{k} {sid} exit {proc.returncode}\n")
                    failures.append({"repeat": f"r{k}", "scenario": sid, "error": f"exit {proc.returncode}",
                                     "tail": _tail(proc.stderr or proc.stdout)})
        log.write("MATRIX_DONE\n")
    return failures


def run_gates(ctx: RunContext, run_dir: Path, *, sut: Path, scenarios_file: Path = SCENARIOS) -> None:
    """Score ``run_dir`` with ``gates.py`` (writes ``run_dir/gates.json``); raises on failure."""
    cmd = [_python(ctx), str(GATES), str(run_dir), "--sut", str(sut), "--scenarios", str(scenarios_file),
           "--json", str(run_dir / "gates.json")]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=GATES_TIMEOUT_S,
                          cwd=str(SIM_DIR), env=_env(sut), check=False)
    if proc.returncode != 0:
        raise RuntimeError(f"gates.py exit {proc.returncode}: {_tail(proc.stderr or proc.stdout)}")


def _output_dir(opts: dict) -> Path:
    chosen = opts.get("output_dir") or os.environ.get("LF_PERF_MEETING_OUT")
    if chosen:
        path = Path(chosen).expanduser().resolve()
        path.mkdir(parents=True, exist_ok=True)
        return path
    return Path(tempfile.mkdtemp(prefix="lf-perf-meetings-")).resolve()


# --------------------------------------------------------------------------
# Entry points
# --------------------------------------------------------------------------


def adapt_run_dir(ctx: RunContext, run_dir: Path, evidence: dict | None = None) -> list[dict]:
    runtime = ctx.runtime
    if not run_dir.is_dir():
        return failed_cases(runtime, f"meeting run dir {run_dir} does not exist")
    ev = {"run_dir": str(run_dir), **(evidence or {})}
    if not (run_dir / "gates.json").is_file():
        if not any(run_dir.glob("r*/results/*.json")):
            return failed_cases(runtime, f"{run_dir}: no gates.json and no rK/results/*.json")
        scen = run_dir / "scenarios.json"
        try:
            run_gates(ctx, run_dir, sut=_sut(ctx), scenarios_file=scen if scen.is_file() else SCENARIOS)
        except (OSError, RuntimeError, subprocess.SubprocessError) as exc:
            cases = failed_cases(runtime, f"gates.py failed: {exc}", timings=False)
            return cases + load_run_dir(run_dir, runtime=runtime, evidence=ev)[-1:]
    return load_run_dir(run_dir, runtime=runtime, evidence=ev)


def _llm(opts: dict) -> tuple[str, str]:
    url = str(opts.get("ollama_url") or os.environ.get("LF_PERF_OLLAMA_URL") or DEFAULT_OLLAMA_URL)
    model = str(opts.get("llm_model") or os.environ.get("LF_PERF_OLLAMA_MODEL") or DEFAULT_MODEL)
    return url, model


def run_live(ctx: RunContext) -> list[dict]:
    opts = ctx.options
    runtime = ctx.runtime
    url, model = _llm(opts)
    sut = _sut(ctx)
    if not opts.get("driver_cmd"):
        ok, reason = ollama_status(url, model, timeout=float(opts.get("ollama_timeout_s", OLLAMA_TIMEOUT_S)))
        if not ok:
            return not_exercised_cases(runtime, reason)
        missing = missing_deps(_python(ctx), sut)
        if missing is None:
            return not_exercised_cases(runtime, f"cannot run interpreter {_python(ctx)}")
        if missing:
            return not_exercised_cases(
                runtime, f"{_python(ctx)} lacks meeting-sim dependencies: {', '.join(missing)}")
    return run_matrix(ctx, driver_command(ctx), url=url, model=model, sut=sut)


def run_rust(ctx: RunContext) -> list[dict]:
    """Rust: a recorded run dir, else the ``meeting_sim`` example binary (see module doc)."""
    opts = ctx.options
    run_dir = opts.get("rust_run_dir") or os.environ.get(RUST_RUN_DIR_ENV)
    if run_dir:
        return adapt_run_dir(ctx, Path(run_dir).expanduser().resolve())
    binary = rust_sim_binary(ctx)
    if binary is None:
        chosen = opts.get("meeting_sim_bin") or os.environ.get(RUST_BIN_ENV)
        reason = f"{RUST_BIN_ENV}={chosen} is not a file" if chosen else RUST_REASON
        return not_exercised_cases(ctx.runtime, reason)
    url, model = _llm(opts)
    sut = _sut(ctx)
    ok, reason = ollama_status(url, model, timeout=float(opts.get("ollama_timeout_s", OLLAMA_TIMEOUT_S)))
    if not ok:
        return not_exercised_cases(ctx.runtime, reason)
    error = gates_import_error(_python(ctx), sut)
    if error:
        return not_exercised_cases(ctx.runtime, error)
    return run_matrix(ctx, [str(binary)], url=url, model=model, sut=sut, evidence={"meeting_sim_bin": str(binary)})


def run_matrix(ctx: RunContext, base: list[str], *, url: str, model: str, sut: Path,
               evidence: dict | None = None) -> list[dict]:
    """Run ``base`` (driver.py or the Rust ``meeting_sim``) over the matrix, then score it."""
    opts = ctx.options
    runtime = ctx.runtime
    scenarios = plan_scenarios(load_scenarios(), opts.get("scenarios"))
    repeats = plan_repeats(ctx.tier, opts.get("repeats"))
    run_dir = _output_dir(opts)
    # Clear stale outputs if reusing an existing output directory so gates.py / adapt_run_dir
    # never score leftover rK/results/*.json or an old gates.json.
    for stale in (run_dir / "gates.json", run_dir / "matrix.log"):
        if stale.is_file() and not stale.is_symlink():
            stale.unlink()
    for stale_repeat in run_dir.glob("r*"):
        if stale_repeat.is_dir() and not stale_repeat.is_symlink() and stale_repeat.name[1:].isdigit():
            results_dir = stale_repeat / "results"
            if results_dir.is_dir() and not results_dir.is_symlink():
                for old_json in results_dir.glob("*.json"):
                    if not old_json.is_symlink():
                        old_json.unlink()
    # gates.py counts every scenario in the file, so a subset run gets its own file.
    scenarios_file = run_dir / "scenarios.json"
    scenarios_file.write_text(json.dumps({"scenarios": scenarios}, indent=1), encoding="utf-8")
    ids = [s["id"] for s in scenarios]
    failures = run_drivers(ctx, run_dir, scenarios_file, ids, repeats, sut=sut, url=url, model=model,
                           timeout=float(opts.get("timeout_s", DEFAULT_TIMEOUT_S)), base=base)
    if len(failures) == repeats * len(ids):
        first = failures[0]
        return failed_cases(runtime, f"every driver run failed (first: {first['scenario']} {first['error']}: "
                                     f"{first.get('tail', '')})".strip())
    ev = {"repeats": repeats, "scenarios": ids, "llm_model": model, "driver_failures": failures, **(evidence or {})}
    return adapt_run_dir(ctx, run_dir, ev)


@register_suite(SUITE)
def run_meetings(ctx: RunContext) -> list[dict]:
    if ctx.runtime == "rust":
        return run_rust(ctx)
    run_dir = ctx.options.get("run_dir") or os.environ.get("LF_PERF_MEETING_RUN_DIR")
    if run_dir:
        return adapt_run_dir(ctx, Path(run_dir).expanduser().resolve())
    return run_live(ctx)
