"""``footprint`` suite: idle cost, startup time, memory and package size.

Stdlib only; never imports ``localflow`` (``docs/plans/PLAN-perf-evals.md``
§5.5). Three probe groups, each reporting ``ok`` / ``failed`` /
``not_exercised`` rather than going missing:

* ``idle``: ``idle/<runtime>``. Rust runs ``rust/lf-runtime/examples/idle``
  (Linux only; it reads ``/proc``) and parses its one line
  ``idle Ns: VmRSS X kB | VmHWM Y kB | threads T | cpu ticks C``; Python runs
  ``benches/py_idle.py``. Metrics: ``rss_bytes``, ``hwm_bytes``, ``threads``,
  ``cpu_seconds`` over an idle window of 10 s (smoke) or 30 s (nightly/deep).
* ``startup``: ``startup/<runtime>``. Spawns ``lf serve`` (Python, port set in
  a temp ``LOCALFLOW_CONFIG``: the Python command has no ``--port``) or
  ``lf-rs serve --host 127.0.0.1 --port P`` with an isolated temp
  HOME/XDG/config, polls ``GET /healthz`` until ``{"ok": true}`` and records
  ``startup_ms`` samples (3 runs in smoke, 5 otherwise, after one discarded
  warm-up). The server is always terminated in ``finally``.
* ``artifacts``: footprint adapters over CI helper outputs, each enabled by an
  option or env var (``speech_helper_json`` / ``LF_PERF_SPEECH_HELPER_JSON``,
  ``parakeet_memory_json`` / ``LF_PERF_PARAKEET_MEMORY_JSON``,
  ``hub_pages_csv`` / ``LF_PERF_HUB_PAGES_CSV``, ``hub_rss_txt`` /
  ``LF_PERF_HUB_RSS_TXT``, ``package_dir`` / ``LF_PERF_PACKAGE_DIR``). Unset
  inputs are omitted, or reported ``not_exercised`` when
  ``expect_artifacts`` / ``LF_PERF_EXPECT_ARTIFACTS=1`` (nightly).

``RunContext.options`` (all optional): ``os``, ``probes`` (comma list or list
of ``idle``, ``startup``, ``artifacts``), ``idle_seconds``, ``idle_bin``,
``lf_rs_bin``, ``python``, ``startup_runs``, ``startup_warmup``,
``startup_timeout_s``, ``expect_artifacts``, plus the artifact paths above.
"""

from __future__ import annotations

import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from pathlib import Path

from ..adapters import (
    load_hub_pages,
    load_hub_rss,
    load_parakeet_memory,
    load_speech_helper,
    measure_packages,
)
from ..registry import RunContext, register_suite
from ..schema import make_case, make_metric, normalize_os

SUITE = "footprint"
PROBES = ("idle", "startup", "artifacts")
PY_IDLE = Path(__file__).resolve().parents[1] / "benches" / "py_idle.py"
EXIT_UNAVAILABLE = 3  # py_idle.py: localflow not importable
ERROR_TAIL = 800
DEFAULT_CLK_TCK = 100
DEFAULT_STARTUP_TIMEOUT_S = 60.0
HEALTH_POLL_S = 0.02
HOST = "127.0.0.1"

IDLE_BUILD_HINT = "cargo build --release -p lf-runtime --example idle"
LF_RS_BUILD_HINT = "cargo build --release -p lf --bin lf-rs"
MISSING_IDLE_REASON = (
    f"Rust idle example not found; build it with `{IDLE_BUILD_HINT}` (in rust/) or set LF_PERF_IDLE_BIN"
)
MISSING_LF_RS_REASON = (
    f"lf-rs binary not found; build it with `{LF_RS_BUILD_HINT}` (in rust/) or set LF_PERF_LF_RS_BIN"
)
NON_LINUX_IDLE_REASON = "idle.rs reads /proc and only runs on Linux"

#: (option key, env var, adapter, label) for every artifact adapter.
ARTIFACTS: tuple[tuple[str, str, Callable[[Path], list[dict]], str], ...] = (
    ("speech_helper_json", "LF_PERF_SPEECH_HELPER_JSON", load_speech_helper, "speech_helper"),
    ("parakeet_memory_json", "LF_PERF_PARAKEET_MEMORY_JSON", load_parakeet_memory, "parakeet_memory"),
    ("hub_pages_csv", "LF_PERF_HUB_PAGES_CSV", load_hub_pages, "hub_pages"),
    ("hub_rss_txt", "LF_PERF_HUB_RSS_TXT", load_hub_rss, "hub_rss"),
    ("package_dir", "LF_PERF_PACKAGE_DIR", measure_packages, "package"),
)


def _tail(text: str | bytes | None) -> str:
    if isinstance(text, bytes):
        text = text.decode("utf-8", errors="replace")
    text = (text or "").strip()
    return text[-ERROR_TAIL:] if len(text) > ERROR_TAIL else text


def _truthy(value: object) -> bool:
    if isinstance(value, str):
        return value.strip().lower() in ("1", "true", "yes", "on")
    return bool(value)


def _exe(name: str) -> str:
    return f"{name}.exe" if sys.platform == "win32" else name


def _find_binary(repo_root: Path, override: str | None, env_var: str, rel: tuple[str, ...]) -> Path | None:
    explicit = override or os.environ.get(env_var)
    if explicit:
        path = Path(explicit)
        return path.resolve() if path.is_file() else None
    for profile in ("release", "debug"):
        path = repo_root.joinpath("rust", "target", profile, *rel)
        if path.is_file():
            return path
    return None


def find_idle_bin(repo_root: Path, override: str | None = None) -> Path | None:
    """``override`` / ``LF_PERF_IDLE_BIN``, else ``rust/target/{release,debug}/examples/idle``."""
    return _find_binary(repo_root, override, "LF_PERF_IDLE_BIN", ("examples", _exe("idle")))


def find_lf_rs(repo_root: Path, override: str | None = None) -> Path | None:
    """``override`` / ``LF_PERF_LF_RS_BIN``, else ``rust/target/{release,debug}/lf-rs``."""
    return _find_binary(repo_root, override, "LF_PERF_LF_RS_BIN", (_exe("lf-rs"),))


def isolated_env(root: Path, *, repo_root: Path | None = None, config: Path | None = None) -> dict:
    """``os.environ`` with HOME/XDG/config dirs under ``root`` (never the user's)."""
    env = dict(os.environ)
    dirs = {}
    for key, sub in (
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("APPDATA", "appdata"),
        ("LOCALAPPDATA", "localappdata"),
    ):
        path = root / sub
        path.mkdir(parents=True, exist_ok=True)
        dirs[key] = str(path)
    env.update(dirs)
    env["LOCALFLOW_CONFIG"] = str(config or root / "config" / "localflow" / "config.toml")
    env["NO_PROXY"] = env["no_proxy"] = ",".join(filter(None, (HOST, "localhost", env.get("NO_PROXY"))))
    env["PYTHONIOENCODING"] = "utf-8"
    env["PYTHONUTF8"] = "1"
    if repo_root is not None:  # Python children import localflow from this checkout
        py_entries = [str(repo_root)]
        if config is not None:
            # On Windows, platformdirs.user_config_dir uses SHGetFolderPathW and ignores
            # APPDATA/LOCALAPPDATA, so a child `lf serve` would overwrite the live api-token.
            # Only inject sitecustomize.py when an isolated config is requested (not idle_bare).
            iso_py = root / "py_isolate"
            iso_py.mkdir(parents=True, exist_ok=True)
            (iso_py / "sitecustomize.py").write_text(
                "import os, pathlib\n"
                "_cfg = os.environ.get('LOCALFLOW_CONFIG')\n"
                "if _cfg:\n"
                "    _dir = str(pathlib.Path(_cfg).parent)\n"
                "    try:\n"
                "        import platformdirs\n"
                "        platformdirs.user_config_dir = lambda *a, **kw: _dir\n"
                "    except Exception:\n"
                "        pass\n",
                encoding="utf-8",
            )
            py_entries.append(str(iso_py))
        if os.environ.get("PYTHONPATH"):
            py_entries.append(os.environ["PYTHONPATH"])
        env["PYTHONPATH"] = os.pathsep.join(py_entries)
    return env


# --------------------------------------------------------------------------
# Idle probe
# --------------------------------------------------------------------------

_IDLE_LINE = re.compile(
    r"idle\s+(?P<seconds>\d+)s:\s*VmRSS\s+(?P<rss>\S+)(?:\s+kB)?\s*\|\s*"
    r"VmHWM\s+(?P<hwm>\S+)(?:\s+kB)?\s*\|\s*threads\s+(?P<threads>\S+)\s*\|\s*"
    r"cpu ticks\s+(?P<ticks>\d+)"
)


def clock_ticks_per_second() -> int:
    try:
        value = os.sysconf("SC_CLK_TCK")
    except (AttributeError, ValueError, OSError):
        return DEFAULT_CLK_TCK
    return int(value) if value and value > 0 else DEFAULT_CLK_TCK


def parse_idle_output(text: str, clk_tck: int | None = None) -> dict:
    """Parse ``idle.rs`` stdout into ``{seconds, rss_bytes, hwm_bytes, threads, cpu_seconds}``.

    Raises ``ValueError`` when the line is missing or a field is ``n/a``.
    """
    match = None
    for line in text.splitlines():
        match = _IDLE_LINE.search(line) or match
    if match is None:
        raise ValueError("no 'idle Ns: VmRSS ... | cpu ticks N' line in output")
    try:
        rss_kb, hwm_kb, threads = int(match["rss"]), int(match["hwm"]), int(match["threads"])
    except ValueError as exc:
        raise ValueError(f"idle output has unreadable fields: {match.group(0)!r}") from exc
    tck = clk_tck or clock_ticks_per_second()
    return {
        "seconds": int(match["seconds"]),
        "rss_bytes": rss_kb * 1024,
        "hwm_bytes": hwm_kb * 1024,
        "threads": threads,
        "cpu_seconds": int(match["ticks"]) / tck,
        "cpu_ticks": int(match["ticks"]),
        "clk_tck": tck,
    }


def idle_case(runtime: str, data: dict, evidence: dict | None = None) -> dict:
    metrics = {
        "rss_bytes": make_metric(unit="bytes", better="lower", value=int(data["rss_bytes"])),
        "hwm_bytes": make_metric(unit="bytes", better="lower", value=int(data["hwm_bytes"])),
        "threads": make_metric(unit="count", better="lower", value=int(data["threads"])),
        "cpu_seconds": make_metric(unit="s", better="lower", value=float(data["cpu_seconds"])),
    }
    ev = {"idle_seconds": data.get("seconds")}
    ev.update(evidence or {})
    return make_case(suite=SUITE, case=f"idle/{runtime}", dims=_idle_dims(runtime), metrics=metrics, evidence=ev)


def _idle_dims(runtime: str) -> dict:
    return {"probe": "idle", "runtime": runtime}


def _idle_non_ok(runtime: str, status: str, reason: str) -> dict:
    return make_case(suite=SUITE, case=f"idle/{runtime}", dims=_idle_dims(runtime), status=status, error=reason)


def _run_child(cmd: list[str], *, cwd: Path, env: dict, timeout: float) -> subprocess.CompletedProcess | str:
    try:
        return subprocess.run(
            cmd, cwd=str(cwd), env=env, capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=timeout, check=False,
        )
    except subprocess.TimeoutExpired as exc:
        return f"timed out after {timeout:g} s: {_tail(exc.stderr)}".strip()
    except OSError as exc:
        return f"could not start: {exc}"


def run_idle(ctx: RunContext, os_name: str, seconds: int) -> dict:
    timeout = seconds + 120.0
    with tempfile.TemporaryDirectory(prefix="lf-perf-idle-") as tmp:
        root = Path(tmp)
        if ctx.runtime == "rust":
            if os_name != "linux":
                return _idle_non_ok("rust", "not_exercised", NON_LINUX_IDLE_REASON)
            binary = find_idle_bin(ctx.repo_root, ctx.options.get("idle_bin"))
            if binary is None:
                return _idle_non_ok("rust", "not_exercised", MISSING_IDLE_REASON)
            proc = _run_child([str(binary), str(seconds)], cwd=root, env=isolated_env(root), timeout=timeout)
            if isinstance(proc, str):
                return _idle_non_ok("rust", "failed", f"idle {proc}")
            if proc.returncode != 0:
                return _idle_non_ok("rust", "failed", f"idle exited {proc.returncode}: {_tail(proc.stderr) or '(no stderr)'}")
            try:
                data = parse_idle_output(proc.stdout)
            except ValueError as exc:
                return _idle_non_ok("rust", "failed", f"{exc}: {_tail(proc.stdout)}")
            return idle_case("rust", data, {"source": "idle.rs", "cpu_ticks": data["cpu_ticks"], "clk_tck": data["clk_tck"]})

        python = ctx.options.get("python") or os.environ.get("LF_PERF_PYTHON") or sys.executable
        out = root / "idle.json"
        cmd = [str(python), str(PY_IDLE), "--seconds", str(seconds), "--json", str(out)]
        if _truthy(ctx.options.get("idle_bare", False)):
            cmd.append("--bare")
        env = isolated_env(root, repo_root=ctx.repo_root)
        proc = _run_child(cmd, cwd=ctx.repo_root, env=env, timeout=timeout)
        if isinstance(proc, str):
            return _idle_non_ok("python", "failed", f"py_idle {proc}")
        if proc.returncode == EXIT_UNAVAILABLE:
            return _idle_non_ok("python", "not_exercised", _tail(proc.stderr) or "localflow not importable")
        if proc.returncode != 0:
            return _idle_non_ok("python", "failed", f"py_idle exited {proc.returncode}: {_tail(proc.stderr) or '(no stderr)'}")
        try:
            data = json.loads(out.read_text(encoding="utf-8"))
            missing = [k for k in ("rss_bytes", "hwm_bytes", "threads", "cpu_seconds") if data.get(k) is None]
            if missing:
                return _idle_non_ok("python", "failed", f"py_idle could not measure {', '.join(missing)}")
            return idle_case("python", data, {"source": f"py_idle/{data.get('source')}", "mode": data.get("mode")})
        except (OSError, ValueError, TypeError, KeyError) as exc:
            return _idle_non_ok("python", "failed", f"could not read py_idle JSON: {exc}")


# --------------------------------------------------------------------------
# Startup probe
# --------------------------------------------------------------------------


class StartupError(RuntimeError):
    pass


def free_port(host: str = HOST) -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind((host, 0))
        return int(sock.getsockname()[1])


_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def health_ok(url: str, timeout: float = 1.0) -> bool:
    """True when ``url`` answers 200 and is not JSON with a falsy ``ok``."""
    try:
        with _OPENER.open(url, timeout=timeout) as response:
            if response.status != 200:
                return False
            body = response.read()
    except (urllib.error.URLError, OSError, ValueError):
        return False
    try:
        data = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return True  # plain 200
    return not (isinstance(data, dict) and "ok" in data and data["ok"] is not True)


def _stop(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=10)


def time_to_healthy(cmd: list[str], *, port: int, env: dict, cwd: Path, log: Path, timeout: float) -> float:
    """Spawn ``cmd``, poll ``/healthz`` on ``port``; ms from spawn to healthy.

    Raises ``StartupError`` if the process exits or never becomes healthy.
    The process is always terminated before returning.
    """
    url = f"http://{HOST}:{port}/healthz"
    with log.open("w", encoding="utf-8", errors="replace") as handle:
        start = time.perf_counter()
        proc = subprocess.Popen(cmd, cwd=str(cwd), env=env, stdout=handle, stderr=subprocess.STDOUT)
        try:
            deadline = start + timeout
            while True:
                if health_ok(url, timeout=1.0):
                    return (time.perf_counter() - start) * 1000.0
                code = proc.poll()
                if code is not None:
                    raise StartupError(f"server exited {code} before /healthz was ok")
                if time.perf_counter() > deadline:
                    raise StartupError(f"/healthz not ok after {timeout:g} s")
                time.sleep(HEALTH_POLL_S)
        finally:
            _stop(proc)


def _config_text(port: int) -> str:
    return f'config_version = 2\n[setup]\nversion = 1\n[server]\nhost = "{HOST}"\nport = {port}\n'


def startup_command(ctx: RunContext, port: int) -> list[str] | str:
    """The server argv for ``port``, or a ``not_exercised`` reason."""
    if ctx.runtime == "rust":
        binary = find_lf_rs(ctx.repo_root, ctx.options.get("lf_rs_bin"))
        if binary is None:
            return MISSING_LF_RS_REASON
        return [str(binary), "serve", "--host", HOST, "--port", str(port)]
    python = ctx.options.get("python") or os.environ.get("LF_PERF_PYTHON") or sys.executable
    # `lf serve` takes host/port from the config file (no --host/--port flags).
    return [str(python), "-m", "localflow.cli", "serve"]


def measure_startup(
    command: Callable[[int], list[str] | str],
    *,
    runs: int,
    warmup: int,
    timeout: float,
    repo_root: Path | None = None,
    cwd: Path | None = None,
) -> list[float] | str:
    """Startup ms per timed run, or a ``not_exercised`` reason from ``command``.

    Every run gets a fresh free port, written to the temp config as well as
    passed to ``command``. Raises ``StartupError`` on a failed run.
    """
    samples: list[float] = []
    with tempfile.TemporaryDirectory(prefix="lf-perf-startup-") as tmp:
        root = Path(tmp)
        config = root / "config" / "localflow" / "config.toml"
        env = isolated_env(root, repo_root=repo_root, config=config)
        for idx in range(warmup + runs):
            port = free_port()
            cmd = command(port)
            if isinstance(cmd, str):
                return cmd
            config.parent.mkdir(parents=True, exist_ok=True)
            config.write_text(_config_text(port), encoding="utf-8")
            log = root / f"server-{idx}.log"
            try:
                ms = time_to_healthy(cmd, port=port, env=env, cwd=cwd or root, log=log, timeout=timeout)
            except StartupError as exc:
                text = log.read_text(encoding="utf-8", errors="replace") if log.exists() else ""
                raise StartupError(f"{exc}: {_tail(text) or '(no output)'}") from exc
            except OSError as exc:
                raise StartupError(f"could not start server: {exc}") from exc
            if idx >= warmup:
                samples.append(round(ms, 3))
    return samples


def run_startup(ctx: RunContext, runs: int) -> dict:
    runtime = ctx.runtime
    dims = {"probe": "startup", "runtime": runtime}
    name = f"startup/{runtime}"
    warmup = int(ctx.options.get("startup_warmup", 1))
    timeout = float(ctx.options.get("startup_timeout_s", DEFAULT_STARTUP_TIMEOUT_S))
    try:
        result = measure_startup(
            lambda port: startup_command(ctx, port),
            runs=runs,
            warmup=warmup,
            timeout=timeout,
            repo_root=ctx.repo_root if runtime == "python" else None,
            cwd=ctx.repo_root,
        )
    except StartupError as exc:
        msg = str(exc)
        if runtime == "python" and "ModuleNotFoundError:" in msg:
            return make_case(suite=SUITE, case=name, dims=dims, status="not_exercised", error=msg)
        return make_case(suite=SUITE, case=name, dims=dims, status="failed", error=msg)
    if isinstance(result, str):
        return make_case(suite=SUITE, case=name, dims=dims, status="not_exercised", error=result)
    server = "lf-rs serve" if runtime == "rust" else "lf serve"
    return make_case(
        suite=SUITE,
        case=name,
        dims=dims,
        metrics={"startup_ms": make_metric(unit="ms", better="lower", samples=result)},
        evidence={"server": server, "warmup": warmup, "health": "/healthz"},
    )


# --------------------------------------------------------------------------
# Artifact adapters
# --------------------------------------------------------------------------


def run_artifacts(ctx: RunContext) -> list[dict]:
    expect = _truthy(ctx.options.get("expect_artifacts", os.environ.get("LF_PERF_EXPECT_ARTIFACTS", "")))
    cases: list[dict] = []
    for key, env_var, adapter, label in ARTIFACTS:
        raw = ctx.options.get(key) or os.environ.get(env_var)
        if not raw:
            if expect:
                cases.append(
                    make_case(
                        suite=SUITE,
                        case=f"{label}/*",
                        dims={"component": label},
                        status="not_exercised",
                        error=f"no {label} artifact given (set {env_var} or option {key!r})",
                    )
                )
            continue
        try:
            cases.extend(adapter(Path(raw)))
        except Exception as exc:  # noqa: BLE001 - adapters should not raise; never crash the suite
            cases.append(
                make_case(
                    suite=SUITE, case=f"{label}/*", dims={"component": label},
                    status="failed", error=f"{label} adapter crashed: {type(exc).__name__}: {exc}",
                )
            )
    return cases


# --------------------------------------------------------------------------
# Suite
# --------------------------------------------------------------------------


def _probes(opts: dict) -> tuple[str, ...]:
    raw = opts.get("probes")
    if raw is None:
        return PROBES
    items = raw.split(",") if isinstance(raw, str) else list(raw)
    chosen = tuple(p.strip() for p in items if p and p.strip())
    unknown = [p for p in chosen if p not in PROBES]
    if unknown:
        raise ValueError(f"unknown footprint probes {unknown}; expected some of {PROBES}")
    return chosen


@register_suite(SUITE)
def run_footprint(ctx: RunContext) -> list[dict]:
    opts = ctx.options
    if ctx.runtime not in ("python", "rust"):
        raise ValueError(f"unknown runtime {ctx.runtime!r}")
    os_name = normalize_os(opts.get("os") or sys.platform)
    smoke = ctx.tier == "smoke"
    probes = _probes(opts)
    cases: list[dict] = []
    if "idle" in probes:
        seconds = int(opts.get("idle_seconds", 10 if smoke else 30))
        cases.append(run_idle(ctx, os_name, seconds))
    if "startup" in probes:
        runs = int(opts.get("startup_runs", 3 if smoke else 5))
        cases.append(run_startup(ctx, runs))
    if "artifacts" in probes:
        cases.extend(run_artifacts(ctx))
    return cases
