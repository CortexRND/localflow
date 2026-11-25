import hmac
import html
import logging
import secrets
import subprocess
import threading
import time
from contextlib import contextmanager
from dataclasses import replace
from datetime import datetime
from pathlib import Path
from urllib.parse import parse_qs, urlparse

import numpy as np
from fastapi import FastAPI, HTTPException, Request, UploadFile
from fastapi.responses import HTMLResponse, JSONResponse
from pydantic import BaseModel, Field
from starlette.concurrency import run_in_threadpool

from localflow import api as api_logic
from localflow import recall as recall_logic
from localflow import sounds, stt_lease
from localflow.apitoken import load_or_create
from localflow.audio import rms_dbfs
from localflow.cleanup import Cleaner
from localflow.commands import discover_commands
from localflow.config import (
    Config,
    ConfigError,
    apply_config_update,
    config_to_dict,
    load_config,
    save_config,
)
from localflow.context import category_for
from localflow.diarization import models as diar_models
from localflow.dictionary import DictionaryStore, apply_replacements, stt_prompt
from localflow.dispatch import (
    APPROVE_FROM,
    REJECT_FROM,
    PromptQueue,
    PromptTransitionError,
    parse_work_prompts,
    transition_prompt,
    write_prompts,
)
from localflow.formatting import apply_spoken_formatting, apply_style
from localflow.history import HistoryStore
from localflow.hotphrases import HotPhraseStore, render_dictation
from localflow.live_config import (
    RUNTIME_FIELDS,
    ConfigWatcher,
    adopt_groups,
    merge_live,
    runtime_changes,
)
from localflow.log import SERVER_LOG_PATH, setup_logging
from localflow.logfiles import prepare_process_logs
from localflow.meeting_templates import load_templates, resolve_template
from localflow.meetings import (
    MeetingSession,
    MeetingSummarizer,
    MeetingWatcher,
    ObsidianWriter,
    answer_question,
    format_transcript,
    generate_title,
    is_default_title,
    latest_transcript_log,
    read_transcript_log,
    title_client,
)
from localflow.platform import current
from localflow.providers.factory import (
    build_fast_llm,
    build_llm,
    build_stt,
    build_workprompts_llm,
    fast_model_config,
)
from localflow.secrets import SecretsUnavailable
from localflow.stt import Transcriber
from localflow.symbols import apply_spoken_symbols
from localflow.system_audio import open_system_audio
from localflow.workprompts import WorkPromptGenerator

app = FastAPI()

setup_logging(SERVER_LOG_PATH)  # not localflow.log: dictation rotates that one
log = logging.getLogger("localflow.server")
_config = load_config()
_api_token = load_or_create()
_commands = discover_commands(_config.command_names, _config.skills_dirs)
_transcriber: Transcriber | None = None
_cleaner: Cleaner | None = None
# Re-entrant: _borrowed_transcriber holds it while _get_transcriber loads.
_lock = threading.RLock()
_config_lock = threading.Lock()
_transcribe_lock = threading.Lock()

# `_on_meeting_detected` is defined further down; forward it lazily.
_watcher = MeetingWatcher(
    min_busy_seconds=_config.meeting_min_busy_seconds,
    on_detect=lambda platform: _on_meeting_detected(platform),
)
# Meeting state is touched by HTTP handlers AND the watcher thread. Every
# read-modify-write of _session / _session_meta / _session_state goes through
# _session_lock; _session_state is the gate that makes check-then-act atomic
# across the slow, unlocked mic-open and summarize phases.
_session_lock = threading.Lock()
_session_state = "idle"  # idle | starting | running | stopping
_session: MeetingSession | None = None
_session_meta: dict = {}
_last_saved: dict | None = None
_ollama = build_llm(_config)
_summarizer = MeetingSummarizer(
    build_llm(_config, timeout=300, num_ctx=8192)
)
_writer = ObsidianWriter(
    _config.vault_path, _config.notes_folder, _config.logs_folder
)
_prompt_gen = WorkPromptGenerator(build_workprompts_llm(_config))
# One shared instance: PromptQueue's lock is per-instance, so two instances
# racing would be last-writer-wins over the whole queue file.
_queue = PromptQueue()
# Same shared-file pattern as the queue: the desktop app reads this file.
_hot_phrases = HotPhraseStore()
_dictionary = DictionaryStore()
_history: HistoryStore | None = None
# No queue lock here on purpose: PromptQueue serialises across processes itself,
# and status changes go through its compare-and-set. A lock here would only
# guard the server against the server.
# Speaker-label model download: only POST /api/diarization/download starts it,
# and only the shutdown hook sets the cancel event.
_diar_lock = threading.Lock()
_diar_download = {"state": "idle", "done_bytes": 0, "total_bytes": 0, "error": None}
_diar_cancel = threading.Event()
_diar_thread: threading.Thread | None = None  # the last worker started, under _diar_lock
_DIAR_SHUTDOWN_JOIN_SECONDS = 2.0


@app.on_event("startup")
def _start_watcher() -> None:
    global _api_token
    _api_token = load_or_create()
    if _config.meetings_enabled and _config.meeting_watch:
        _watcher.start()


@app.on_event("shutdown")
def _cancel_diarization_download() -> None:
    # Ask the worker to stop and give it a bounded moment to clean up. A read
    # stalled on the network (up to 30 s) can outlast the wait; any partial
    # file left behind is removed by a later explicit download once it is old.
    with _diar_lock:  # a POST holding the lock can't clear the event after this
        _diar_cancel.set()
        worker = _diar_thread
    if worker is not None:
        worker.join(timeout=_DIAR_SHUTDOWN_JOIN_SECONDS)  # not under _diar_lock

_STATIC_DIR = Path(__file__).parent / "static"


# Only the static shell is public: it holds no secrets and reads the token
# from its own URL. Everything else needs the token, whatever the client IP:
# a local reverse proxy (tailscale serve) makes every peer look like loopback.
_PUBLIC_PATHS = frozenset({"/", "/healthz", "/favicon.ico"})
_LOOPBACK_HOSTS = ("127.0.0.1", "localhost", "::1")
MAX_UPLOAD_BYTES = 25 * 1024 * 1024
FFMPEG_TIMEOUT_SECONDS = 120


# Single-use codes that let a browser open /settings or / without the API
# token ever appearing in a URL (process lists, browser history). The
# launcher POSTs the code from a private file; a code presented twice means
# someone else saw it, so the token is rotated.
LAUNCH_CODE_TTL_SECONDS = 60
_MAX_LAUNCH_CODES = 32
_MAX_LAUNCH_BODY_BYTES = 1024
_launch_codes: dict[str, float] = {}
_used_launch_codes: dict[str, float] = {}
_launch_lock = threading.Lock()
_LAUNCH_PATHS = frozenset({"/", "/settings"})
# Pages that may embed the token must not outlive the request in any cache.
_NO_STORE = {"Cache-Control": "no-store"}


def _prune(codes: dict[str, float], now: float) -> None:
    for stale in [c for c, expiry in codes.items() if expiry <= now]:
        del codes[stale]
    while len(codes) >= _MAX_LAUNCH_CODES:
        del codes[next(iter(codes))]


def _issue_launch_code() -> str:
    code = secrets.token_urlsafe(32)
    now = time.monotonic()
    with _launch_lock:
        _prune(_launch_codes, now)
        _launch_codes[code] = now + LAUNCH_CODE_TTL_SECONDS
    return code


def _redeem_launch_code(code: str | None) -> tuple[str, str]:
    """Return ("ok", token at redemption), ("reused", "") or ("invalid", "").

    "reused" means the code was already redeemed: treat it as theft. The token
    is read under the same lock rotation takes, so a redeemer never sees the
    replacement token.
    """
    if not code:
        return "invalid", ""
    now = time.monotonic()
    with _launch_lock:
        expiry = _launch_codes.pop(code, None)
        if expiry is not None and expiry > now:
            _prune(_used_launch_codes, now)
            _used_launch_codes[code] = now + LAUNCH_CODE_TTL_SECONDS
            return "ok", _api_token
        if _used_launch_codes.pop(code, 0.0) > now:
            return "reused", ""
    return "invalid", ""


def _rotate_api_token() -> None:
    global _api_token
    with _launch_lock:
        _launch_codes.clear()
        _used_launch_codes.clear()
        _api_token = load_or_create()
    log.warning("launch code presented twice; API token rotated")


async def _launch_code_from_form(request: Request) -> str | None:
    content_type = request.headers.get("Content-Type", "").split(";")[0].strip().lower()
    if content_type != "application/x-www-form-urlencoded":
        return None
    body = b""
    async for chunk in request.stream():
        body += chunk
        if len(body) > _MAX_LAUNCH_BODY_BYTES:
            return None
    values = parse_qs(body.decode("utf-8", "replace")).get("launch") or [None]
    return values[0]


def _token_ok(supplied: str | None) -> bool:
    return bool(supplied) and bool(_api_token) and hmac.compare_digest(
        supplied.encode(), _api_token.encode()
    )


def _host_is_loopback(host_header: str) -> bool:
    try:
        host = urlparse(f"//{host_header}").hostname
    except ValueError:
        return False
    return (host or "").lower() in _LOOPBACK_HOSTS


@app.middleware("http")
async def _api_auth(request: Request, call_next):
    path = request.scope["path"]
    # Pages embed the token this request proved, never a later (rotated) one.
    request.state.page_token = ""
    if request.method == "POST" and path in _LAUNCH_PATHS:
        result, token = _redeem_launch_code(await _launch_code_from_form(request))
        if result == "ok":
            request.state.page_token = token
            return await call_next(request)
        if result == "reused":
            _rotate_api_token()
        return JSONResponse({"detail": "launch link expired or already used"}, status_code=401)
    if path in _PUBLIC_PATHS:
        return await call_next(request)
    supplied = request.headers.get("X-Localflow-Token") or request.query_params.get("token")
    if not _token_ok(supplied):
        if not _host_is_loopback(request.headers.get("Host", "")):
            return JSONResponse({"detail": "invalid host"}, status_code=403)
        return JSONResponse({"detail": "invalid API token"}, status_code=401)
    request.state.page_token = supplied
    if path == "/transcribe":
        try:
            length = int(request.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        if length > MAX_UPLOAD_BYTES:
            return JSONResponse({"detail": "upload too large"}, status_code=413)
    return await call_next(request)


def _get_transcriber() -> Transcriber:
    current = _transcriber
    if current is not None:
        return current
    with _lock:
        return _load_transcriber_locked()


def _load_transcriber_locked() -> Transcriber:
    """The current speech model, loading it if needed. Caller holds _lock."""
    global _transcriber
    if _transcriber is None:
        # One config for lease, build and final lease: a reload may replace
        # _config while a slow load runs, and the lease must name what loaded.
        config = _config
        # Protect the selected model while it downloads/loads (an unknown or
        # automatic engine protects every cached model), until the real
        # lease below names what actually loaded.
        selection = stt_lease.record(
            stt_lease.selection_backend(config.stt_backend), config.model_size,
            scoped=True,
        )
        try:
            _transcriber = build_stt(config)
            loaded_size = getattr(_transcriber, "model_size", None) or config.model_size
            # What actually loaded (auto may have fallen back): Free up
            # space must never delete it while this process runs.
            stt_lease.record(_transcriber.backend, loaded_size)
        finally:
            if selection is not None:
                stt_lease.release(selection)
    return _transcriber


@contextmanager
def _borrowed_transcriber():
    """The current speech model for one transcription, leased until it ends.

    Taking the model and its scoped lease happen together under _lock, the
    lock a speech-model reload takes to drop the old model, so a reload can
    never retire (and Free up space never delete) a model that an in-flight
    phone /transcribe call is still using.
    """
    lease = None
    with _lock:
        transcriber = _get_transcriber()
        if isinstance(transcriber, Transcriber):
            lease = stt_lease.record(
                str(transcriber.backend), str(transcriber.model_size), scoped=True,
            )
    try:
        yield transcriber
    finally:
        if lease is not None:
            stt_lease.release(lease)


def _get_cleaner() -> Cleaner:
    global _cleaner
    if _cleaner is None:
        with _config_lock:
            if _cleaner is None:
                _cleaner = Cleaner(_ollama, build_fast_llm(_config, builder=build_llm))
    return _cleaner


def _get_history() -> HistoryStore:
    global _history
    if _history is None:
        _history = HistoryStore(enabled=_config.history_enabled)
    return _history


def _record_phone_history(
    audio: np.ndarray, raw_text: str, final_text: str, status: str
) -> None:
    try:
        _get_history().add(
            kind="dictate",
            app_name="phone",
            category="other",
            duration_s=len(audio) / 16000,
            raw_text=raw_text,
            final_text=final_text,
            status=status,
        )
    except Exception:
        log.exception("could not save server dictation history")


def _decode_audio(data: bytes) -> np.ndarray:
    try:
        proc = _run_ffmpeg(data, max_seconds=_config.max_recording_seconds)
    except FileNotFoundError:
        raise HTTPException(
            status_code=500, detail="ffmpeg not installed (brew install ffmpeg)"
        )
    except subprocess.TimeoutExpired:
        raise HTTPException(status_code=400, detail="audio decode timed out")
    if proc.returncode != 0 or not proc.stdout:
        stderr = proc.stderr.decode(errors="replace").strip()
        message = stderr.splitlines()[-1] if stderr else "ffmpeg failed to decode audio"
        raise HTTPException(status_code=400, detail=message[:300])
    return np.frombuffer(proc.stdout, dtype=np.float32)


# Demuxers for what browsers' MediaRecorder and iOS Shortcuts produce. Keeps
# playlist/concat demuxers (which open other URLs or files) out of reach.
_FFMPEG_INPUT_FORMATS = "matroska,webm,ogg,mov,mp4,m4a,wav,mp3,aac,flac,caf"


def _run_ffmpeg(data: bytes, max_seconds: int = 1200) -> subprocess.CompletedProcess:
    return subprocess.run(
        [
            "ffmpeg",
            "-nostdin",
            "-protocol_whitelist", "pipe",
            "-format_whitelist", _FFMPEG_INPUT_FORMATS,
            "-i", "pipe:0",
            "-t", str(max(1, int(max_seconds))),
            "-f", "f32le",
            "-ac", "1",
            "-ar", "16000",
            "pipe:1",
        ],
        input=data,
        capture_output=True,
        timeout=FFMPEG_TIMEOUT_SECONDS,
        check=False,
    )


def _transcribe_sync(audio: np.ndarray, clean: bool) -> str:
    if rms_dbfs(audio) < _config.silence_threshold_dbfs:
        log.info("silence gate: audio below %.1f dBFS", _config.silence_threshold_dbfs)
        _record_phone_history(audio, "", "", "silent")
        return ""
    words = _dictionary.words()
    replacements = _dictionary.replacements()
    raw_text = ""
    try:
        with _borrowed_transcriber() as transcriber:
            raw_text = transcriber.transcribe(audio, prompt=stt_prompt(words))
        if not raw_text:
            _record_phone_history(audio, raw_text, "", "silent")
            return raw_text
        phrases = _hot_phrases.enabled() if _config.hot_phrases else []
        category = category_for(None, _config.app_category_overrides)
        style_name = (
            getattr(_config, f"style_{category}") if _config.styles_enabled else None
        )
        level = _config.cleanup_level if _config.cleanup_enabled else "none"
        should_clean = clean and level != "none"
        clean_step = (
            lambda value: _get_cleaner().clean(
                value,
                level=level,
                style=style_name,
                vocabulary=words,
            )
        ) if should_clean else None
        style = (
            (lambda value: apply_style(value, style_name, words))
            if style_name
            else None
        )
        final_text = render_dictation(
            raw_text,
            phrases,
            replace=lambda value: apply_replacements(value, replacements),
            fmt=apply_spoken_formatting if _config.spoken_punctuation else None,
            clean=clean_step,
            symbols=(lambda t: apply_spoken_symbols(t, _commands))
            if _config.spoken_symbols
            else None,
            style=style,
        )
    except Exception:
        _record_phone_history(audio, raw_text, "", "failed")
        raise
    _record_phone_history(
        audio,
        raw_text,
        final_text,
        "ok" if final_text else "silent",
    )
    return final_text


@app.get("/healthz")
def healthz() -> dict:
    return {"ok": True}


@app.api_route("/", methods=["GET", "POST"])
def index(request: Request) -> HTMLResponse:
    page = (_STATIC_DIR / "index.html").read_text(encoding="utf-8")
    return HTMLResponse(
        page.replace("__LOCALFLOW_LAUNCH_TOKEN__", html.escape(request.state.page_token)),
        headers=_NO_STORE,
    )


@app.api_route("/settings", methods=["GET", "POST"])
def settings(request: Request) -> HTMLResponse:
    page = (_STATIC_DIR / "settings.html").read_text(encoding="utf-8")
    return HTMLResponse(
        page.replace("__LOCALFLOW_TOKEN__", html.escape(request.state.page_token)),
        headers=_NO_STORE,
    )


@app.post("/api/launch-code")
def api_launch_code() -> dict:
    return {"code": _issue_launch_code(), "expires_in": LAUNCH_CODE_TTL_SECONDS}


@app.get("/api/status")
def api_status() -> dict:
    return api_logic.status(_config, current(), _transcriber is not None)


@app.get("/api/config")
def api_config() -> dict:
    return config_to_dict(_config)


@app.put("/api/config")
def api_config_update(body: dict) -> dict:
    global _config, _transcriber, _cleaner, _ollama, _commands, _history, _summarizer
    with _config_lock:
        try:
            updated = apply_config_update(_config, body)
            new_llm = api_logic.validate_runtime(updated)
        except ConfigError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        commands_changed = (
            updated.command_names != _config.command_names
            or updated.skills_dirs != _config.skills_dirs
        )
        save_config(updated)
        _config = updated
        if commands_changed:
            _commands = discover_commands(_config.command_names, _config.skills_dirs)
        _transcriber = None
        _cleaner = None
        _ollama = new_llm
        _summarizer = MeetingSummarizer(build_llm(_config, timeout=300, num_ctx=8192))
        _history = None
        result = config_to_dict(_config)
    return {**result, "restart_required": api_logic.restart_required(body)}


def _diar_snapshot() -> dict:
    with _diar_lock:
        return dict(_diar_download)


def _diar_progress(done: int, total: int) -> None:
    with _diar_lock:
        _diar_download.update(done_bytes=done, total_bytes=total)


def _diar_worker() -> None:
    from localflow.models import DownloadCancelled, friendly_download_error

    state, error = "failed", "Download failed."
    try:
        diar_models.download_diarization_models(_diar_progress, _diar_cancel)
        state, error = "idle", None
    except DownloadCancelled:
        state, error = "cancelled", None
    except Exception as exc:  # noqa: BLE001
        error = friendly_download_error(exc)
    finally:  # never left "downloading", whatever was raised
        with _diar_lock:
            _diar_download.update(state=state, error=error)


@app.get("/api/diarization/status")
def api_diarization_status() -> dict:
    # Plain def: FastAPI runs it in the threadpool, so hashing never blocks the loop.
    # Snapshot the download first: a download finishing while the files are
    # hashed then reads "downloading" (poll again), never "idle" and not ready.
    download = _diar_snapshot()
    return {
        **diar_models.status(),
        **diar_models.flags(_config),
        "download": download,
    }


@app.post("/api/diarization/download", status_code=202)
def api_diarization_download() -> dict:
    global _diar_thread
    with _diar_lock:
        if _diar_download["state"] == "downloading":
            return {"started": False, "download": dict(_diar_download)}
        _diar_download.update(
            state="downloading",
            done_bytes=0,
            total_bytes=sum(model.size_bytes for model in diar_models.DIAR_MODELS),
            error=None,
        )
        _diar_cancel.clear()
        snapshot = dict(_diar_download)
        try:
            worker = threading.Thread(
                target=_diar_worker, name="diarization-download", daemon=True
            )
            worker.start()
        except Exception as exc:  # noqa: BLE001 - e.g. RuntimeError: can't start new thread
            log.warning("Could not start the speaker model download: %s", exc)
            message = "Could not start the speaker model download. Try again."
            _diar_download.update(state="failed", error=message)
            raise HTTPException(status_code=503, detail=message) from None
        _diar_thread = worker
    return {"started": True, "download": snapshot}


@app.get("/api/providers/stt")
def api_stt_providers() -> list[dict]:
    return api_logic.stt_providers()


@app.get("/api/providers/llm")
def api_llm_providers() -> list[dict]:
    return api_logic.llm_providers()


@app.get("/api/models")
def api_models(
    kind: str,
    provider: str,
    base_url: str | None = None,
) -> dict:
    return api_logic.models(_config, kind, provider, base_url)


@app.post("/api/test/llm")
def api_test_llm(body: dict | None = None) -> dict:
    body = body or {}
    return api_logic.test_llm(
        _config,
        body.get("provider"),
        body.get("base_url"),
        body.get("model"),
    )


@app.get("/api/secrets")
def api_secrets() -> dict[str, bool]:
    return api_logic.secrets_status(_config)


@app.put("/api/secrets/{name}")
def api_secret_update(name: str, body: dict) -> dict[str, bool]:
    global _ollama, _cleaner, _prompt_gen, _summarizer
    if name not in api_logic.SECRET_NAMES:
        raise HTTPException(status_code=404, detail="unknown secret")
    value = body.get("value", "")
    if not isinstance(value, str):
        raise HTTPException(status_code=400, detail="value must be a string")
    with _config_lock:
        try:
            # An activated config's key is rotated in its own endpoint-bound
            # slot; the generic llm_api_key stays with legacy configs.
            api_logic.update_secret(name, value, _config)
            if name == "llm_api_key":
                _ollama = build_llm(_config)
                _summarizer = MeetingSummarizer(build_llm(_config, timeout=300, num_ctx=8192))
                _cleaner = None
            elif name == "fireworks_api_key":
                _prompt_gen = WorkPromptGenerator(build_workprompts_llm(_config))
        except api_logic.InvalidKeyRef as exc:  # raised before any write
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        except SecretsUnavailable as exc:
            raise HTTPException(status_code=503, detail=str(exc)) from exc
    return api_logic.secrets_status(_config)


@app.get("/api/autostart")
def api_autostart() -> dict[str, str]:
    try:
        status = current().autostart_status()
    except (RuntimeError, OSError):
        status = "unavailable"
    return {"status": status}


@app.post("/api/autostart")
def api_autostart_update(body: dict) -> dict[str, str]:
    enabled = body.get("enabled")
    if type(enabled) is not bool:
        raise HTTPException(status_code=400, detail="enabled must be a boolean")
    try:
        if enabled:
            current().autostart_install()
        else:
            current().autostart_uninstall()
    except (RuntimeError, OSError) as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc
    try:
        status = current().autostart_status()
    except (RuntimeError, OSError) as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc
    return {"status": status}


@app.post("/transcribe")
async def transcribe(file: UploadFile, clean: int = 0) -> dict:
    start = time.monotonic()
    data = await file.read(MAX_UPLOAD_BYTES + 1)
    if len(data) > MAX_UPLOAD_BYTES:
        raise HTTPException(status_code=413, detail="upload too large")
    audio = await run_in_threadpool(_decode_audio, data)
    if audio.size == 0:
        raise HTTPException(status_code=400, detail="decoded audio is empty")
    text = await run_in_threadpool(_transcribe_sync, audio, bool(clean))
    elapsed_ms = int((time.monotonic() - start) * 1000)
    return {"text": text, "ms": elapsed_ms}


MAX_USER_NOTES_CHARS = 100_000
MAX_CHAT_QUESTION_CHARS = 2_000


MAX_TITLE_CHARS = 200


class MeetingStart(BaseModel):
    title: str = Field("Meeting", max_length=MAX_TITLE_CHARS)
    category: str = ""
    template: str = ""     # template id; empty = [meetings] default_template
    user_notes: str = ""   # notes typed before pressing start
    # True: name the notes automatically on save; False: the user typed the
    # title (kept as is, even "Meeting"); omitted: decided from the title
    # (empty or "Meeting" = automatic), as for older callers.
    auto_title: bool | None = None


class MeetingNotes(BaseModel):
    notes: str


class MeetingChat(BaseModel):
    question: str


def _templates() -> dict:
    return load_templates(_config.meeting_templates_dir)


@app.get("/meeting/templates")
def meeting_templates() -> dict:
    templates = _templates()
    default = _config.meeting_default_template
    if default not in templates:
        default = resolve_template(None, _config.meeting_templates_dir).id
    return {
        "default": default,
        "templates": [
            {"id": t.id, "name": t.name, "description": t.description,
             "sections": t.section_names()}
            for t in templates.values()
        ],
    }


@app.get("/meeting/status")
def meeting_status() -> dict:
    with _session_lock:  # snapshot so session and its meta can't disagree
        session, meta, last_saved = _session, dict(_session_meta), _last_saved
    live = None
    if session is not None and session.active:
        live = {
            "title": meta.get("title", "Meeting"),
            # True while the title is a stand-in that is named on save.
            "title_auto": bool(meta.get("title_auto")),
            "category": meta.get("category", "Other"),
            "template": meta.get("template", ""),
            "user_notes": meta.get("user_notes", ""),
            "system_audio": session.system_audio_status,
            "system_audio_error": session.system_audio_error,
            **_diarization_payload(session),
            "seconds": int(session.elapsed_seconds()),
            "segments": [
                {"stamp": s.stamp, "text": s.text, "source": s.source}
                for s in session.segments[-8:]
            ],
            "segment_count": len(session.segments),
        }
    return {
        "watching": (
            _config.meetings_enabled
            and _watcher.error == ""
            and _config.meeting_watch
        ),
        "watch_error": _watcher.error,
        "mic_busy": _watcher.mic_busy,
        "detected": _watcher.detected,
        "platform": _watcher.platform,
        "auto_start": _config.meeting_auto_start,
        "capture_system_audio": _config.capture_system_audio,
        "speaker_diarization": bool(_config.speaker_diarization),
        "session": live,
        "last_saved": last_saved,
        "chat_available": _chat_transcript_path() is not None,
    }


def _open_system_audio():
    """System audio source per config, or (None, reason). Never raises."""
    if not _config.capture_system_audio:
        return None, ""
    try:
        return open_system_audio(_config.sample_rate, _config.system_audio_device), ""
    except Exception as exc:  # noqa: BLE001 - a missing source means mic-only
        log.warning("system audio unavailable; mic only: %s", exc)
        return None, str(exc) or type(exc).__name__


def _open_diarization():
    """Diarization collector per config, or (None, reason). Never raises.
    With the flag off nothing from localflow.diarization is imported."""
    if not _config.speaker_diarization:
        return None, ""
    try:
        from localflow.diarization.factory import build_diarization
        return build_diarization(_config)
    except Exception as exc:  # noqa: BLE001 - diarization is optional
        log.warning("speaker diarization unavailable: %s", exc)
        return None, str(exc) or type(exc).__name__


def _diarization_state(session) -> tuple[str, str]:
    """(status, error) for a session; tolerant of sessions without the attributes."""
    error = getattr(session, "diarization_error", "") or ""
    status = getattr(session, "diarization_status", None)
    if status not in ("off", "collecting", "unavailable"):
        status = "unavailable" if error else "off"
    return status, error


def _diarization_payload(session) -> dict:
    status, error = _diarization_state(session)
    return {"diarization": status, "diarization_error": error}


def _close_diarization(session) -> None:
    """Stop a session's collector without finalizing. Never raises."""
    collector = getattr(session, "diarization", None)
    if collector is None:
        return
    try:
        collector.close()
    except Exception:
        log.exception("closing speaker diarization failed")


def _finalize_diarization(session, segments) -> tuple[list, str, list[dict]]:
    """(segments, status, speakers). Never raises; any problem keeps the original segments."""
    collector = getattr(session, "diarization", None)
    if collector is None:
        return segments, "off", []
    try:
        result = collector.finalize(segments)
        if result.status == "ok":
            speakers = [
                {"id": s.id, "label": s.label, "name": s.name, "source": s.source,
                 "seconds": round(float(s.seconds), 1)}
                for s in result.speakers
            ]
            return list(result.segments), "ok", speakers
        status = result.status if result.status in ("channel", "failed", "timeout") else "failed"
    except Exception:
        log.exception("speaker diarization finalize failed; keeping channel labels")
        return segments, "failed", []
    return segments, status, []


def _start_session(title: str, category: str, template: str = "",
                   user_notes: str = "", *, auto_title: bool | None = None) -> dict:
    """Claim the session slot, then open the mic. Shared by HTTP and the watcher.

    `auto_title`: the title is a placeholder to be replaced by a generated one
    when the notes are saved. By default that is decided from the title itself
    (empty or the "Meeting" the Hub and web page fill in); the watcher passes
    True for its "<Platform> meeting". A title the user typed is kept as is.
    """
    global _session, _session_meta, _last_saved, _session_state
    if not (title or "").strip():
        auto_title, title = True, ""  # nothing typed: always name it on save
    elif auto_title is None:
        auto_title = is_default_title(title)
    template_id = template or _config.meeting_default_template
    if template and template not in _templates():
        raise HTTPException(status_code=400, detail=f"unknown template {template!r}")
    if len(user_notes) > MAX_USER_NOTES_CHARS:
        raise HTTPException(status_code=413, detail="notes too long")
    with _session_lock:
        if _session_state != "idle":
            raise HTTPException(
                status_code=409, detail="a meeting session is already running"
            )
        _session_state = "starting"  # slot claimed; nobody else can start or stop

    # Outside the lock: the first _get_transcriber() loads a whisper model and
    # the mic open can block. "starting" already excludes everyone else.
    category = category or _watcher.platform or "Other"
    _watcher.pause()  # we hold the mic now; don't self-detect
    try:
        system_audio, system_error = _open_system_audio()
        collector, diarization_error = _open_diarization()
        extra = {"diarization": collector} if collector is not None else {}
        session = MeetingSession(
            _get_transcriber(),
            _transcribe_lock,
            sample_rate=_config.sample_rate,
            chunk_seconds=_config.meeting_chunk_seconds,
            system_audio=system_audio,
            **extra,
        )
        session.system_audio_error = system_error
        if diarization_error:
            session.diarization_error = diarization_error
        session.start()
    except Exception as exc:  # noqa: BLE001 - cleanup boundary: always release the slot
        _watcher.resume()
        with _session_lock:
            _session_state = "idle"
        raise HTTPException(status_code=500, detail=f"mic open failed: {exc}")

    with _session_lock:
        _session = session
        _session_meta = {
            "title": title or "Meeting",
            "title_auto": bool(auto_title),
            "category": category,
            "template": template_id,
            "user_notes": user_notes,
        }
        _last_saved = None
        _session_state = "running"
    if _config.sounds_enabled:
        sounds.play("start")
    return {
        "ok": True,
        "category": category,
        "template": template_id,
        "system_audio": session.system_audio_status,
        "system_audio_error": session.system_audio_error,
        **_diarization_payload(session),
    }


def _on_meeting_detected(platform: str) -> None:
    """Watcher-thread callback: auto-start a session on detection."""
    if not _config.meeting_auto_start or not _config.meeting_watch:
        return
    with _session_lock:
        if _session_state != "idle":
            return  # cheap pre-check; _start_session re-checks atomically
    title = f"{platform} meeting" if platform and platform != "Other" else "Meeting"
    try:
        _start_session(title, "", auto_title=True)
        log.info("auto-started meeting session (platform=%s)", platform)
    except HTTPException as exc:
        log.warning("auto-start skipped: %s", exc.detail)


@app.post("/meeting/start")
def meeting_start(body: MeetingStart) -> dict:
    return _start_session(
        body.title, body.category, body.template, body.user_notes,
        auto_title=body.auto_title,
    )


@app.post("/meeting/notes")
def meeting_notes(body: MeetingNotes) -> dict:
    """Save the in-meeting notepad; the latest save is what gets enhanced on stop."""
    if len(body.notes) > MAX_USER_NOTES_CHARS:
        raise HTTPException(status_code=413, detail="notes too long")
    with _session_lock:
        if _session_state != "running":
            raise HTTPException(status_code=409, detail="no meeting session running")
        _session_meta["user_notes"] = body.notes
    return {"ok": True, "chars": len(body.notes)}


def _chat_transcript_path() -> Path | None:
    with _session_lock:
        last_saved = _last_saved
    if last_saved and last_saved.get("log_path"):
        path = Path(last_saved["log_path"])
        if path.is_file():
            return path
    # After a restart _last_saved is gone; fall back to the newest log on disk.
    return latest_transcript_log(Path(_config.vault_path).expanduser() / _config.logs_folder)


@app.post("/meeting/chat")
async def meeting_chat(body: MeetingChat) -> dict:
    """Answer a question about the last saved meeting from its transcript."""
    question = body.question.strip()
    if not question:
        raise HTTPException(status_code=400, detail="question is empty")
    if len(question) > MAX_CHAT_QUESTION_CHARS:
        raise HTTPException(status_code=413, detail="question too long")
    path = _chat_transcript_path()
    if path is None:
        raise HTTPException(status_code=404, detail="no saved meeting transcript yet")
    transcript = read_transcript_log(path)

    def _answer() -> str:
        return answer_question(_summarizer.llm, transcript, question)

    try:
        answer = await run_in_threadpool(_answer)
    except Exception as exc:
        log.exception("meeting chat failed")
        raise HTTPException(status_code=502, detail=f"LLM error: {exc}")
    return {"answer": answer, "log_path": str(path)}


MAX_RECALL_QUESTION_CHARS = 1_000


class RecallRequest(BaseModel):
    question: str
    since: str | None = None
    k: int = 8
    llm: bool = True


@app.post("/api/recall")
async def api_recall(body: RecallRequest) -> dict:
    """Search dictation history, transcripts and notes; optionally answer with the LLM.

    Lives under /api/ so the token middleware applies even to loopback POSTs: this
    exposes everything the user has said.
    """
    question = body.question.strip()
    if not question:
        raise HTTPException(status_code=400, detail="question is empty")
    if len(question) > MAX_RECALL_QUESTION_CHARS:
        raise HTTPException(status_code=413, detail="question too long")
    if not 1 <= body.k <= 20:
        raise HTTPException(status_code=400, detail="k must be between 1 and 20")
    try:
        recall_logic.parse_since(body.since)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc))
    kwargs = {
        "store": _get_history() if _config.history_enabled else None,
        "vault_path": _config.vault_path,
        "notes_folder": _config.notes_folder,
        "logs_folder": _config.logs_folder,
        "k": body.k,
        "since": body.since,
    }

    def _run():
        if not body.llm:
            return recall_logic.search(question, **kwargs)
        return recall_logic.recall(question, llm=_summarizer.llm, **kwargs)

    try:
        result = await run_in_threadpool(_run)
    except Exception as exc:
        log.exception("recall failed")
        raise HTTPException(status_code=500, detail=f"recall failed: {exc}")
    return result.to_dict()


def _write_prompt_files(prompts_md: str, title: str, started_at: datetime) -> dict:
    """Extract work prompts to files + queue. Never fails the meeting save."""
    prompts_dir = Path(_config.prompts_dir).expanduser()
    prompts = parse_work_prompts(prompts_md)
    if not prompts:
        return {}
    paths = write_prompts(prompts, title, started_at, prompts_dir)
    _queue.add(prompts, paths, title)
    return {"prompts": len(paths), "prompts_dir": str(prompts_dir)}


@app.post("/meeting/stop")
async def meeting_stop() -> dict:
    global _session, _session_state
    with _session_lock:
        if _session_state != "running" or _session is None:
            raise HTTPException(status_code=409, detail="no meeting session running")
        session = _session
        meta = dict(_session_meta)
        # Release the slot and mark it draining before the slow work below, so a
        # second /meeting/stop or an auto-start sees "stopping" and bounces.
        _session = None
        _session_state = "stopping"

    def _finish() -> dict:
        global _last_saved
        finalized = False
        try:
            segments = session.stop()
            if _config.sounds_enabled:
                sounds.play("stop")
            _watcher.resume()
            segments, diarization_status, speakers = _finalize_diarization(session, segments)
            finalized = True
        finally:
            if not finalized:  # capture shutdown failed: don't leave the worker running
                _close_diarization(session)
        # Snapshot the summary client and config together before a reload can
        # change the provider or model for the title.
        with _config_lock:
            summarizer = _summarizer
            config = _config
        transcript = format_transcript(segments)
        template = resolve_template(meta.get("template"), config.meeting_templates_dir)
        user_notes = meta.get("user_notes", "")
        notes_md = summarizer.summarize(transcript, user_notes=user_notes, template=template)
        title = meta.get("title", "Meeting")
        title_from_model = False
        auto_title = bool(meta.get("title_auto"))
        if auto_title:
            # Never fails the save: any problem gives "Meeting Nov 16, 10:30 AM".
            title_llm = summarizer.llm
            fast_config = fast_model_config(config)
            if fast_config is not None:
                try:
                    title_llm = build_llm(fast_config, timeout=300, num_ctx=8192)
                except Exception as exc:
                    log.warning("meeting title client unavailable (%s); using the date",
                                type(exc).__name__)
                    title_llm = None
            title, title_from_model = generate_title(
                title_client(title_llm), transcript, user_notes, session.started_at,
            )
        extra: dict = {}
        if config.work_prompts:
            prompts_md = _prompt_gen.generate(notes_md)
            if prompts_md:
                notes_md += f"\n\n## Suggested Work Prompts\n\n{prompts_md}"
                try:
                    extra = _write_prompt_files(prompts_md, title, session.started_at)
                except Exception:
                    log.exception("writing work prompts failed; notes still saved")
        saved = _writer.write(
            title=title,
            category=meta.get("category", "Other"),
            started_at=session.started_at,
            duration_seconds=session.elapsed_seconds(),
            notes_md=notes_md,
            segments=segments,
            user_notes=user_notes,
            template=template.id,
            generated_title=auto_title,
        )
        payload = {
            "title": saved.title,
            "title_generated": title_from_model,
            "notes_path": str(saved.notes_path),
            "log_path": str(saved.log_path),
            "segments": len(segments),
            "template": template.id,
            "enhanced": bool(user_notes.strip()),
            "meeting_id": getattr(saved, "meeting_id", None),
            "diarization": diarization_status,
            "speakers": speakers,
            **extra,
        }
        with _session_lock:
            _last_saved = payload
        return payload

    try:
        return await run_in_threadpool(_finish)
    finally:
        with _session_lock:
            _session_state = "idle"
        _release_stale_transcriber()  # a speech model change waited for this meeting


@app.post("/meeting/dismiss")
def meeting_dismiss() -> dict:
    _watcher.dismiss()
    return {"ok": True}


# Prompts are reviewed here but dispatched from the CLI only — no run endpoint.
# The approve/reject rules live in localflow.dispatch so the native hub applies
# exactly the same guards and compare-and-set.


@app.get("/prompts")
def prompts_list(status: str | None = None) -> dict:
    return {"entries": _queue.list(status=status)}


def _transition(entry_id: str, verb: str, status: str, allowed: tuple[str, ...]) -> dict:
    """Guarded status change. 404 if the entry is gone, 409 if the move is invalid."""
    try:
        return transition_prompt(_queue, entry_id, verb, status, allowed)
    except PromptTransitionError as exc:
        raise HTTPException(status_code=exc.status_code, detail=exc.detail)


@app.post("/prompts/{entry_id}/approve")
def prompts_approve(entry_id: str) -> dict:
    return _transition(entry_id, "approve", "approved", APPROVE_FROM)


@app.post("/prompts/{entry_id}/reject")
def prompts_reject(entry_id: str) -> dict:
    return _transition(entry_id, "reject", "rejected", REJECT_FROM)


def _sync_meeting_watcher() -> None:
    """Start or stop mic watching to match the (possibly reloaded) config."""
    if _config.meetings_enabled and _config.meeting_watch:
        _watcher.start()
    else:
        _watcher.stop()


_stt_reload_pending = False


def _release_stale_transcriber() -> bool:
    """Drop the old speech model once no meeting is using it.

    A meeting keeps the model it started with (its audio is never handed to
    a new one mid-meeting); the next transcription or meeting loads the new
    choice lazily through _get_transcriber, which also renews the STT lease.
    Until then the old lease stays, so Free up space keeps both protected.
    """
    global _transcriber, _stt_reload_pending
    with _session_lock:
        if _session_state != "idle":
            return False
        with _lock:
            if not _stt_reload_pending:
                return False
            _transcriber = None
            _stt_reload_pending = False
    log.info("speech model change picked up; the new model loads on next use")
    return True


def _apply_live_config(fresh: Config) -> None:
    """ConfigWatcher callback: adopt live settings edited by another process
    (Hub Settings, Models & keys, `lf tweak`, `lf config set`).

    The allowlisted settings are copied (merge_live). The model groups are
    copied whole, never field by field (live_config.adopt_groups): a new
    cleanup provider/model/key ref rebuilds the LLM clients (Ask, cleanup,
    summaries and meeting titles), and a new speech model replaces the
    loaded one once no meeting is running. Host, port and paths stay as
    started; merge_live logs those as restart-only."""
    global _config, _ollama, _cleaner, _summarizer, _stt_reload_pending
    with _config_lock:
        # Saves write the file's own features.meetings back (config_toml);
        # track the file's current value even when nothing live changed.
        _config = replace(_config, _file_meetings_enabled=fresh._file_meetings_enabled)
        merged, changed = merge_live(_config, fresh, reloaded=RUNTIME_FIELDS)
        groups = runtime_changes(merged, fresh)
        if not changed and not groups:
            return
        if groups:
            merged = adopt_groups(merged, fresh, groups)
        _config = merged
        if "llm" in groups:
            try:
                _ollama = build_llm(_config)
                _summarizer = MeetingSummarizer(build_llm(_config, timeout=300, num_ctx=8192))
                _cleaner = None
            except Exception:
                log.exception("could not rebuild the cleanup model client")
        if "stt" in groups:
            with _lock:
                _stt_reload_pending = True
    if "stt" in groups:
        _release_stale_transcriber()
    if "meeting_watch" in changed:
        _sync_meeting_watcher()
    names = sorted(changed) + [f"{group} model" for group in sorted(groups)]
    log.info("settings updated without restart: %s", ", ".join(names))


def main() -> None:
    import uvicorn

    prepare_process_logs()
    # Started here, not in the startup hook, so tests driving the app through
    # TestClient never watch the real config file.
    ConfigWatcher(_apply_live_config, on_tick=_release_stale_transcriber).start()
    uvicorn.run(app, host=_config.server_host, port=_config.server_port)


if __name__ == "__main__":
    main()
