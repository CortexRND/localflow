"""`claude -p` as a local headless process.

argv is assembled only from fixed tokens and validated config; the prompt is
written to stdin (never argv), the child runs in its own process group with a
scrubbed environment, and one reader thread per session blocks on the pipe.
"""

from __future__ import annotations

import json
import logging
import os
import re
import signal
import subprocess
import sys
import threading
from collections import deque

from localflow.agents.models import (
    OK,
    AgentSession,
    ProviderCapabilities,
    ProviderError,
    Readiness,
)
from localflow.agents.providers.base import AgentProvider, EventSink
from localflow.agents.redact import scrub_env

log = logging.getLogger("localflow.agents.claude")

# Fixed argv prefixes; `agents.claude_wrapper` is an enum, never free-form.
WRAPPERS: dict[str, tuple[str, ...]] = {"none": (), "headroom": ("headroom", "wrap")}
WRAPPER_ENV: dict[str, dict[str, str]] = {
    "none": {},
    "headroom": {"HEADROOM_SAVINGS_PROFILE": "coding", "HEADROOM_TELEMETRY": "on",
                 "HEADROOM_BEACON": "off"},
}
MODEL_RE = re.compile(r"^[A-Za-z0-9._:\[\]-]{0,80}$")
PERMISSION_MODES = ("default", "acceptEdits", "plan")
VERSION_TIMEOUT_SECONDS = 10
MAX_STDOUT_LINES = 4000


def _last_line(text: str) -> str:
    lines = [line for line in (text or "").splitlines() if line.strip()]
    return lines[-1].strip()[:200] if lines else "no output"


def build_argv(config, *, resume_id: str = "", model: str = "") -> list[str]:
    wrapper = WRAPPERS.get(getattr(config, "agent_claude_wrapper", "none"), ())
    binary = getattr(config, "agent_claude_bin", "claude") or "claude"
    argv = [*wrapper, binary, "--"] if wrapper else [binary]
    if getattr(config, "agent_claude_stream", False):
        argv += ["-p", "--output-format", "stream-json", "--verbose"]
    else:
        argv += ["-p", "--output-format", "json"]
    if resume_id:
        argv += ["--resume", resume_id]
    mode = getattr(config, "agent_claude_permission_mode", "default")
    if mode in PERMISSION_MODES and mode != "default":
        argv += ["--permission-mode", mode]
    model = model or getattr(config, "agent_claude_model", "")
    if model and MODEL_RE.match(model):
        argv += ["--model", model]
    return argv


def _result_object(lines) -> dict | None:
    for line in reversed(list(lines)):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            data = json.loads(line)
        except ValueError:
            continue
        if isinstance(data, dict) and data.get("type") == "result":
            return data
    return None


def _assistant_text(data: dict) -> str:
    message = data.get("message")
    if not isinstance(message, dict):
        return ""
    parts = []
    for block in message.get("content") or []:
        if isinstance(block, dict) and block.get("type") == "text":
            parts.append(str(block.get("text", "")))
    return "".join(parts)


class ClaudeProvider(AgentProvider):
    id = "claude"
    label = "Claude Code"
    capabilities = ProviderCapabilities(
        kind="local_process", resume=True, live_message=False, cancel=True, streaming=False,
        needs_cwd=True, reattach=False, secret_names=("agent_claude_api_key",),
    )

    def __init__(self, config, ctx, profile=None):
        super().__init__(config, ctx, profile)
        self._procs: dict[str, subprocess.Popen] = {}
        self._timers: dict[str, threading.Timer] = {}
        self._lock = threading.Lock()

    @classmethod
    def capabilities_for(cls, config, profile=None) -> ProviderCapabilities:
        from dataclasses import replace

        return replace(cls.capabilities,
                       streaming=bool(getattr(config, "agent_claude_stream", False)))

    @classmethod
    def required_secrets(cls, config, profile=None) -> tuple[str, ...]:
        return ()  # optional: empty means the CLI's own login

    # -- readiness --

    def _binary_found(self) -> bool:
        binary = getattr(self.config, "agent_claude_bin", "claude") or "claude"
        if os.path.isabs(binary):
            return os.path.isfile(binary) and os.access(binary, os.X_OK)
        return self.ctx.which(binary) is not None

    def readiness(self, *, deep: bool = False) -> Readiness:
        wrapper = getattr(self.config, "agent_claude_wrapper", "none")
        if not self._binary_found():
            return Readiness(False, "missing_binary",
                             "The claude command was not found; install Claude Code or set "
                             "agents.claude_bin.")
        if wrapper != "none" and self.ctx.which(WRAPPERS[wrapper][0]) is None:
            return Readiness(False, "missing_wrapper",
                             f"The {WRAPPERS[wrapper][0]} command was not found; install it or "
                             "set agents.claude_wrapper to none.")
        if not deep:
            return OK
        binary = getattr(self.config, "agent_claude_bin", "claude") or "claude"
        try:
            result = self.ctx.run([binary, "--version"], capture_output=True, text=True,
                                  timeout=VERSION_TIMEOUT_SECONDS, shell=False,
                                  env=scrub_env(self.ctx.environ), stdin=subprocess.DEVNULL)
        except (OSError, subprocess.SubprocessError) as exc:
            return Readiness(False, "missing_binary",
                             f"claude --version failed ({type(exc).__name__}).")
        if result.returncode != 0:
            return Readiness(False, "missing_binary",
                             f"claude --version exited {result.returncode}.")
        return Readiness(True, "ok", self.ctx.redactor(_last_line(result.stdout)))

    # -- lifecycle --

    def _env(self) -> dict[str, str]:
        env = scrub_env(self.ctx.environ)
        env.pop("ANTHROPIC_API_KEY", None)
        key = self.ctx.get_secret("agent_claude_api_key")
        if key:
            env["ANTHROPIC_API_KEY"] = key
        env.update(WRAPPER_ENV.get(getattr(self.config, "agent_claude_wrapper", "none"), {}))
        return env

    def start(self, session: AgentSession, sink: EventSink) -> None:
        if not session.cwd or not os.path.isdir(session.cwd):
            raise ProviderError("spawn_failed", "the working directory does not exist")
        argv = build_argv(self.config, resume_id=session.internal.get("resume_external_id", ""),
                          model=session.options.get("model", ""))
        kwargs: dict = {}
        if sys.platform == "win32":
            kwargs["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
        else:
            kwargs["start_new_session"] = True
        try:
            proc = self.ctx.popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, cwd=session.cwd, env=self._env(),
                                  shell=False, **kwargs)
        except (OSError, ValueError) as exc:
            raise ProviderError("spawn_failed",
                                f"could not start claude ({type(exc).__name__})") from None
        with self._lock:
            self._procs[session.id] = proc
        streaming = bool(getattr(self.config, "agent_claude_stream", False))
        threading.Thread(target=self._read, args=(session, proc, sink, streaming),
                         name=f"lf-agents-claude-{session.id}", daemon=True).start()

    def _read(self, session: AgentSession, proc, sink: EventSink, streaming: bool) -> None:
        stderr_lines: deque[str] = deque(maxlen=200)

        def drain_stderr():
            try:
                for raw in proc.stderr:
                    line = raw.decode("utf-8", "replace") if isinstance(raw, bytes) else raw
                    stderr_lines.append(line)
                    if streaming:
                        sink.output("stderr", line)
            except (OSError, ValueError):
                pass

        err_thread = threading.Thread(target=drain_stderr, daemon=True,
                                      name=f"lf-agents-claude-err-{session.id}")
        err_thread.start()
        try:
            proc.stdin.write(session.prompt.encode("utf-8"))
            proc.stdin.close()
        except (OSError, ValueError):
            pass
        lines: deque[str] = deque(maxlen=MAX_STDOUT_LINES)
        try:
            for raw in proc.stdout:
                line = raw.decode("utf-8", "replace") if isinstance(raw, bytes) else raw
                lines.append(line)
                if streaming:
                    self._stream_line(line, sink)
        except (OSError, ValueError):
            pass
        returncode = proc.wait()
        err_thread.join(timeout=5)
        with self._lock:
            self._procs.pop(session.id, None)
            timer = self._timers.pop(session.id, None)
        if timer is not None:
            timer.cancel()
            if sys.platform != "win32":
                # Cancelled or timed out: a descendant may have closed its pipes and
                # survived SIGTERM, so kill the whole group now that the leader is gone.
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except OSError:  # ProcessLookupError / PermissionError: the group is gone
                    pass
        self._finish(lines, "".join(stderr_lines), returncode, sink, streaming)

    def _stream_line(self, line: str, sink: EventSink) -> None:
        stripped = line.strip()
        if not stripped:
            return
        try:
            data = json.loads(stripped)
        except ValueError:
            sink.output("system", stripped)
            return
        if not isinstance(data, dict):
            return
        if data.get("session_id") and data.get("type") == "system":
            sink.external(str(data["session_id"]), "")
        if data.get("type") == "assistant":
            text = _assistant_text(data)
            if text:
                sink.output("agent", text)

    def _finish(self, lines, stderr: str, returncode: int, sink: EventSink,
                streaming: bool) -> None:
        result = _result_object(lines)
        if result is None:
            code = "exit_nonzero" if returncode != 0 else "bad_output"
            what = f"claude exited {returncode}" if returncode != 0 else "unreadable claude output"
            sink.finish("failed", error={"code": code,
                                         "message": f"{what}: {_last_line(stderr)}"})
            return
        if result.get("session_id"):
            sink.external(str(result["session_id"]), "")
        sink.usage(cost_usd=result.get("total_cost_usd"), duration_ms=result.get("duration_ms"),
                   turns=result.get("num_turns"))
        text = str(result.get("result") or "")
        if not streaming and text:
            sink.output("agent", text)
        if result.get("is_error"):
            sink.finish("failed", result_text=text,
                        error={"code": "exit_nonzero", "message": _last_line(text or stderr)})
        elif returncode != 0:
            sink.finish("failed", result_text=text,
                        error={"code": "exit_nonzero",
                               "message": f"claude exited {returncode}: {_last_line(stderr)}"})
        else:
            sink.finish("completed", result_text=text)

    def stop(self, session: AgentSession) -> None:
        """SIGTERM the process group, SIGKILL after the grace period."""
        with self._lock:
            proc = self._procs.get(session.id)
        if proc is None or proc.poll() is not None:
            return
        _signal_group(proc, kill=False)
        timer = threading.Timer(self.ctx.kill_grace_seconds, _signal_group, args=(proc, True))
        timer.name = f"lf-agents-kill-{session.id}"
        timer.daemon = True
        with self._lock:
            self._timers[session.id] = timer
        timer.start()

    def cancel(self, session: AgentSession, sink: EventSink) -> None:
        self.stop(session)
        sink.finish("cancelled")

    def close(self) -> None:
        with self._lock:
            procs = list(self._procs.values())
        for proc in procs:
            _signal_group(proc, kill=True)


def _signal_group(proc, kill: bool) -> None:
    exited = proc.poll() is not None
    if exited and (sys.platform == "win32" or not kill):
        return
    try:
        if sys.platform == "win32":
            if kill:
                proc.kill()
            else:
                proc.send_signal(getattr(signal, "CTRL_BREAK_EVENT", signal.SIGTERM))
        else:
            # SIGKILL goes to the whole group even when the leader already exited on
            # SIGTERM: its descendants may still be running.
            os.killpg(proc.pid, signal.SIGKILL if kill else signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        if exited:
            return  # the whole group is already gone
        try:
            proc.kill() if kill else proc.terminate()
        except OSError:
            pass
    except OSError:
        try:
            proc.kill() if kill else proc.terminate()
        except OSError:
            pass
