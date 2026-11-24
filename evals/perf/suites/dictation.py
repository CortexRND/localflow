"""``dictation`` suite: clip in, text out (``docs/plans/PLAN-perf-evals.md`` §5.2).

Stdlib only; never imports ``localflow``. Two case groups, each reporting
``ok`` / ``failed`` / ``not_exercised`` rather than going missing:

* ``text/<runtime>`` (every tier; cheap and deterministic, gates PRs): every
  case of ``tests/fixtures/dictation_corpus.jsonl`` through the dictation
  text pipeline (replacements -> spoken formatting -> hot phrases -> style),
  the same steps as ``tests/test_rust_parity.py::test_dictation_corpus_pipeline``.
  Python runs ``benches/py_dictation_text.py``; Rust drives the existing
  ``rust/lf/examples/parity`` binary with ``{"op": "render", ...}`` requests.
  ``parity`` reads one JSON array per process, so each timed pass is one
  process over ``repeat`` copies of the corpus and the median spawn cost of an
  empty batch is subtracted. Metrics: ``exact_match_rate`` (every output of
  every pass must equal the case's ``expected``), ``case_us`` (mean µs per
  case, one sample per pass) and ``corpus_ms`` (one corpus iteration, one
  sample per pass).
* ``http/<runtime>/<model>`` (nightly/deep; on smoke only with option
  ``include_http`` or ``LF_PERF_DICTATION_HTTP=1``; never with ``skip_http`` /
  ``LF_PERF_DICTATION_SKIP_HTTP=1``): starts ``lf serve`` or ``lf-rs serve``
  on a temp config (``[stt] model``, cleanup off, free port, isolated
  HOME/XDG), waits for ``/healthz``, then ``POST /transcribe?clean=0`` with
  every STT corpus clip as a multipart upload carrying ``X-Localflow-Token``.
  Metrics: ``startup_ms``, ``cold_first_request_ms`` (client wall of the very
  first request), ``server_ms`` / ``client_wall_ms`` (warm requests),
  ``error_rate`` (non-200 or not ``{"text": str, "ms": number}``) and ``wer``
  (corpus-level, scored by :mod:`evals.perf.wer`). The server is always
  terminated in ``finally``.

``RunContext.options`` (all optional): ``corpus`` (dictation JSONL),
``python``, ``parity_bin``, ``lf_rs_bin``, ``text_passes``, ``text_warmup``,
``text_repeat``, ``text_timeout_s``, ``include_http``, ``skip_http``,
``http_models`` (comma list or list), ``http_runs`` (passes over the clips),
``http_timeout_s`` (per request), ``startup_timeout_s``, ``stt_provider``,
``clips`` (list of ``{"id", "path", "reference"}`` or a JSON file holding
one, used instead of downloading the STT corpus), ``manifest``,
``cache_dir``.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from collections.abc import Callable
from pathlib import Path

from ..corpus.download import (
    DEFAULT_MANIFEST_PATH,
    default_cache_dir,
    ensure_stt_clips,
    load_stt_manifest,
)
from ..registry import RunContext, register_suite
from ..schema import compute_median, make_case, make_metric
from ..wer import normalize_text, word_edit_distance
from .footprint import (
    DEFAULT_STARTUP_TIMEOUT_S,
    HEALTH_POLL_S,
    HOST,
    MISSING_LF_RS_REASON,
    _exe,
    _find_binary,
    _stop,
    _tail,
    _truthy,
    find_lf_rs,
    free_port,
    health_ok,
    isolated_env,
)
from .stt import NIGHTLY_MODELS, SMOKE_MODELS

SUITE = "dictation"
REPO_CORPUS = Path("tests") / "fixtures" / "dictation_corpus.jsonl"
PY_TEXT_BENCH = Path(__file__).resolve().parents[1] / "benches" / "py_dictation_text.py"
EXIT_UNAVAILABLE = 3  # py_dictation_text.py: localflow not importable
DEFAULT_TEXT_TIMEOUT_S = 300.0
DEFAULT_HTTP_TIMEOUT_S = 600.0  # first request may download/load the model
MAX_EVIDENCE_MISMATCHES = 10

PARITY_BUILD_HINT = "cargo build --release -p lf --example parity"
MISSING_PARITY_REASON = (
    f"Rust parity example not found; build it with `{PARITY_BUILD_HINT}` (in rust/) or set LF_PERF_PARITY_BIN"
)
HTTP_SMOKE_REASON = (
    "HTTP dictation runs on nightly/deep; on smoke set LF_PERF_DICTATION_HTTP=1 or option include_http"
)
HTTP_SKIPPED_REASON = "HTTP dictation skipped (skip_http / LF_PERF_DICTATION_SKIP_HTTP=1)"


def find_parity_bin(repo_root: Path, override: str | None = None) -> Path | None:
    """``override`` / ``LF_PERF_PARITY_BIN``, else ``rust/target/{release,debug}/examples/parity``."""
    return _find_binary(repo_root, override, "LF_PERF_PARITY_BIN", ("examples", _exe("parity")))


def load_dictation_corpus(path: Path) -> list[dict]:
    cases = [json.loads(line) for line in Path(path).read_text(encoding="utf-8").splitlines() if line.strip()]
    if not cases:
        raise ValueError(f"{path}: empty dictation corpus")
    for idx, case in enumerate(cases):
        if not isinstance(case, dict) or not isinstance(case.get("input"), str) or not isinstance(
            case.get("expected"), str
        ):
            raise TypeError(f"{path}: case {idx} needs string 'input' and 'expected'")
    return cases


def render_request(case: dict) -> dict:
    """The ``parity`` ``render`` request for one corpus case (as in the parity test)."""
    overrides = case.get("config_overrides") or {}
    return {
        "op": "render",
        "text": case["input"],
        "phrases": [list(p) for p in case.get("phrases") or []],
        "replacements": case.get("replacements") or [],
        "formatting": bool(overrides.get("spoken_formatting", False)),
        "style": overrides.get("style"),
    }


# --------------------------------------------------------------------------
# Text pipeline
# --------------------------------------------------------------------------


def score_outputs(cases: list[dict], outputs: list[list[object]]) -> tuple[float, list[dict]]:
    """``(exact_match_rate, mismatches)``; a case matches only if every output equals ``expected``."""
    mismatches = []
    for case, outs in zip(cases, outputs):
        bad = [o for o in outs if o != case["expected"]] if outs else [None]
        if bad:
            mismatches.append({"name": case.get("name"), "expected": case["expected"], "actual": bad[0]})
    return (len(cases) - len(mismatches)) / len(cases), mismatches


def text_case(runtime: str, cases: list[dict], outputs: list[list[object]], case_us: list[float],
              evidence: dict) -> dict:
    rate, mismatches = score_outputs(cases, outputs)
    n = len(cases)
    metrics = {
        "exact_match_rate": make_metric(unit="ratio", better="higher", value=rate),
        "case_us": make_metric(unit="us", better="lower", samples=[round(x, 3) for x in case_us]),
        "corpus_ms": make_metric(unit="ms", better="lower", samples=[round(x * n / 1000.0, 4) for x in case_us]),
    }
    ev = {"n_cases": n, "mismatches": len(mismatches)}
    if mismatches:
        ev["mismatch_examples"] = mismatches[:MAX_EVIDENCE_MISMATCHES]
    ev.update(evidence)
    return make_case(suite=SUITE, case=f"text/{runtime}", dims=_text_dims(runtime), metrics=metrics, evidence=ev)


def _text_dims(runtime: str) -> dict:
    return {"mode": "text", "runtime": runtime}


def _text_non_ok(runtime: str, status: str, reason: str) -> dict:
    return make_case(suite=SUITE, case=f"text/{runtime}", dims=_text_dims(runtime), status=status, error=reason)


def _python(ctx: RunContext) -> str:
    return str(ctx.options.get("python") or os.environ.get("LF_PERF_PYTHON") or sys.executable)


def run_text_python(ctx: RunContext, corpus: Path, cases: list[dict], *, passes: int, warmup: int,
                    repeat: int, timeout: float) -> dict:
    with tempfile.TemporaryDirectory(prefix="lf-perf-dictation-text-") as tmp:
        root = Path(tmp)
        out = root / "text.json"
        cmd = [_python(ctx), str(PY_TEXT_BENCH), "--corpus", str(corpus), "--passes", str(passes),
               "--warmup", str(warmup), "--repeat", str(repeat), "--json", str(out)]
        env = isolated_env(root, repo_root=ctx.repo_root)
        try:
            proc = subprocess.run(cmd, cwd=str(ctx.repo_root), env=env, capture_output=True, text=True,
                                  encoding="utf-8", errors="replace", timeout=timeout, check=False)
        except subprocess.TimeoutExpired as exc:
            return _text_non_ok("python", "failed", f"text bench timed out after {timeout:g} s: {_tail(exc.stderr)}")
        except OSError as exc:
            return _text_non_ok("python", "failed", f"could not start text bench: {exc}")
        if proc.returncode == EXIT_UNAVAILABLE:
            return _text_non_ok("python", "not_exercised", _tail(proc.stderr) or "localflow not importable")
        if proc.returncode != 0:
            return _text_non_ok("python", "failed",
                                f"text bench exited {proc.returncode}: {_tail(proc.stderr) or '(no stderr)'}")
        try:
            report = json.loads(out.read_text(encoding="utf-8"))
            results = report["cases"]
            if len(results) != len(cases):
                raise ValueError(f"bench returned {len(results)} cases, corpus has {len(cases)}")
            outputs = [list(r["outputs"]) for r in results]
            reps = int(report["repeat"]) * len(cases)
            case_us = [ns / 1000.0 / reps for ns in report["pass_ns"]]
            if not case_us:
                raise ValueError("bench returned no timed passes")
        except (OSError, ValueError, TypeError, KeyError) as exc:
            return _text_non_ok("python", "failed", f"could not read text bench JSON: {exc}")
    return text_case("python", cases, outputs, case_us,
                     {"source": "py_dictation_text", "passes": passes, "warmup": warmup, "repeat": repeat})


class ParityError(RuntimeError):
    pass


def _drive_parity(binary: Path, requests: list[dict], *, env: dict, cwd: Path, timeout: float) -> tuple[list, float]:
    """``(responses, wall_ms)`` for one ``parity`` process over ``requests``."""
    payload = json.dumps(requests, ensure_ascii=False).encode("utf-8")
    start = time.perf_counter()
    try:
        proc = subprocess.run([str(binary)], input=payload, cwd=str(cwd), env=env, capture_output=True,
                              timeout=timeout, check=False)
    except subprocess.TimeoutExpired as exc:
        raise ParityError(f"parity timed out after {timeout:g} s: {_tail(exc.stderr)}".strip()) from exc
    except OSError as exc:
        raise ParityError(f"could not start parity: {exc}") from exc
    wall_ms = (time.perf_counter() - start) * 1000.0
    if proc.returncode != 0:
        raise ParityError(f"parity exited {proc.returncode}: {_tail(proc.stderr) or '(no stderr)'}")
    try:
        responses = json.loads(proc.stdout.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise ParityError(f"parity printed invalid JSON: {exc}: {_tail(proc.stdout)}") from exc
    if not isinstance(responses, list) or len(responses) != len(requests):
        got = len(responses) if isinstance(responses, list) else type(responses).__name__
        raise ParityError(f"parity returned {got} responses for {len(requests)} requests")
    return responses, wall_ms


def _response_text(response: object) -> object:
    """The rendered text of ``{"ok": str}``, else a marker dict that never matches ``expected``."""
    if isinstance(response, dict) and isinstance(response.get("ok"), str):
        return response["ok"]
    if isinstance(response, dict) and "error" in response:
        return {"error": str(response["error"])}
    return {"error": f"unexpected response {response!r}"[:200]}


def run_text_rust(ctx: RunContext, cases: list[dict], *, passes: int, warmup: int, repeat: int,
                  timeout: float, spawn_runs: int = 3) -> dict:
    binary = find_parity_bin(ctx.repo_root, ctx.options.get("parity_bin"))
    if binary is None:
        return _text_non_ok("rust", "not_exercised", MISSING_PARITY_REASON)
    n = len(cases)
    batch = [render_request(c) for c in cases] * repeat
    outputs: list[list[object]] = [[] for _ in cases]
    with tempfile.TemporaryDirectory(prefix="lf-perf-dictation-parity-") as tmp:
        root = Path(tmp)
        env = isolated_env(root)
        try:
            # Process spawn + JSON setup cost, subtracted from every timed pass.
            spawn_ms = compute_median([_drive_parity(binary, [], env=env, cwd=root, timeout=timeout)[1]
                                       for _ in range(max(1, spawn_runs))])
            case_us: list[float] = []
            for idx in range(warmup + passes):
                responses, wall_ms = _drive_parity(binary, batch, env=env, cwd=root, timeout=timeout)
                for pos, response in enumerate(responses):
                    text = _response_text(response)
                    bucket = outputs[pos % n]
                    if text not in bucket:
                        bucket.append(text)
                if idx >= warmup:
                    case_us.append(max(0.0, wall_ms - spawn_ms) * 1000.0 / (repeat * n))
        except ParityError as exc:
            return _text_non_ok("rust", "failed", str(exc))
    return text_case("rust", cases, outputs, case_us, {
        "source": "parity render", "binary": binary.name, "passes": passes, "warmup": warmup,
        "repeat": repeat, "spawn_ms": round(spawn_ms, 3),
    })


def run_text(ctx: RunContext) -> dict:
    opts = ctx.options
    corpus = Path(opts.get("corpus") or ctx.repo_root / REPO_CORPUS)
    try:
        cases = load_dictation_corpus(corpus)
    except (OSError, ValueError, TypeError) as exc:
        return _text_non_ok(ctx.runtime, "failed", f"dictation corpus unreadable: {exc}")
    smoke = ctx.tier == "smoke"
    passes = int(opts.get("text_passes", 5 if smoke else 10))
    warmup = int(opts.get("text_warmup", 1))
    timeout = float(opts.get("text_timeout_s", DEFAULT_TEXT_TIMEOUT_S))
    if ctx.runtime == "rust":
        repeat = int(opts.get("text_repeat", 50))
        return run_text_rust(ctx, cases, passes=passes, warmup=warmup, repeat=repeat, timeout=timeout)
    repeat = int(opts.get("text_repeat", 20))
    return run_text_python(ctx, corpus, cases, passes=passes, warmup=warmup, repeat=repeat, timeout=timeout)


# --------------------------------------------------------------------------
# HTTP end to end
# --------------------------------------------------------------------------


class ServerError(RuntimeError):
    pass


_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def encode_multipart(field: str, filename: str, data: bytes, content_type: str = "audio/wav") -> tuple[bytes, str]:
    """``(body, content_type_header)`` for one file field."""
    boundary = f"----lf-perf-{uuid.uuid4().hex}"
    head = (
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="{field}"; filename="{filename}"\r\n'
        f"Content-Type: {content_type}\r\n\r\n"
    ).encode()
    tail = f"\r\n--{boundary}--\r\n".encode()
    return head + data + tail, f"multipart/form-data; boundary={boundary}"


def post_transcribe(base_url: str, token: str, wav: bytes, *, filename: str, timeout: float) -> dict:
    """One ``POST /transcribe?clean=0``: ``{"ok", "wall_ms", "text", "ms", "error"}`` (never raises)."""
    body, ctype = encode_multipart("file", filename, wav)
    request = urllib.request.Request(
        f"{base_url}/transcribe?clean=0", data=body, method="POST",
        headers={"Content-Type": ctype, "X-Localflow-Token": token, "Content-Length": str(len(body))},
    )
    start = time.perf_counter()
    status, raw, error = None, b"", None
    try:
        with _OPENER.open(request, timeout=timeout) as response:
            status, raw = response.status, response.read()
    except urllib.error.HTTPError as exc:
        status = exc.code
        try:
            raw = exc.read()
        except OSError:
            raw = b""
        error = f"HTTP {exc.code}: {_tail(raw) or exc.reason}"
    except (urllib.error.URLError, OSError, ValueError) as exc:
        error = f"request failed: {exc}"
    wall_ms = (time.perf_counter() - start) * 1000.0
    result = {"ok": False, "wall_ms": wall_ms, "status": status, "text": None, "ms": None, "error": error}
    if error is not None:
        return result
    if status != 200:
        result["error"] = f"HTTP {status}"
        return result
    try:
        data = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        result["error"] = f"invalid JSON: {exc}: {_tail(raw)}"
        return result
    ms = data.get("ms") if isinstance(data, dict) else None
    if not (isinstance(data, dict) and isinstance(data.get("text"), str)
            and isinstance(ms, (int, float)) and not isinstance(ms, bool)):
        result["error"] = f"response is not {{'text': str, 'ms': number}}: {_tail(raw)}"
        return result
    result.update(ok=True, text=data["text"], ms=float(ms))
    return result


def http_config_text(port: int, model: str, provider: str | None = None) -> str:
    lines = [
        "config_version = 2",
        "[setup]",
        "version = 1",
        "[server]",
        f'host = "{HOST}"',
        f"port = {port}",
        "[stt]",
        f"model = {json.dumps(model)}",
    ]
    if provider:
        lines.append(f"provider = {json.dumps(provider)}")
    lines += ["[llm]", "cleanup_enabled = false", ""]
    return "\n".join(lines)


def token_candidates(root: Path) -> list[Path]:
    """Where either server looks for ``api-token`` under the isolated ``root``."""
    return [
        root / "config" / "localflow" / "api-token",  # XDG_CONFIG_HOME (Linux)
        root / "home" / "Library" / "Application Support" / "localflow" / "api-token",  # macOS
        root / "appdata" / "localflow" / "api-token",  # Windows roaming
        root / "localappdata" / "localflow" / "api-token",  # Windows local
    ]


def seed_token(root: Path) -> str:
    """Write one token to every candidate path (``lf-rs serve`` reads it without rotating)."""
    token = uuid.uuid4().hex + uuid.uuid4().hex
    for path in token_candidates(root):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(token + "\n", encoding="utf-8")
    return token


def read_token(root: Path) -> str | None:
    """The most recently written ``api-token`` under ``root`` (``lf serve`` rotates it on startup)."""
    found = []
    for path in root.rglob("api-token"):
        try:
            found.append((path.stat().st_mtime_ns, path.read_text(encoding="utf-8").strip()))
        except OSError:
            continue
    found = [f for f in found if f[1]]
    return max(found)[1] if found else None


def _model_cache_env(env: dict) -> dict:
    """Keep the caller's Hugging Face cache so models are not re-downloaded per temp HOME."""
    if not any(os.environ.get(k) for k in ("HF_HUB_CACHE", "HUGGINGFACE_HUB_CACHE", "HF_HOME")):
        base = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
        env["HF_HUB_CACHE"] = str(Path(base) / "huggingface" / "hub")
    return env


def server_command(ctx: RunContext, port: int) -> list[str] | str:
    """The server argv for ``port``, or a ``not_exercised`` reason."""
    if ctx.runtime == "rust":
        binary = find_lf_rs(ctx.repo_root, ctx.options.get("lf_rs_bin"))
        if binary is None:
            return MISSING_LF_RS_REASON
        return [str(binary), "serve", "--host", HOST, "--port", str(port)]
    return [_python(ctx), "-m", "localflow.cli", "serve"]


def wait_healthy(proc: subprocess.Popen, url: str, start: float, timeout: float) -> float:
    deadline = start + timeout
    while True:
        if health_ok(url, timeout=1.0):
            return (time.perf_counter() - start) * 1000.0
        code = proc.poll()
        if code is not None:
            raise ServerError(f"server exited {code} before /healthz was ok")
        if time.perf_counter() > deadline:
            raise ServerError(f"/healthz not ok after {timeout:g} s")
        time.sleep(HEALTH_POLL_S)


def _corpus_wer(pairs: list[tuple[str, str]]) -> float | None:
    edits = words = 0
    for reference, hypothesis in pairs:
        ref = normalize_text(reference)
        edits += word_edit_distance(ref, normalize_text(hypothesis))
        words += len(ref)
    if not pairs:
        return None
    return edits / words if words else (0.0 if edits == 0 else 1.0)


def _http_dims(runtime: str, model: str) -> dict:
    return {"mode": "http", "runtime": runtime, "model": model}


def _http_name(runtime: str, model: str) -> str:
    return f"http/{runtime}/{model}"


def _http_non_ok(runtime: str, model: str, status: str, reason: str) -> dict:
    return make_case(suite=SUITE, case=_http_name(runtime, model), dims=_http_dims(runtime, model),
                     status=status, error=reason)


def run_http_case(
    ctx: RunContext,
    model: str,
    clips: list[dict],
    *,
    command: Callable[[int], list[str] | str] | None = None,
    runs: int = 1,
    request_timeout: float = DEFAULT_HTTP_TIMEOUT_S,
    startup_timeout: float = DEFAULT_STARTUP_TIMEOUT_S,
) -> dict:
    """Start one server, transcribe every clip ``runs`` times, return one case."""
    runtime = ctx.runtime
    command = command or (lambda port: server_command(ctx, port))
    with tempfile.TemporaryDirectory(prefix="lf-perf-dictation-http-") as tmp:
        root = Path(tmp)
        port = free_port()
        cmd = command(port)
        if isinstance(cmd, str):
            return _http_non_ok(runtime, model, "not_exercised", cmd)
        config = root / "config" / "localflow" / "config.toml"
        env = _model_cache_env(isolated_env(root, repo_root=ctx.repo_root if runtime == "python" else None,
                                            config=config))
        # Both Rust lf-core::config::platform_config_base and Python platformdirs
        # honor WIN_PD_OVERRIDE_LOCAL_APPDATA on Windows so api-token stays under root/config/localflow.
        env["WIN_PD_OVERRIDE_LOCAL_APPDATA"] = str((root / "config").resolve())
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(http_config_text(port, model, ctx.options.get("stt_provider")), encoding="utf-8")
        seed_token(root)
        log = root / "server.log"
        base_url = f"http://{HOST}:{port}"
        with log.open("w", encoding="utf-8", errors="replace") as handle:
            start = time.perf_counter()
            try:
                proc = subprocess.Popen(cmd, cwd=str(ctx.repo_root), env=env, stdout=handle,
                                        stderr=subprocess.STDOUT)
            except OSError as exc:
                return _http_non_ok(runtime, model, "failed", f"could not start server: {exc}")
            try:
                startup_ms = wait_healthy(proc, f"{base_url}/healthz", start, startup_timeout)
                token = read_token(root)
                if not token:
                    raise ServerError("no api-token file found under the temp config dir")
                results = []
                for _ in range(max(1, runs)):
                    for clip in clips:
                        wav = Path(clip["path"]).read_bytes()
                        res = post_transcribe(base_url, token, wav, filename=f"{clip['id']}.wav",
                                              timeout=request_timeout)
                        res["clip"] = clip["id"]
                        results.append(res)
            except ServerError as exc:
                handle.flush()
                text = log.read_text(encoding="utf-8", errors="replace") if log.exists() else ""
                return _http_non_ok(runtime, model, "failed", f"{exc}: {_tail(text) or '(no output)'}")
            except OSError as exc:
                return _http_non_ok(runtime, model, "failed", f"could not read clip: {exc}")
            finally:
                _stop(proc)
    return http_case(runtime, model, clips, results, startup_ms)


def http_case(runtime: str, model: str, clips: list[dict], results: list[dict], startup_ms: float) -> dict:
    """Aggregate request results (first one is the cold request) into one case."""
    if not results:
        return _http_non_ok(runtime, model, "failed", "no clips to transcribe")
    cold, warm = results[0], results[1:]
    errors = [r for r in results if not r["ok"]]
    metrics = {
        "startup_ms": make_metric(unit="ms", better="lower", value=round(startup_ms, 3)),
        "cold_first_request_ms": make_metric(unit="ms", better="lower", value=round(cold["wall_ms"], 3)),
        "error_rate": make_metric(unit="ratio", better="lower", value=len(errors) / len(results)),
    }
    server = [r["ms"] for r in warm if r["ok"]]
    wall = [round(r["wall_ms"], 3) for r in warm if r["ok"]]
    if server:
        metrics["server_ms"] = make_metric(unit="ms", better="lower", samples=server)
    if wall:
        metrics["client_wall_ms"] = make_metric(unit="ms", better="lower", samples=wall)
    refs = {c["id"]: c["reference"] for c in clips}
    latest: dict[str, str] = {}
    for r in results:
        if r["ok"]:
            latest[r["clip"]] = r["text"]
    wer = _corpus_wer([(refs[cid], text) for cid, text in latest.items()])
    if wer is not None:
        metrics["wer"] = make_metric(unit="ratio", better="lower", value=wer)
    evidence = {
        "server": "lf-rs serve" if runtime == "rust" else "lf serve",
        "requests": len(results),
        "errors": len(errors),
        "cold_server_ms": cold["ms"],
        "transcripts": {cid: latest.get(cid) for cid in refs},
    }
    if errors:
        evidence["error_examples"] = [{"clip": r["clip"], "error": r["error"]} for r in errors[:5]]
    return make_case(suite=SUITE, case=_http_name(runtime, model), dims=_http_dims(runtime, model),
                     metrics=metrics, evidence=evidence)


def _list_option(raw: object) -> list[str]:
    items = raw.split(",") if isinstance(raw, str) else list(raw)  # type: ignore[arg-type]
    return [str(x).strip() for x in items if str(x).strip()]


def http_models(ctx: RunContext) -> list[str]:
    raw = ctx.options.get("http_models")
    if raw:
        return _list_option(raw)
    return list(SMOKE_MODELS if ctx.tier == "smoke" else NIGHTLY_MODELS)


def http_enabled(ctx: RunContext) -> str | None:
    """``None`` when HTTP should run, else the ``not_exercised`` reason."""
    opts = ctx.options
    if _truthy(opts.get("skip_http", os.environ.get("LF_PERF_DICTATION_SKIP_HTTP", ""))):
        return HTTP_SKIPPED_REASON
    if ctx.tier != "smoke":
        return None
    if _truthy(opts.get("include_http", os.environ.get("LF_PERF_DICTATION_HTTP", ""))):
        return None
    return HTTP_SMOKE_REASON


def resolve_clips(ctx: RunContext) -> list[dict]:
    """Clips from option ``clips`` (list or JSON file), else the pinned STT corpus."""
    raw = ctx.options.get("clips")
    if raw:
        data = json.loads(Path(raw).read_text(encoding="utf-8")) if isinstance(raw, (str, Path)) else raw
        if isinstance(data, dict):
            data = data.get("clips")
        if not isinstance(data, list) or not data:
            raise ValueError("option 'clips' needs a non-empty list of {id, path, reference}")
        for clip in data:
            if not all(isinstance(clip.get(k), str) and clip.get(k) for k in ("id", "path", "reference")):
                raise ValueError(f"bad clip entry {clip!r}: needs id, path and reference")
        return [dict(c) for c in data]
    manifest = load_stt_manifest(Path(ctx.options.get("manifest") or DEFAULT_MANIFEST_PATH))
    return ensure_stt_clips(manifest, Path(ctx.options.get("cache_dir") or default_cache_dir()),
                            smoke_only=ctx.tier == "smoke")


def run_http(ctx: RunContext) -> list[dict]:
    opts = ctx.options
    models = http_models(ctx)
    reason = http_enabled(ctx)
    if reason is None and server_command(ctx, 0) == MISSING_LF_RS_REASON:
        reason = MISSING_LF_RS_REASON
    if reason is not None:
        return [_http_non_ok(ctx.runtime, m, "not_exercised", reason) for m in models]
    try:
        clips = resolve_clips(ctx)
    except Exception as exc:  # noqa: BLE001 - network/ffmpeg/digest: report, never crash
        return [_http_non_ok(ctx.runtime, m, "failed", f"STT corpus unavailable: {type(exc).__name__}: {exc}")
                for m in models]
    runs = int(opts.get("http_runs", 1 if ctx.tier == "smoke" else 3))
    request_timeout = float(opts.get("http_timeout_s", DEFAULT_HTTP_TIMEOUT_S))
    startup_timeout = float(opts.get("startup_timeout_s", DEFAULT_STARTUP_TIMEOUT_S))
    return [run_http_case(ctx, m, clips, runs=runs, request_timeout=request_timeout,
                          startup_timeout=startup_timeout) for m in models]


# --------------------------------------------------------------------------
# Suite
# --------------------------------------------------------------------------


@register_suite(SUITE)
def run_dictation(ctx: RunContext) -> list[dict]:
    if ctx.runtime not in ("python", "rust"):
        raise ValueError(f"unknown runtime {ctx.runtime!r}")
    return [run_text(ctx), *run_http(ctx)]
