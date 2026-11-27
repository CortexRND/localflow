"""Deterministic scripted provider for tests and `agents.sandbox` (no network).

A script advances one step per `poll()`, so tests drive it without real time.
"""

from __future__ import annotations

from localflow.agents.models import (
    OK,
    AgentSession,
    ProviderCapabilities,
    ProviderError,
    Readiness,
)
from localflow.agents.providers.base import AgentProvider, EventSink

SCRIPTS = ("ok", "fail", "needs_input", "slow", "hang", "flood", "secret_echo", "flaky_cancel")
SLOW_POLLS = 5
FLOOD_CHUNK = "x" * 4000 + "\n"
PLANTED_FALLBACK = "sk-ant-api03-mockplantedsecretvalue0000"


class MockProvider(AgentProvider):
    id = "mock"
    label = "Mock (sandbox)"
    capabilities = ProviderCapabilities(
        kind="cloud", resume=True, live_message=True, cancel=True, streaming=True,
        needs_cwd=False, reattach=True, secret_names=(),
    )

    def __init__(self, config, ctx, profile=None):
        super().__init__(config, ctx, profile)
        self._runs: dict[str, dict] = {}

    def readiness(self, *, deep: bool = False) -> Readiness:
        if not getattr(self.config, "agent_sandbox", False):
            return Readiness(False, "disabled", "The mock provider only runs in sandbox mode.")
        return OK

    def _run(self, session: AgentSession) -> dict:
        run = self._runs.get(session.id)
        if run is None:
            script = session.options.get("script", "ok")
            run = {"script": script, "step": 0, "messages": []}
            self._runs[session.id] = run
        return run

    def start(self, session: AgentSession, sink: EventSink) -> None:
        script = session.options.get("script", "ok")
        if script not in SCRIPTS:
            raise ProviderError("spawn_failed", f"unknown mock script {script!r}")
        self._run(session)
        sink.external(f"mock-{session.id}", "")
        if session.internal.get("resume_external_id"):
            sink.output("agent", f"resuming {session.internal['resume_external_id']}")

    def poll(self, session: AgentSession, sink: EventSink) -> float | None:
        run = self._run(session)
        run["step"] += 1
        step, script = run["step"], run["script"]
        interval = self.ctx.mock_poll_seconds
        if script == "ok":
            sink.output("agent", f"step {step}: working on it")
            if step >= 2:
                sink.finish("completed", result_text=f"done: {session.title}")
                return None
        elif script == "fail":
            sink.output("stderr", "mock: something went wrong")
            sink.finish("failed", error={"code": "exit_nonzero", "message": "mock run failed"})
            return None
        elif script == "needs_input":
            if not run["messages"]:
                if sink.current_state() != "waiting_input":
                    sink.output("agent", "I need more information.")
                    sink.state("waiting_input")
                return interval
            message = run["messages"][-1]
            sink.output("agent", f"echo: {message}")
            sink.finish("completed", result_text=f"received: {message}")
            return None
        elif script == "slow":
            sink.output("agent", f"slow step {step}")
            if step >= SLOW_POLLS:
                sink.finish("completed", result_text="slow run done")
                return None
        elif script in ("hang", "flaky_cancel"):
            return interval
        elif script == "flood":
            limit = int(getattr(self.config, "agent_max_output_kb", 512)) * 1024
            for _ in range(max(1, (limit * 2) // len(FLOOD_CHUNK) // 4)):
                sink.output("agent", FLOOD_CHUNK)
            if step >= 4:
                sink.finish("completed", result_text=FLOOD_CHUNK * 6)
                return None
        elif script == "secret_echo":
            planted = self.ctx.get_secret("agent_devin_api_key") or PLANTED_FALLBACK
            sink.output("agent", f"the key is {planted}")
            sink.finish("completed", result_text=f"leaked {planted}",)
            return None
        return interval

    def send_message(self, session: AgentSession, text: str, sink: EventSink) -> None:
        self._run(session)["messages"].append(text)

    def cancel(self, session: AgentSession, sink: EventSink) -> None:
        if self._run(session)["script"] == "flaky_cancel":
            raise ProviderError("cancel_failed", "the mock refused to stop")
        self._runs.pop(session.id, None)
        sink.finish("cancelled")
