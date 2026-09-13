import logging
import os
import subprocess
import threading
import time
from datetime import datetime
from pathlib import Path

import numpy as np
from fastapi import FastAPI, HTTPException, UploadFile
from fastapi.responses import FileResponse
from pydantic import BaseModel
from starlette.concurrency import run_in_threadpool

from localflow import sounds
from localflow.cleanup import Cleaner
from localflow.commands import discover_commands
from localflow.config import load_config
from localflow.dispatch import (
    PromptQueue,
    StatusConflict,
    parse_work_prompts,
    write_prompts,
)
from localflow.log import setup_logging
from localflow.meetings import (
    MeetingSession,
    MeetingSummarizer,
    MeetingWatcher,
    ObsidianWriter,
)
from localflow.providers.llm_ollama import OllamaLLM
from localflow.providers.llm_openai_compat import OpenAICompatLLM
from localflow.stt import Transcriber
from localflow.symbols import apply_spoken_symbols
from localflow.workprompts import WorkPromptGenerator

app = FastAPI()

setup_logging()
log = logging.getLogger("localflow.server")
_config = load_config()
_commands = discover_commands(_config.command_names, _config.skills_dirs)
_transcriber: Transcriber | None = None
_cleaner: Cleaner | None = None
_lock = threading.Lock()
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
_ollama = OllamaLLM(
    _config.ollama_url,
    _config.ollama_model,
    timeout=_config.cleanup_timeout,
    num_ctx=_config.cleanup_num_ctx,
)
_summarizer = MeetingSummarizer(
    OllamaLLM(
        _config.ollama_url,
        _config.ollama_model,
        timeout=300,
        num_ctx=8192,
    )
)
_writer = ObsidianWriter(
    _config.vault_path, _config.notes_folder, _config.logs_folder
)
key = _config.fireworks_api_key or os.environ.get("FIREWORKS_API_KEY", "")
_prompt_gen = WorkPromptGenerator(
    OpenAICompatLLM(
        base_url="https://api.fireworks.ai/inference/v1",
        model=_config.fireworks_model,
        api_key=key,
        temperature=0.3,
    )
    if key
    else None
)
# One shared instance: PromptQueue's lock is per-instance, so two instances
# racing would be last-writer-wins over the whole queue file.
_queue = PromptQueue()
# No queue lock here on purpose: PromptQueue serialises across processes itself,
# and status changes go through its compare-and-set. A lock here would only
# guard the server against the server.


@app.on_event("startup")
def _start_watcher() -> None:
    if _config.meeting_watch:
        _watcher.start()

_STATIC_DIR = Path(__file__).parent / "static"


def _get_transcriber() -> Transcriber:
    global _transcriber
    if _transcriber is None:
        with _lock:
            if _transcriber is None:
                _transcriber = Transcriber(
                    model_size=_config.model_size,
                    language=_config.language,
                    backend=_config.stt_backend,
                )
    return _transcriber


def _get_cleaner() -> Cleaner:
    global _cleaner
    if _cleaner is None:
        with _lock:
            if _cleaner is None:
                _cleaner = Cleaner(_ollama)
    return _cleaner


def _decode_audio(data: bytes) -> np.ndarray:
    try:
        proc = _run_ffmpeg(data)
    except FileNotFoundError:
        raise HTTPException(
            status_code=500, detail="ffmpeg not installed (brew install ffmpeg)"
        )
    if proc.returncode != 0 or not proc.stdout:
        stderr = proc.stderr.decode(errors="replace").strip()
        message = stderr.splitlines()[-1] if stderr else "ffmpeg failed to decode audio"
        raise HTTPException(status_code=400, detail=message[:300])
    return np.frombuffer(proc.stdout, dtype=np.float32)


def _run_ffmpeg(data: bytes) -> subprocess.CompletedProcess:
    return subprocess.run(
        [
            "ffmpeg",
            "-i", "pipe:0",
            "-f", "f32le",
            "-ac", "1",
            "-ar", "16000",
            "pipe:1",
        ],
        input=data,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def _transcribe_sync(audio: np.ndarray, clean: bool) -> str:
    text = _get_transcriber().transcribe(audio)
    if clean:
        text = _get_cleaner().clean(text)
    if _config.spoken_symbols and text:
        text = apply_spoken_symbols(text, _commands)
    return text


@app.get("/healthz")
def healthz() -> dict:
    return {"ok": True}


@app.get("/")
def index() -> FileResponse:
    return FileResponse(_STATIC_DIR / "index.html")


@app.post("/transcribe")
async def transcribe(file: UploadFile, clean: int = 0) -> dict:
    start = time.monotonic()
    data = await file.read()
    audio = await run_in_threadpool(_decode_audio, data)
    if audio.size == 0:
        raise HTTPException(status_code=400, detail="decoded audio is empty")
    text = await run_in_threadpool(_transcribe_sync, audio, bool(clean))
    elapsed_ms = int((time.monotonic() - start) * 1000)
    return {"text": text, "ms": elapsed_ms}


class MeetingStart(BaseModel):
    title: str = "Meeting"
    category: str = ""


@app.get("/meeting/status")
def meeting_status() -> dict:
    with _session_lock:  # snapshot so session and its meta can't disagree
        session, meta, last_saved = _session, dict(_session_meta), _last_saved
    live = None
    if session is not None and session.active:
        live = {
            "title": meta.get("title", "Meeting"),
            "category": meta.get("category", "Other"),
            "seconds": int(session.elapsed_seconds()),
            "segments": [
                {"stamp": s.stamp, "text": s.text} for s in session.segments[-8:]
            ],
            "segment_count": len(session.segments),
        }
    return {
        "watching": _watcher.error == "" and _config.meeting_watch,
        "watch_error": _watcher.error,
        "mic_busy": _watcher.mic_busy,
        "detected": _watcher.detected,
        "platform": _watcher.platform,
        "auto_start": _config.meeting_auto_start,
        "session": live,
        "last_saved": last_saved,
    }


def _start_session(title: str, category: str) -> dict:
    """Claim the session slot, then open the mic. Shared by HTTP and the watcher."""
    global _session, _session_meta, _last_saved, _session_state
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
        session = MeetingSession(
            _get_transcriber(),
            _transcribe_lock,
            sample_rate=_config.sample_rate,
            chunk_seconds=_config.meeting_chunk_seconds,
        )
        session.start()
    except Exception as exc:
        _watcher.resume()
        with _session_lock:
            _session_state = "idle"
        raise HTTPException(status_code=500, detail=f"mic open failed: {exc}")

    with _session_lock:
        _session = session
        _session_meta = {"title": title or "Meeting", "category": category}
        _last_saved = None
        _session_state = "running"
    if _config.sounds_enabled:
        sounds.play("start")
    return {"ok": True, "category": category}


def _on_meeting_detected(platform: str) -> None:
    """Watcher-thread callback: auto-start a session on detection."""
    if not _config.meeting_auto_start:
        return
    with _session_lock:
        if _session_state != "idle":
            return  # cheap pre-check; _start_session re-checks atomically
    title = f"{platform} meeting" if platform and platform != "Other" else "Meeting"
    try:
        _start_session(title, "")
        log.info("auto-started meeting session (platform=%s)", platform)
    except HTTPException as exc:
        log.warning("auto-start skipped: %s", exc.detail)


@app.post("/meeting/start")
def meeting_start(body: MeetingStart) -> dict:
    return _start_session(body.title, body.category)


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
    global _session, _last_saved, _session_state
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
        segments = session.stop()
        if _config.sounds_enabled:
            sounds.play("stop")
        _watcher.resume()
        transcript = " ".join(s.text for s in segments)
        notes_md = _summarizer.summarize(transcript)
        title = meta.get("title", "Meeting")
        extra: dict = {}
        if _config.work_prompts:
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
        )
        payload = {
            "notes_path": str(saved.notes_path),
            "log_path": str(saved.log_path),
            "segments": len(segments),
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


@app.post("/meeting/dismiss")
def meeting_dismiss() -> dict:
    _watcher.dismiss()
    return {"ok": True}


# Prompts are reviewed here but dispatched from the CLI only — no run endpoint.
# Approve doubles as retry: a `failed` entry is a dispatch that errored, and
# stranding it would let one transient Orca hiccup kill the prompt for good.
_APPROVE_FROM = ("pending", "failed")
_REJECT_FROM = ("pending",)


@app.get("/prompts")
def prompts_list(status: str | None = None) -> dict:
    return {"entries": _queue.list(status=status)}


def _transition(entry_id: str, verb: str, status: str, allowed: tuple[str, ...]) -> dict:
    """Guarded status change. 404 if the entry is gone, 409 if the move is invalid.

    Compare-and-set inside the queue rather than get-then-set here: the CLI
    dispatches in a separate process, so it can flip an entry to `dispatched`
    between our check and our write, and we would silently undo it.
    """
    try:
        updated = _queue.set_status(entry_id, status, expect=allowed)
    except StatusConflict as exc:
        raise HTTPException(
            status_code=409,
            detail=f"cannot {verb} a {exc.current} prompt (allowed from: {', '.join(allowed)})",
        )
    if updated is None:
        raise HTTPException(status_code=404, detail=f"unknown prompt: {entry_id}")
    return updated


@app.post("/prompts/{entry_id}/approve")
def prompts_approve(entry_id: str) -> dict:
    return _transition(entry_id, "approve", "approved", _APPROVE_FROM)


@app.post("/prompts/{entry_id}/reject")
def prompts_reject(entry_id: str) -> dict:
    return _transition(entry_id, "reject", "rejected", _REJECT_FROM)


def main() -> None:
    import uvicorn

    uvicorn.run(app, host=_config.server_host, port=_config.server_port)


if __name__ == "__main__":
    main()
