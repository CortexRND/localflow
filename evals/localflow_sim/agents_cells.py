"""The `agents` suite (gates A1-A8, see PROTOCOL.md "Agents"): one function per cell.

Imported only by `driver.py --child` for `--suite agents`, after `prepare()` has
isolated the cell and before the SUT is imported. Every cell talks to the real
SUT (`localflow.server` over a loopback TestClient, `localflow.app._process_job`,
`localflow.agents.*`); the only stand-ins are at the vendor boundary:

- `claude` / `headroom` on a private PATH are the SUT's own `fake_claude.py`
  recorder (argv / stdin / cwd / env, canned output);
- Devin and generic HTTP agents are an in-cell loopback stub (`Stub`);
- no cell touches the network or a real vendor CLI.

Planted fake secrets (`sim-secret-...`) live only in this process' environment.
Cells record booleans about them, never the values; the driver also scrubs any
planted value that reaches a result file to `[REDACTED:planted]`, which the
A5 gate counts as a leak.

Each cell returns observations only; `gates.py` judges them against the frozen
expectations in `scenarios.json`.
"""

from __future__ import annotations

import contextlib
import dataclasses
import itertools
import json
import logging
import os
import secrets
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

TMP = Path(os.environ.get("LFSIM_TMP", tempfile.gettempdir()))
SUT = Path(os.environ.get("SIM_SUT", str(Path.home() / "work/sim-sut"))).expanduser()
HARNESS = TMP / "_harness"  # the fake vendor CLI's own recordings; not part of the SUT sandbox
BIN = HARNESS / "bin"
RECORD = HARNESS / "claude.jsonl"
REPO = TMP / "repo"
REPO_ALIAS = "simrepo"
S = "/api/agents/sessions"
TERMINAL = ("completed", "failed", "cancelled", "timed_out")
PLANTED: dict[str, str] = {}  # secret name -> value; never written anywhere
LOCAL_PEER = ("127.0.0.1", 50000)
REMOTE_PEER = ("10.0.0.5", 50001)


# ---------------------------------------------------------------- isolation ---

def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def agents_toml(tmp: Path, cell: dict) -> str:
    """The `[server]` / `[agents]` part of the cell's config.toml (written by the parent)."""
    values = {
        "enabled": True, "hot_commands": False, "sandbox": True, "default_provider": "claude",
        "repos": [f"{REPO_ALIAS}={tmp / 'repo'}"], "default_repo": REPO_ALIAS,
        **cell.get("agents", {}),
    }

    def toml(value) -> str:
        if isinstance(value, bool):
            return "true" if value else "false"
        if isinstance(value, (int, float)):
            return str(value)
        if isinstance(value, list):
            return "[" + ", ".join(toml(v) for v in value) + "]"
        return json.dumps(str(value))

    lines = ["[server]", f"port = {free_port()}", "[agents]"]
    lines += [f"{key} = {toml(value)}" for key, value in values.items()]
    return "\n".join(lines) + "\n"


PLANTED_ENV = {"agent_devin_api_key": "LOCALFLOW_AGENT_DEVIN_API_KEY",
               "agent_claude_api_key": "LOCALFLOW_AGENT_CLAUDE_API_KEY",
               "agent_http_token": "LFSIM_PLANTED_HTTP_TOKEN"}


def plant() -> dict[str, str]:
    """Fresh fake secrets for one cell, as environment variables (parent side)."""
    return {env: f"sim-secret-{name.split('_')[1]}-{secrets.token_hex(16)}" for name, env in PLANTED_ENV.items()}


def prepare(cell: dict) -> None:
    """Temp git repo, private PATH with the fake claude/headroom, planted secrets.

    Runs in the cell process before `localflow` is imported. The planted values come
    from the parent's environment (`plant()`), so it can scrub them from its logs too."""
    HARNESS.mkdir(parents=True, exist_ok=True)
    BIN.mkdir(exist_ok=True)
    REPO.mkdir(exist_ok=True)
    git = ["git", "-C", str(REPO), "-c", "user.name=sim", "-c", "user.email=sim@example.invalid"]
    subprocess.run(["git", "init", "-q", str(REPO)], check=True, capture_output=True)
    (REPO / "README.md").write_text("# sim repo\n\nA throwaway repository for the agents sim.\n",
                                    encoding="utf-8")
    subprocess.run([*git, "add", "README.md"], check=True, capture_output=True)
    subprocess.run([*git, "commit", "-q", "-m", "sim: initial"], check=True, capture_output=True)
    fake = SUT / "tests" / "fixtures" / "agents" / "fake_claude.py"
    assert fake.is_file(), f"fake_claude.py missing at {fake}"
    for name in ("claude", "headroom"):
        shim = BIN / name
        shim.write_text(f"#!{sys.executable}\nimport os, runpy\n"
                        f"os.environ['FAKE_CLAUDE_NAME'] = {name!r}\n"
                        f"runpy.run_path({str(fake)!r}, run_name='__main__')\n", encoding="utf-8")
        shim.chmod(0o755)
    os.environ["PATH"] = os.pathsep.join([str(BIN), "/usr/bin", "/bin"])
    assert shutil.which("claude") == str(BIN / "claude"), shutil.which("claude")
    os.environ["FAKE_CLAUDE_RECORD"] = str(RECORD)
    os.environ.pop("FAKE_CLAUDE_MODE", None)
    os.environ.setdefault("PYNPUT_BACKEND", "dummy")
    # Nothing of the operator's vendor setup leaks into the cell (or the fake claude's env).
    for name in [n for n in os.environ if n.startswith(("ANTHROPIC_", "HEADROOM_", "CLAUDE_"))]:
        os.environ.pop(name, None)
    for name in ("DEVIN_API_KEY", "LOCALFLOW_DEVIN_BASE_URL"):
        os.environ.pop(name, None)
    for name, env in PLANTED_ENV.items():
        os.environ.setdefault(env, plant()[env])
        PLANTED[name] = os.environ[env]


def capture_logs() -> Path:
    """Every `localflow.*` record at DEBUG into a sandbox file (scanned by A5)."""
    path = TMP / "home" / ".localflow" / "sim-agents.log"
    path.parent.mkdir(parents=True, exist_ok=True)
    handler = logging.FileHandler(path, encoding="utf-8")
    handler.setFormatter(logging.Formatter("%(levelname)s %(name)s %(message)s"))
    root = logging.getLogger("localflow")
    root.addHandler(handler)
    root.setLevel(logging.DEBUG)
    return path


def http_token_env(profile_id: str) -> str:
    from localflow.agents.providers.http import secret_name

    return f"LOCALFLOW_{secret_name(profile_id).upper()}"


# --------------------------------------------------------------- utilities ---

def wait_for(predicate, timeout: float = 10.0, interval: float = 0.01):
    deadline = time.monotonic() + timeout
    while True:
        value = predicate()
        if value or time.monotonic() >= deadline:
            return value
        time.sleep(interval)


class FakeClock:
    def __init__(self, start: float = 1000.0):
        self.now = start

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


def records() -> list[dict]:
    if not RECORD.exists():
        return []
    return [json.loads(line) for line in RECORD.read_text(encoding="utf-8").splitlines() if line]


def record_view(rec: dict) -> dict:
    """What a cell may keep of one fake-claude invocation (never env values)."""
    env = rec.get("env") or {}
    return {
        "name": rec.get("name"), "argv": rec.get("argv"), "stdin": rec.get("stdin"),
        "cwd": rec.get("cwd"),
        "env_localflow": sorted(k for k in env if k.upper().startswith("LOCALFLOW_")),
        "env_scrubbed": not any(k in env for k in ("FIREWORKS_API_KEY", "DEVIN_API_KEY")),
        "anthropic_key_is_planted": env.get("ANTHROPIC_API_KEY") == PLANTED.get("agent_claude_api_key"),
        "headroom_env": {k: env[k] for k in sorted(env) if k.startswith("HEADROOM_")},
        "own_process_group": rec.get("pid") == rec.get("pgid"),
    }


def event_log_text(events: list[dict]) -> str:
    lines = []
    for e in events:
        if e["type"] == "state":
            lines.append(f"#{e['seq']} state -> {e['state']}")
        else:
            lines.append(f"#{e['seq']} {e['type']} [{e['stream']}] {e['text']}")
    return "\n".join(lines)


# ----------------------------------------------------------------- stubs ---

class Stub:
    """Scripted loopback HTTP server for the devin / http providers.

    `script(method, path, *responses)`: responses are (status, json-or-bytes[, headers]),
    consumed in order, the last one repeating. Unscripted routes answer 404.
    Requests are kept in memory only (they carry the planted bearer token)."""

    def __init__(self):
        self.requests: list[dict] = []
        self._routes: dict[tuple[str, str], list] = {}
        self._lock = threading.Lock()
        stub = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def _handle(self):
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body = json.loads(raw) if raw else None
                except ValueError:
                    body = None
                with stub._lock:
                    stub.requests.append({"method": self.command, "path": self.path,
                                          "headers": dict(self.headers), "json": body})
                    queue = stub._routes.get((self.command, self.path))
                    if queue:
                        status, payload, headers = queue.pop(0) if len(queue) > 1 else queue[0]
                    else:
                        status, payload, headers = 404, {"detail": "not scripted"}, None
                data = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                for key, value in (headers or {}).items():
                    self.send_header(key, value)
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = _handle

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self._server.serve_forever, daemon=True).start()

    @property
    def url(self) -> str:
        host, port = self._server.server_address[:2]
        return f"http://{host}:{port}"

    def script(self, method: str, path: str, *responses) -> None:
        with self._lock:
            self._routes[(method, path)] = [r if len(r) == 3 else (*r, None) for r in responses]

    def take(self) -> list[dict]:
        """Requests since the last take, as the gate sees them (auth as a boolean)."""
        with self._lock:
            taken, self.requests = self.requests, []
        out = []
        for req in taken:
            auth = req["headers"].get("Authorization", "")
            out.append({"method": req["method"], "path": req["path"], "json": req["json"],
                        "auth": "planted" if auth in planted_bearers() else ("none" if not auth else "other")})
        return out

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()


def planted_bearers() -> set[str]:
    return {f"Bearer {PLANTED[n]}" for n in ("agent_devin_api_key", "agent_http_token") if n in PLANTED}


# ------------------------------------------------------- server / clients ---

class Api:
    """The real `localflow.server` app through loopback TestClients; keeps every
    response text in memory (A5 scans them) and schema-checks route responses."""

    def __init__(self):
        from fastapi.testclient import TestClient

        from localflow import server

        self.server = server
        self.schema = json.loads((SUT / "docs" / "contracts" / "agents-api.v1.schema.json")
                                 .read_text(encoding="utf-8"))
        base = f"http://127.0.0.1:{server._config.server_port}"
        token = {"X-Localflow-Token": server._api_token}
        self.local = TestClient(server.app, base_url=base, client=LOCAL_PEER, headers=token)
        self.remote = TestClient(server.app, base_url=base, client=REMOTE_PEER, headers=token)
        self.bare = TestClient(server.app, base_url=base, client=LOCAL_PEER)
        self.wrong = TestClient(server.app, base_url=base, client=LOCAL_PEER,
                                headers={"X-Localflow-Token": "A" * len(server._api_token)})
        self.foreign = TestClient(server.app, base_url="http://evil.example", client=LOCAL_PEER)
        self.foreign_token = TestClient(server.app, base_url="http://evil.example", client=LOCAL_PEER,
                                        headers=token)
        self.bodies: list[str] = []
        self.log: list[dict] = []

    def set_config(self, **changes) -> None:
        self.server._config = dataclasses.replace(self.server._config, **changes)

    @property
    def manager(self):
        from localflow.agents import api as agents_api

        return agents_api.get_manager()

    def call(self, method: str, path: str, route: str | None = None, client=None, **kw):
        response = (client or self.local).request(method.upper(), path, **kw)
        self.bodies.append(response.text)
        try:
            body = response.json()
        except ValueError:
            body = response.text
        entry = {"call": f"{method.upper()} {route or path}", "status": response.status_code}
        if route is not None:
            entry["schema_errors"] = route_errors(self.schema, method.upper(), route,
                                                  response.status_code, body)
        self.log.append(entry)
        return response.status_code, body, response

    def get(self, path, route=None, **kw):
        return self.call("get", path, route, **kw)[:2]

    def post(self, path, route=None, **kw):
        return self.call("post", path, route, **kw)[:2]

    def session(self, sid: str) -> dict:
        return self.local.get(f"{S}/{sid}").json()["session"]

    def events(self, sid: str) -> list[dict]:
        out, after = [], 0
        while True:
            page = self.local.get(f"{S}/{sid}/events", params={"after": after, "limit": 500}).json()
            out += page["events"]
            after = page["next_after"]
            if not page["has_more"]:
                return out

    def wait_state(self, sid: str, states, timeout: float = 10.0) -> dict:
        states = (states,) if isinstance(states, str) else tuple(states)
        def reached():
            session = self.session(sid)
            return session if session["state"] in states else None

        return wait_for(reached, timeout) or self.session(sid)

    def launch(self, **body) -> tuple[int, dict]:
        return self.post(S, json=body)

    def mock(self, script: str, **body) -> str:
        status, data = self.launch(prompt=body.pop("prompt", f"sim: run the {script} mock script"),
                                   provider="mock", options={"script": script}, **body)
        assert status == 202, (status, data)
        return data["session"]["id"]


def session_view(api: Api, sid: str) -> dict:
    session = api.session(sid)
    events = api.events(sid)
    return {
        "id": sid, "state": session["state"], "states": [e["state"] for e in events if e["type"] == "state"],
        "seqs": [e["seq"] for e in events], "last_seq": session["last_seq"], "error": session["error"],
        "result_text": session["result_text"], "external_id": session["external_id"],
        "parent_id": session["parent_id"], "root_id": session["root_id"], "revision": session["revision"],
        "usage": session["usage"], "truncated_events": sum(e["type"] == "truncated" for e in events),
    }


# ---------------------------------------------------- JSON Schema (subset) ---

_TYPES = {
    "object": lambda v: isinstance(v, dict), "array": lambda v: isinstance(v, list),
    "string": lambda v: isinstance(v, str), "boolean": lambda v: isinstance(v, bool),
    "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
    "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
    "null": lambda v: v is None,
}


def schema_errors(schema: dict, value, node: dict, path: str = "$") -> list[str]:
    import re

    if "$ref" in node:
        return schema_errors(schema, value, schema["$defs"][node["$ref"].rsplit("/", 1)[-1]], path)
    if "const" in node and value != node["const"]:
        return [f"{path}: != const"]
    if "enum" in node and value not in node["enum"]:
        return [f"{path}: {str(value)[:40]!r} not in enum"]
    if "oneOf" in node and sum(not schema_errors(schema, value, s, path) for s in node["oneOf"]) != 1:
        return [f"{path}: oneOf"]
    if "type" in node:
        types = node["type"] if isinstance(node["type"], list) else [node["type"]]
        if not any(_TYPES[t](value) for t in types):
            return [f"{path}: {type(value).__name__} is not {types}"]
    errors: list[str] = []
    if isinstance(value, str):
        if "pattern" in node and not re.search(node["pattern"], value):
            errors.append(f"{path}: pattern")
        if "maxLength" in node and len(value) > node["maxLength"]:
            errors.append(f"{path}: maxLength")
    if _TYPES["number"](value):
        if "minimum" in node and value < node["minimum"]:
            errors.append(f"{path}: minimum")
        if "maximum" in node and value > node["maximum"]:
            errors.append(f"{path}: maximum")
    if isinstance(value, dict):
        props = node.get("properties", {})
        errors += [f"{path}: missing {n}" for n in node.get("required", []) if n not in value]
        extra = node.get("additionalProperties", True)
        for name, item in value.items():
            if name in props:
                errors += schema_errors(schema, item, props[name], f"{path}.{name}")
            elif extra is False:
                errors.append(f"{path}: unexpected {name}")
            elif isinstance(extra, dict):
                errors += schema_errors(schema, item, extra, f"{path}.{name}")
    if isinstance(value, list) and "items" in node:
        for i, item in enumerate(value):
            errors += schema_errors(schema, item, node["items"], f"{path}[{i}]")
    return errors


def route_errors(schema: dict, method: str, route: str, status: int, body) -> list[str]:
    table = schema["x-routes"].get(f"{method} {route}")
    if table is None:
        return [f"route {method} {route} not in the contract"]
    # `403 local_only` is part of §6.1 for every non-GET route but missing from x-routes.
    name = table.get(str(status)) or ("Error" if status == 403 and method != "GET" else None)
    if name is None:
        return [f"status {status} not in the contract for {method} {route}"]
    return schema_errors(schema, body, schema["$defs"][name])[:8]


# ======================================================== A1 lifecycle ===

def cell_lifecycle(cell: dict) -> dict:
    from localflow.agents.manager import AgentManager
    from localflow.agents.models import STATES, SessionConflict
    from localflow.agents.providers.base import ProviderContext
    from localflow.agents.store import AgentSessionStore
    from localflow.secrets import get_secret

    api = Api()
    out: dict = {"runs": {}, "terminal_checks": [], "illegal": [], "jev_error_items": [],
                 "jev_result_items": []}

    def finish(name: str, sid: str) -> dict:
        api.wait_state(sid, TERMINAL)
        out["runs"][name] = view = session_view(api, sid)
        return view

    ok = api.mock("ok", prompt="Summarise the README of the sim repo in one sentence")
    finish("ok", ok)
    finish("fail", api.mock("fail", prompt="Run the scripted failure"))
    waiting = api.mock("needs_input", prompt="Rename the default branch")
    out["waiting_seen"] = api.wait_state(waiting, "waiting_input")["state"]
    status, body = api.post(f"{S}/{waiting}/messages", json={"text": "use main as the new name"})
    out["message"] = {"status": status, "event_type": (body.get("event") or {}).get("type")
                      if isinstance(body, dict) else None}
    finish("needs_input", waiting)

    # Cancel while queued and while running: one slot, occupied by a hanging run.
    api.set_config(agent_max_concurrent=1)
    occupy = api.mock("hang", prompt="Hold the only slot")
    api.wait_state(occupy, "running")
    queued = api.mock("ok", prompt="Wait behind the hanging run")
    out["queued_seen"] = api.session(queued)["state"]
    status, _ = api.post(f"{S}/{queued}/cancel")
    out["cancel_queued_status"] = status
    finish("cancel_queued", queued)
    status, _ = api.post(f"{S}/{occupy}/cancel")
    out["cancel_running_status"] = status
    finish("cancel_running", occupy)
    api.set_config(agent_max_concurrent=2)

    flaky = api.mock("flaky_cancel", prompt="Refuse to stop")
    api.wait_state(flaky, "running")
    status, body = api.post(f"{S}/{flaky}/cancel")
    out["flaky_cancel_status"] = status
    finish("flaky_cancel", flaky)

    # Timeout (`hang`) on the real manager with an injected clock: no 10 s wait.
    clock = FakeClock()
    store = AgentSessionStore(TMP / "home" / ".localflow" / "agents-clock")
    manager = AgentManager(lambda: api.server._config, store=store, supervise=False,
                           inline_start=True, clock=clock,
                           ctx=ProviderContext(get_secret=get_secret, mock_poll_seconds=1.0))
    hung, _ = manager.launch({"prompt": "Never finish", "provider": "mock",
                              "options": {"script": "hang"}, "timeout_seconds": 10})
    for code in ("queued", "starting"):
        try:
            store.transition(hung.id, code)
            out["illegal"].append({"from": "running", "to": code, "raised": False})
        except SessionConflict as exc:
            out["illegal"].append({"from": "running", "to": code, "raised": True,
                                   "current": exc.current, "expected": list(exc.expected)})
    try:
        store.transition(hung.id, "completed", expect=("queued",))
        out["cas"] = {"raised": False}
    except SessionConflict as exc:
        out["cas"] = {"raised": True, "current": exc.current, "expected": list(exc.expected)}
    for _ in range(30):
        if store.get(hung.id).state in TERMINAL:
            break
        clock.advance(1.0)
        manager.tick()
    events = [e.to_dict() for e in store.events(hung.id, limit=500)]
    hung_s = store.get(hung.id)
    out["runs"]["hang"] = {"id": hung.id, "state": hung_s.state, "error": hung_s.error,
                           "states": [e["state"] for e in events if e["type"] == "state"],
                           "seqs": [e["seq"] for e in events], "last_seq": hung_s.last_seq}
    manager.shutdown()

    # Resume: a child (and grandchild) of the completed `ok` run.
    status, body = api.launch(prompt="Continue from the summary", provider="mock", resume_of=ok)
    child = body["session"]["id"] if status == 202 else ""
    if child:
        finish("resume_child", child)
        status2, body2 = api.launch(prompt="And continue again", provider="mock", resume_of=child)
        if status2 == 202:
            finish("resume_grandchild", body2["session"]["id"])
    out["resume_ids"] = {"parent": ok, "child": child}
    status, body = api.launch(prompt="Resume something still running", provider="mock",
                              resume_of="0" * 12)
    out["resume_unknown"] = {"status": status, "code": body.get("code") if isinstance(body, dict) else None}

    # Terminal sessions never change: HTTP cancel / message and direct store moves.
    store = api.manager.store
    for name in ("ok", "fail", "cancel_running", "flaky_cancel"):
        sid = out["runs"][name]["id"]
        before = api.session(sid)
        c_status, c_body = api.post(f"{S}/{sid}/cancel")
        m_status, m_body = api.post(f"{S}/{sid}/messages", json={"text": "are you there?"})
        direct = []
        for to in STATES:
            try:
                store.transition(sid, to)
                direct.append({"to": to, "raised": False})
            except SessionConflict as exc:
                direct.append({"to": to, "raised": True, "current": exc.current})
        after = api.session(sid)
        out["terminal_checks"].append({
            "run": name, "state": before["state"],
            "cancel": {"status": c_status, "code": c_body.get("code"), "state": c_body.get("state"),
                       "expected": c_body.get("expected"), "session_state": (c_body.get("session") or {}).get("state")},
            "message": {"status": m_status, "code": m_body.get("code"), "state": m_body.get("state"),
                        "expected": m_body.get("expected")},
            "direct": direct,
            "unchanged": all(before[k] == after[k] for k in ("state", "last_seq", "revision", "result_text", "error")),
        })
        if name == "ok":
            out["jev_error_items"].append({
                "id": "terminal:ok", "cause": "The user tried to cancel a session that had already completed.",
                "detail": c_body.get("detail", ""), "error_message": ""})

    for name, cause in (("fail", "The agent run itself failed (the scripted mock run exited with an error)."),
                        ("hang", "The agent produced no result before the session's 10 second timeout."),
                        ("flaky_cancel", "The provider refused to stop the run when the user cancelled it.")):
        error = out["runs"][name]["error"] or {}
        out["jev_error_items"].append({"id": name, "cause": cause, "detail": "",
                                       "error_message": error.get("message", "")})
    ok_session = api.session(ok)
    ok_events = api.events(ok)
    summary = next(s for s in api.local.get(S, params={"limit": 200}).json()["sessions"] if s["id"] == ok)
    out["jev_result_items"].append({
        "id": "ok", "prompt": ok_session["prompt"], "event_log": event_log_text(ok_events),
        "result_text": ok_session["result_text"], "result_preview": summary["result_preview"]})
    out["http"] = api.log
    return out


# ================================================= A2 hot commands seam ===

class RecordingPost:
    """`HotCommandLauncher(post=...)`: the launch request the server would get."""

    def __init__(self):
        self.payloads: list[dict] = []

    def __call__(self, url, payload, token):
        self.payloads.append(dict(payload))

        class Response:
            status_code = 202

            @staticmethod
            def json():
                return {"session": {}}

        return Response()


def describe_rule(rule: dict) -> str:
    """One hot-command rule as Jev reads it (JA1 / JA2)."""
    template = rule.get("prompt_template", "{prompt}")
    parts = [f"- trigger \"{rule['trigger']}\" -> provider {rule['provider']}"]
    if rule.get("repo"):
        parts.append(f"default repo {rule['repo']}")
    if "{prompt}" not in template:
        parts.append(f"fixed prompt \"{template}\" (any further words are ignored)")
    else:
        if template != "{prompt}":
            parts.append(f"prompt template \"{template}\"")
        parts.append(f"needs at least {rule.get('min_prompt_words', 3)} prompt words")
    text = ", ".join(parts)
    return text + (" (DISABLED: never fires)" if rule.get("enabled") is False else "")


def cell_hotcommands(cell: dict) -> dict:
    from unittest.mock import Mock

    import numpy as np

    from localflow import app
    from localflow.agents import rules as rule_ops
    from localflow.agents.handoff import HotCommandLauncher, HotCommandSeam
    from localflow.agents.models import HotCommandConflict
    from localflow.agents.store import HotCommandStore
    from localflow.dictionary import DictionaryStore
    from localflow.history import HistoryStore
    from localflow.hotphrases import HotPhraseStore
    from localflow.jobs import Job

    api = Api()
    spec = cell["hot"]
    extra_repos = {alias: TMP / "repos" / alias.replace(" ", "-") for alias in spec["extra_repos"]}
    for path in extra_repos.values():
        path.mkdir(parents=True, exist_ok=True)
    repos = [f"{REPO_ALIAS}={REPO}", *(f"{a}={p}" for a, p in extra_repos.items())]
    api.set_config(agent_hot_commands=True, agent_repos=repos, sounds_enabled=False, cleanup_enabled=False)
    phrases = HotPhraseStore()
    for phrase in spec["hot_phrases"]:
        phrases.add(phrase["trigger"], phrase["text"])
    dictionary = DictionaryStore()
    for spoken, written in spec["replacements"]:
        dictionary.add_replacement(spoken, written)

    out: dict = {"rules": {}, "conflicts": [], "utterances": [], "jev_prompt_items": [],
                 "jev_intent_items": []}
    for rule in spec["rules"]:
        body = {k: v for k, v in rule.items() if k != "key"}
        status, data = api.post("/api/agents/hot-commands", "/api/agents/hot-commands", json=body)
        out["rules"][rule["key"]] = {"status": status, "id": data.get("id") if isinstance(data, dict) else None}
    rule_ids = {key: r["id"] for key, r in out["rules"].items()}
    count_before = len(api.local.get("/api/agents/hot-commands").json()["rules"])
    for conflict in spec["conflicts"]:
        status, data = api.post("/api/agents/hot-commands", "/api/agents/hot-commands",
                                json={"trigger": conflict["trigger"], "provider": "claude"})
        out["conflicts"].append({"name": conflict["name"], "status": status,
                                 "code": data.get("code") if isinstance(data, dict) else None})
    store = HotCommandStore(api.manager.store.root / "hot_commands.json")
    try:
        rule_ops.add_rule(store, {"trigger": spec["conflicts"][0]["trigger"], "provider": "claude"},
                          api.server._config, known_provider=api.manager.known_provider)
        out["direct_conflict"] = "accepted"
    except HotCommandConflict as exc:
        out["direct_conflict"] = type(exc).__name__
    out["rules_after_conflicts"] = len(store.list()) == count_before

    clock = FakeClock()
    post = RecordingPost()
    launcher = HotCommandLauncher(base_url=lambda: "http://127.0.0.1:9", token=lambda: "sim",
                                  post=post, play=lambda cue: None)
    seam = HotCommandSeam(store, launcher, clock=clock, known_provider=api.manager.known_provider)
    history = HistoryStore(TMP / "a2-history.sqlite", TMP / "a2-clips")
    pasted: list[str] = []
    app.paste_text = lambda text, method: pasted.append(text)
    audio = np.full(16000, 0.1, np.float32)
    base_cfg = api.server._config

    def config_for(context: dict):
        cfg = dataclasses.replace(base_cfg, **context.get("flags", {}))
        drop = context.get("drop_repo")
        if drop:
            cfg = dataclasses.replace(cfg, agent_repos=[r for r in cfg.agent_repos if not r.startswith(drop + "=")])
        return cfg

    def run_job(text: str, cfg, kind: str = "dictate", use_seam: bool = True):
        transcriber = Mock(transcribe=Mock(return_value=text))
        cleaner = Mock(clean=Mock(side_effect=lambda value, **_kw: value))
        transformer = Mock(run=Mock(return_value="transformed text"))
        job = {"dictate": Job(kind="dictate", audio=audio),
               "command": Job(kind="command", audio=audio, selection="selected text"),
               "transform": Job(kind="transform", selection=text, preset="polish")}[kind]
        return app._process_job(
            job, cfg, transcriber, cleaner, transformer, (), phrases, dictionary, history,
            hot_command_seam=seam if use_seam else None)

    def baseline(text: str, kind: str = "dictate"):
        off = dataclasses.replace(base_cfg, agents_enabled=False, agent_hot_commands=False)
        return run_job(text, off, kind=kind, use_seam=False)[0]

    for item in [*({**p, "expect": True} for p in spec["positives"]),
                 *({**n, "expect": False} for n in spec["negatives"])]:
        context = item.get("context", {})
        cfg = config_for(context)
        kind = context.get("kind", "dictate")
        clock.advance(context.get("advance", 10.0))
        before_posts = len(post.payloads)
        if context.get("cooldown"):
            run_job(item["text"], cfg)  # the first of the pair launches
            launcher.join()
            clock.advance(1.0)
            before_posts = len(post.payloads)
        if context.get("retry"):  # re-transcribe a saved clip: never a hot command
            run_job("plain words to retry later", cfg, use_seam=False)
            entry = history.recent(limit=1)[0]

            def retry(config, entry_id=entry["id"], text=item["text"]):
                return app.retry_history_entry(
                    entry_id, history, config=config,
                    transcriber=Mock(transcribe=Mock(return_value=text)),
                    cleaner=Mock(clean=Mock(side_effect=lambda v, **_k: v)),
                    dictionary_store=dictionary, hot_phrases=phrases)

            result, error, row = retry(cfg), False, None
            normal = retry(dataclasses.replace(base_cfg, agents_enabled=False, agent_hot_commands=False))
        else:
            result, error = run_job(item["text"], cfg, kind=kind)
            row = history.recent(limit=1)[0]
            normal = None if item["expect"] else baseline(item["text"], kind)
        launcher.join()
        payloads = post.payloads[before_posts:]
        route, route_status = None, None
        flags_on = bool(cfg.agents_enabled and cfg.agent_hot_commands)
        comparable = kind == "dictate" and not context.get("cooldown") and not context.get("retry")
        if comparable:
            api.server._config = cfg
            status, data = api.post("/api/agents/hot-commands/test", "/api/agents/hot-commands/test",
                                    json={"text": item["text"]})
            api.server._config = base_cfg
            route_status = status
            route = data if status == 200 else {"status": status, **(data if isinstance(data, dict) else {})}
        out["utterances"].append({
            "id": item["id"], "case": item.get("case", "positive"), "text": item["text"], "expect": item["expect"],
            "kind": kind, "returned": result, "error": error, "normal": normal,
            "launches": [{"rule_id": p.get("rule_id"), "provider": p.get("provider"), "repo": p.get("repo", ""),
                          "prompt": p.get("prompt"), "origin": p.get("origin"), "id": p.get("id")} for p in payloads],
            "history": None if row is None else {k: row.get(k) for k in ("kind", "raw_text", "final_text", "status")},
            "history_audio": None if row is None else bool(row.get("audio_path")),
            "route": route, "route_status": route_status, "route_comparable": comparable, "flags_on": flags_on,
        })
    out["pasted"] = pasted  # the worker pastes; _process_job itself never does
    out["rule_ids"] = rule_ids

    # Jev JA1 / JA2 inputs: flags-on dictation utterances only (no context the
    # rule list cannot show, such as a cooldown or a feature flag).
    rule_lines = [describe_rule(r) for r in spec["rules"]]
    rules_text = ("Grammar: TRIGGER [in|on REPO_ALIAS] [to] PROMPT, with the trigger as the first words.\n"
                  + "\n".join(rule_lines)
                  + f"\nConfigured repo aliases: {', '.join([REPO_ALIAS, *spec['extra_repos']])}.")
    for utt in out["utterances"]:
        launch = utt["launches"][0] if utt["launches"] else None
        if utt["expect"] and launch:
            out["jev_prompt_items"].append({"id": utt["id"], "transcript": utt["text"], "rules": rules_text,
                                            "prompt": launch["prompt"],
                                            "expected": next(p["prompt"] for p in spec["positives"] if p["id"] == utt["id"])})
        if utt["route_comparable"] and not (utt["case"] in ("flag_off", "alias_removed")):
            out["jev_intent_items"].append({"id": utt["id"], "transcript": utt["text"], "rules": rules_text,
                                            "launched": bool(utt["launches"]), "expected": utt["expect"],
                                            "case": utt["case"]})
    out["http"] = api.log
    return out


# ================================================ A3 concurrency & bounds ===

def cell_concurrency(cell: dict) -> dict:
    from localflow.agents.models import RUNNING_STATES

    api = Api()
    bounds = cell["bounds"]
    api.set_config(agent_max_concurrent=bounds["max_concurrent"], agent_max_queued=bounds["max_queued"],
                   agent_max_output_kb=bounds["max_output_kb"], agent_max_sessions=2000)
    api.get("/api/agents/status")
    store = api.manager.store
    moves: list[tuple[str, str]] = []
    lock = threading.Lock()
    original = store.transition

    def transition(session_id, to, *args, **kwargs):
        with lock:
            result = original(session_id, to, *args, **kwargs)
            moves.append((session_id, to))
            return result

    store.transition = transition
    out: dict = {}
    launches = []
    for i in range(bounds["slow_launches"]):
        status, body, response = api.call("post", S, S, json={"prompt": f"slow run {i + 1}", "provider": "mock",
                                                              "options": {"script": "slow"}})
        launches.append({"status": status, "id": (body.get("session") or {}).get("id") if isinstance(body, dict) else None,
                         "code": body.get("code") if isinstance(body, dict) else None,
                         "retry_after": response.headers.get("Retry-After")})
    out["launches"] = launches
    accepted = [launch["id"] for launch in launches if launch["id"]]
    for sid in accepted:
        api.wait_state(sid, TERMINAL)
    out["slow_final"] = [api.session(sid)["state"] for sid in accepted]
    out["sessions_after_slow"] = len(store.list())
    states: dict[str, str] = {}
    peak, order = 0, []
    for sid, to in moves:
        states[sid] = to
        peak = max(peak, sum(1 for s in states.values() if s in RUNNING_STATES))
        if to == "starting":
            order.append(sid)
    out["peak_running"] = peak
    out["admission_order"] = order
    out["launch_order"] = accepted

    flood = api.mock("flood", prompt="Print far more than the output limit")
    api.wait_state(flood, TERMINAL, timeout=20)
    view = session_view(api, flood)
    events = api.events(flood)
    log_file = store.events_dir / f"{flood}.jsonl"
    out["flood"] = {"state": view["state"], "truncated_events": view["truncated_events"],
                    "file_bytes": log_file.stat().st_size, "limit_bytes": bounds["max_output_kb"] * 1024,
                    "last_event": {"type": events[-1]["type"], "state": events[-1]["state"]} if events else None,
                    "output_after_truncated": any(e["type"] == "output" for e in events
                                                  if e["seq"] > next((x["seq"] for x in events if x["type"] == "truncated"), 1 << 60)),
                    "result_chars": len(view["result_text"]),
                    "result_truncated": api.session(flood)["result_truncated"], "seqs_contiguous":
                    view["seqs"] == list(range(1, view["last_seq"] + 1))}

    # Retention: max_sessions, with an older session kept active the whole time.
    api.set_config(agent_max_sessions=bounds["max_sessions"])
    active = api.mock("hang", prompt="Stay active while older terminal sessions are pruned")
    api.wait_state(active, "running")
    order_all = [sid for sid in dict.fromkeys([*accepted, flood, active])]
    fails = []
    for i in range(bounds["retention_fails"]):
        sid = api.mock("fail", prompt=f"retention filler {i + 1}")
        api.wait_state(sid, TERMINAL)
        fails.append(sid)
    order_all += fails
    remaining = {s.id: s.state for s in store.list()}
    out["retention"] = {
        "creation_order": order_all, "remaining": sorted(remaining), "count": len(remaining),
        "active_id": active, "active_state": remaining.get(active),
        "event_files": sorted(p.stem for p in store.events_dir.glob("*.jsonl")),
        "terminal_ids_in_order": [sid for sid in order_all if sid != active],
    }
    api.post(f"{S}/{active}/cancel")
    store.transition = original
    out["http"] = api.log
    return out


# ===================================================== A4 API contract ===

ROUTES = [  # (method, route template) — every route of agents-api v1
    ("GET", "/api/agents/status"), ("GET", "/api/agents/providers"),
    ("POST", "/api/agents/providers/{id}/check"), ("PUT", "/api/agents/providers/{id}/secret"),
    ("GET", "/api/agents/http-profiles"), ("POST", "/api/agents/http-profiles"),
    ("PUT", "/api/agents/http-profiles/{id}"), ("DELETE", "/api/agents/http-profiles/{id}"),
    ("POST", "/api/agents/sessions"), ("GET", "/api/agents/sessions"),
    ("GET", "/api/agents/sessions/{id}"), ("POST", "/api/agents/sessions/{id}/cancel"),
    ("POST", "/api/agents/sessions/{id}/messages"), ("DELETE", "/api/agents/sessions/{id}"),
    ("GET", "/api/agents/sessions/{id}/events"), ("GET", "/api/agents/changes"),
    ("GET", "/api/agents/hot-commands"), ("POST", "/api/agents/hot-commands"),
    ("PUT", "/api/agents/hot-commands/{id}"), ("DELETE", "/api/agents/hot-commands/{id}"),
    ("POST", "/api/agents/hot-commands/test"),
]


def cell_api(cell: dict) -> dict:
    from localflow.agents.manager import Sink

    api = Api()
    api.set_config(agent_hot_commands=True)
    out: dict = {}
    stub = Stub()
    stub.script("POST", "/v1/runs", (200, {"id": "run-1"}))
    try:
        status, profile = api.post("/api/agents/http-profiles", "/api/agents/http-profiles", json={
            "label": "Sim agent", "base_url": stub.url, "headers": {"X-Source": "sim"},
            "start": {"method": "POST", "path": "/v1/runs", "body": {"input": "{prompt}"}}})
        status, rule = api.post("/api/agents/hot-commands", "/api/agents/hot-commands",
                                json={"trigger": "launch claude", "provider": "claude"})
        target = api.mock("fail", prompt="target for route probes")
        api.wait_state(target, TERMINAL)
        ids = {"session": target, "rule": rule["id"], "profile": profile["id"], "provider": "mock"}

        def concrete(route: str) -> str:
            return (route.replace("/providers/{id}", f"/providers/{ids['provider']}")
                    .replace("/http-profiles/{id}", f"/http-profiles/{ids['profile']}")
                    .replace("/sessions/{id}", f"/sessions/{ids['session']}")
                    .replace("/hot-commands/{id}", f"/hot-commands/{ids['rule']}"))

        def snapshot() -> dict:
            manager = api.manager
            return {"revision": manager.store.revision, "counts": manager.store.counts(),
                    "states": {s.id: s.state for s in manager.store.list()},
                    "rules": len(api.local.get("/api/agents/hot-commands").json()["rules"]),
                    "profiles": len(api.local.get("/api/agents/http-profiles").json()["profiles"])}

        before = snapshot()
        probes = []
        write_body = {"prompt": "unauthorised launch", "provider": "mock"}
        for method, route in ROUTES:
            path = concrete(route)
            kw = {"json": write_body} if method in ("POST", "PUT") else {}
            row = {"call": f"{method} {route}"}
            for label, client in (("no_token", api.bare), ("wrong_token", api.wrong), ("foreign_host", api.foreign)):
                status, body, _ = api.call(method, path, client=client, **kw)
                row[label] = status
            status, body, _ = api.call(method, path, client=api.foreign_token, **kw)
            row["foreign_host_with_token"] = status
            if method != "GET":
                status, body, _ = api.call(method, path, route, client=api.remote, **kw)
                row["remote"] = status
                row["remote_code"] = body.get("code") if isinstance(body, dict) else None
                row["remote_schema"] = api.log[-1]["schema_errors"]
            probes.append(row)
        status, _, _ = api.call("post", S, S, json=write_body, headers={"X-Forwarded-For": "203.0.113.9"})
        out["proxied_write"] = status
        out["probes"] = probes
        out["contract_gap_403"] = sorted(f"{m} {r}" for m, r in ROUTES
                                         if m != "GET" and "403" not in api.schema["x-routes"][f"{m} {r}"])
        out["unchanged_after_probes"] = snapshot() == before

        # Positive and error flows across every route (each response schema-checked).
        routes_hit = set()

        def hit(method, path, route, **kw):
            routes_hit.add(f"{method.upper()} {route}")
            return api.call(method, path, route, **kw)[:2]

        hit("get", "/api/agents/status", "/api/agents/status")
        hit("get", "/api/agents/providers", "/api/agents/providers")
        hit("post", "/api/agents/providers/mock/check", "/api/agents/providers/{id}/check", json={})
        hit("post", "/api/agents/providers/nosuch/check", "/api/agents/providers/{id}/check", json={})
        hit("put", "/api/agents/providers/devin/secret", "/api/agents/providers/{id}/secret",
            json={"value": "sim-not-a-secret-value"})
        hit("put", "/api/agents/providers/devin/secret", "/api/agents/providers/{id}/secret",
            json={"name": "llm_api_key", "value": "x"})
        hit("get", "/api/agents/http-profiles", "/api/agents/http-profiles")
        hit("post", "/api/agents/http-profiles", "/api/agents/http-profiles", json={"label": "bad"})
        hit("put", f"/api/agents/http-profiles/{profile['id']}", "/api/agents/http-profiles/{id}",
            json={"label": "Sim agent renamed"})
        hit("put", "/api/agents/http-profiles/00000000", "/api/agents/http-profiles/{id}", json={"label": "x"})
        hit("post", S, S, json={"prompt": "x" * 20001, "provider": "mock"})
        hit("post", S, S, json={"prompt": "x", "provider": "mock", "colour": "red"})
        hit("get", f"{S}/000000000000", "/api/agents/sessions/{id}")
        hit("post", f"{S}/{target}/messages", "/api/agents/sessions/{id}/messages", json={"text": "hi"})
        hit("post", f"{S}/{target}/cancel", "/api/agents/sessions/{id}/cancel")
        hit("get", f"{S}/{target}/events", "/api/agents/sessions/{id}/events", params={"wait": 99})
        hit("get", "/api/agents/changes", "/api/agents/changes", params={"since": "x"})
        hit("get", "/api/agents/hot-commands", "/api/agents/hot-commands")
        hit("post", "/api/agents/hot-commands", "/api/agents/hot-commands", json={"trigger": "launch claude", "provider": "claude"})
        hit("put", f"/api/agents/hot-commands/{rule['id']}", "/api/agents/hot-commands/{id}", json={"enabled": False})
        hit("post", "/api/agents/hot-commands/test", "/api/agents/hot-commands/test",
            json={"text": "launch claude to fix the flaky test"})
        hit("delete", f"/api/agents/hot-commands/{rule['id']}", "/api/agents/hot-commands/{id}")
        hit("delete", f"/api/agents/hot-commands/{rule['id']}", "/api/agents/hot-commands/{id}")

        # Pagination and filters.
        titles = [f"page item {i}" for i in range(1, 8)]
        page_ids = []
        for title in titles:
            page_ids.append(api.mock("fail", prompt=f"paging check {title}", title=title))
        cli_id = api.mock("ok", prompt="a cli launch", origin="cli")
        for sid in [*page_ids, cli_id]:
            api.wait_state(sid, TERMINAL)
        everything = api.local.get(S, params={"limit": 200}).json()["sessions"]
        walked, cursor, pages = [], None, 0
        while True:
            params = {"limit": 3, **({"before": cursor} if cursor else {})}
            status, page = hit("get", S, S, params=params)
            walked += [s["id"] for s in page["sessions"]]
            cursor, pages = page["next_cursor"], pages + 1
            if not cursor or pages > 50:
                break
        out["pagination"] = {"all": [s["id"] for s in everything], "walked": walked, "pages": pages,
                             "sorted": [s["id"] for s in sorted(everything, key=lambda s: (s["created_at"], s["id"]), reverse=True)]}
        filters = {}
        for name, params in (("state_failed", {"state": "failed"}), ("state_terminal", {"state": "terminal"}),
                             ("state_active", {"state": "active"}), ("provider_mock", {"provider": "mock"}),
                             ("provider_devin", {"provider": "devin"}),
                             ("origin_cli", {"origin": "cli"}), ("q", {"q": "PAGE ITEM 3"}),
                             ("root_id", {"root_id": target})):
            status, body = hit("get", S, S, params={**params, "limit": 200})
            filters[name] = {"status": status, "ids": [s["id"] for s in body.get("sessions", [])],
                             "states": sorted({s["state"] for s in body.get("sessions", [])}),
                             "origins": sorted({s["origin"] for s in body.get("sessions", [])})}
        filters["_expect"] = {"failed": sorted(s["id"] for s in everything if s["state"] == "failed"),
                              "terminal": sorted(s["id"] for s in everything),
                              "q": [page_ids[2]], "cli": [cli_id], "root": [target],
                              # Exclusion: every session here is mock, so a filter that is ignored returns them all.
                              "devin": sorted(s["id"] for s in everything if s.get("provider") == "devin")}
        bad = {}
        for name, params in (("state", {"state": "sleeping"}), ("limit", {"limit": 0}), ("origin", {"origin": "radio"}),
                             ("unknown", {"colour": "red"}), ("cursor", {"before": "!!"})):
            bad[name] = hit("get", S, S, params=params)[0]
        out["filters"], out["bad_filters"] = filters, bad

        # Idempotent replay and id conflicts.
        cid = secrets.token_hex(6)
        body = {"id": cid, "prompt": "idempotent launch", "provider": "mock", "options": {"script": "ok"}}
        n0 = len(api.manager.store.list())
        first = hit("post", S, S, json=body)
        replay = hit("post", S, S, json=body)
        conflict = hit("post", S, S, json={**body, "prompt": "a different prompt"})
        out["idempotent"] = {"first": first[0], "replay": replay[0], "conflict": conflict[0],
                             "conflict_code": conflict[1].get("code"),
                             "same_session": (first[1].get("session") or {}).get("id") == (replay[1].get("session") or {}).get("id") == cid,
                             "created": len(api.manager.store.list()) - n0}
        api.wait_state(cid, TERMINAL)

        # Events cursor (small pages) never skips or repeats a seq.
        slow = api.mock("slow", prompt="emit a few events")
        api.wait_state(slow, TERMINAL)
        seqs, after = [], 0
        for _ in range(100):
            status, page = hit("get", f"{S}/{slow}/events", "/api/agents/sessions/{id}/events",
                               params={"after": after, "limit": 2})
            seqs += [e["seq"] for e in page["events"]]
            after = page["next_after"]
            if page["terminal"] and not page["has_more"]:
                break
        out["cursor"] = {"seqs": seqs, "last_seq": api.session(slow)["last_seq"]}

        # Long-poll: held for `wait` when idle, released < 1 s after an event.
        hang = api.mock("hang", prompt="hold for long-polls")
        api.wait_state(hang, "running")
        last = api.session(hang)["last_seq"]
        started = time.monotonic()
        status, idle = hit("get", f"{S}/{hang}/events", "/api/agents/sessions/{id}/events",
                           params={"after": last, "wait": 1})
        idle_secs = time.monotonic() - started
        manager = api.manager
        threading.Timer(0.3, lambda: Sink(manager, hang).output("agent", "ping")).start()
        started = time.monotonic()
        status, woke = hit("get", f"{S}/{hang}/events", "/api/agents/sessions/{id}/events",
                           params={"after": last, "wait": 10})
        woke_secs = time.monotonic() - started
        out["long_poll"] = {"wait": 1, "idle_secs": round(idle_secs, 3), "idle_waited": idle.get("waited"),
                            "idle_events": len(idle.get("events", [])), "event_delay": 0.3,
                            "woke_secs": round(woke_secs, 3), "woke_texts": [e["text"] for e in woke.get("events", [])]}
        hit("post", f"{S}/{hang}/messages", "/api/agents/sessions/{id}/messages", json={"text": "still there?"})
        hit("delete", f"{S}/{hang}", "/api/agents/sessions/{id}")
        hit("post", f"{S}/{hang}/cancel", "/api/agents/sessions/{id}/cancel")

        # Change feed: updates, deletions and reset.
        status, start = hit("get", "/api/agents/changes", "/api/agents/changes")
        since = start["revision"]
        later: dict = {}
        threading.Timer(0.3, lambda: later.setdefault("id", api.mock("fail", prompt="a change arrives"))).start()
        began = time.monotonic()
        status, changed = hit("get", "/api/agents/changes", "/api/agents/changes", params={"since": since, "wait": 10})
        changed_secs = time.monotonic() - began
        wait_for(lambda: "id" in later, 5)
        api.wait_state(later["id"], TERMINAL)
        mark = api.local.get("/api/agents/changes").json()["revision"]
        hit("delete", f"{S}/{later['id']}", "/api/agents/sessions/{id}")
        status, deleted = hit("get", "/api/agents/changes", "/api/agents/changes", params={"since": mark})
        status, future = hit("get", "/api/agents/changes", "/api/agents/changes", params={"since": 10**9})
        out["changes"] = {"woke_secs": round(changed_secs, 3), "waited": changed.get("waited"),
                          "changed_ids": [s["id"] for s in changed.get("sessions", [])], "launched": later.get("id"),
                          "deleted": deleted.get("deleted"), "deleted_reset": deleted.get("reset"),
                          "future_reset": future.get("reset")}
        hit("delete", f"/api/agents/http-profiles/{profile['id']}", "/api/agents/http-profiles/{id}")
        hit("delete", f"/api/agents/http-profiles/{profile['id']}", "/api/agents/http-profiles/{id}")
        out["routes_hit"] = sorted(routes_hit)
        out["routes_all"] = [f"{m} {r}" for m, r in ROUTES]
        out["schema"] = [e for e in api.log if "schema_errors" in e]
    finally:
        stub.close()
    return out


# =================================================== A5 secrets & redaction ===

def scan(values: list[bytes], *, skip: Path) -> dict:
    files, leaks = 0, []
    for path in sorted(TMP.rglob("*")):
        if not path.is_file() or path.is_relative_to(skip):
            continue
        files += 1
        try:
            data = path.read_bytes()
        except OSError:
            continue
        if any(v in data for v in values):
            leaks.append(str(path.relative_to(TMP)))
    return {"files": files, "leaks": leaks}


def cell_secrets(cell: dict) -> dict:
    from localflow.agents.manager import AgentManager
    from localflow.agents.providers.base import ProviderContext
    from localflow.agents.store import AgentSessionStore
    from localflow.secrets import get_keyring_secret, get_secret

    capture_logs()
    api = Api()
    out: dict = {}
    devin_key = PLANTED["agent_devin_api_key"]

    # secret_echo: the mock prints the planted Devin key; it must come back redacted.
    echo = api.mock("secret_echo", prompt="Print the configured key")
    api.wait_state(echo, TERMINAL)
    events = api.events(echo)
    session = api.session(echo)
    out["secret_echo"] = {"state": session["state"],
                          "event_redacted": any("[redacted:agent_devin_api_key]" in e["text"] for e in events),
                          "result_redacted": "[redacted:agent_devin_api_key]" in session["result_text"]}
    # A planted key inside a prompt and title is stored redacted.
    status, body = api.launch(prompt=f"Deploy using the key {devin_key} please", provider="mock",
                              title=f"deploy {devin_key}", options={"script": "ok"})
    pid = body["session"]["id"]
    api.wait_state(pid, TERMINAL)
    stored = api.session(pid)
    out["prompt_redacted"] = {"status": status, "prompt": "[redacted:agent_devin_api_key]" in stored["prompt"],
                              "title": "[redacted:agent_devin_api_key]" in stored["title"]}

    # Devin over the stub: the key goes out as the bearer only; an echo is redacted.
    stub = Stub()
    stub2 = Stub()
    try:
        os.environ["LOCALFLOW_DEVIN_BASE_URL"] = stub.url
        stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-s1", "url": "https://app.devin.ai/sessions/devin-s1"}))
        stub.script("GET", "/v1/sessions/devin-s1", (200, {"status_enum": "finished", "messages": [
            {"type": "devin_message", "message": f"I authenticated with {devin_key} and finished."}]}))
        clock = FakeClock()
        sandbox_off = dataclasses.replace(api.server._config, agent_sandbox=False)
        manager = AgentManager(lambda: sandbox_off, store=AgentSessionStore(TMP / "home" / ".localflow" / "agents-a5"),
                               supervise=False, inline_start=True, clock=clock,
                               ctx=ProviderContext(get_secret=get_secret, get_stored_secret=get_keyring_secret))
        launched, _ = manager.launch({"prompt": "Check the CI status", "provider": "devin"})
        for _ in range(5):
            clock.advance(61)
            manager.tick()
        devin = manager.store.get(launched.id)
        devin_events = [e.to_dict() for e in manager.store.events(launched.id, limit=500)]
        requests_seen = stub.take()
        out["devin"] = {"state": devin.state, "auth": [r["auth"] for r in requests_seen],
                        "event_redacted": any("[redacted:agent_devin_api_key]" in e["text"] for e in devin_events),
                        "result_redacted": "[redacted:agent_devin_api_key]" in devin.result_text}
        manager.shutdown()

        # HTTP profiles: literal credentials refused; an origin change withholds the token.
        n0 = len(api.local.get("/api/agents/http-profiles").json()["profiles"])
        literal = []
        for headers in ({"Authorization": "Bearer abcdef0123456789"}, {"X-Api-Key": "literal-key-value"},
                        {"Cookie": "session=abc"}):
            status, body = api.post("/api/agents/http-profiles", "/api/agents/http-profiles", json={
                "label": "literal", "base_url": stub.url, "headers": headers,
                "start": {"method": "POST", "path": "/v1/runs"}})
            literal.append({"header": next(iter(headers)), "status": status,
                            "code": body.get("code") if isinstance(body, dict) else None})
        out["literal_header"] = literal
        out["literal_profiles_unchanged"] = len(api.local.get("/api/agents/http-profiles").json()["profiles"]) == n0
        status, profile = api.post("/api/agents/http-profiles", "/api/agents/http-profiles", json={
            "label": "Origin bound", "base_url": stub.url, "headers": {"Authorization": "Bearer {secret}"},
            "start": {"method": "POST", "path": "/v1/runs", "body": {"input": "{prompt}"}}})
        os.environ[http_token_env(profile["id"])] = PLANTED["agent_http_token"]
        listed = api.local.get("/api/agents/http-profiles").json()["profiles"]
        before = next(p for p in listed if p["id"] == profile["id"])["secret_set"]
        status, moved = api.call("put", f"/api/agents/http-profiles/{profile['id']}", "/api/agents/http-profiles/{id}",
                                 json={"base_url": stub2.url})[:2]
        providers = api.local.get("/api/agents/providers").json()["providers"]
        entry = next(p for p in providers if p["id"] == "http" and p["profile"] == profile["id"])
        api.set_config(agent_sandbox=False)
        stub2.script("POST", "/v1/runs", (200, {"id": "should-not-run"}))
        launch_status, launch_body = api.launch(prompt="Run on the moved origin", provider="http",
                                                profile=profile["id"])
        api.set_config(agent_sandbox=True)
        out["origin_change"] = {"secret_set_before": before, "status": status,
                                "secret_set_after": moved.get("secret_set") if isinstance(moved, dict) else None,
                                "readiness": entry["readiness"]["code"], "launch_status": launch_status,
                                "launch_code": launch_body.get("code") if isinstance(launch_body, dict) else None,
                                "new_origin_requests": [r["auth"] for r in stub2.take()]}
    finally:
        os.environ.pop("LOCALFLOW_DEVIN_BASE_URL", None)
        stub.close()
        stub2.close()

    # PUT /api/config cannot change the three local-only keys from a remote peer.
    config_path = Path(os.environ["LOCALFLOW_CONFIG"])
    disk_before = config_path.read_bytes()
    remote_puts = []
    for key, value in (("claude_bin", "/tmp/evil-claude"), ("repos", ["evil=/etc"]), ("allow_remote", True)):
        status, _, _ = api.call("put", "/api/config", client=api.remote, json={"agents": {key: value}})
        remote_puts.append({"key": key, "status": status})
    out["remote_config"] = {"puts": remote_puts, "disk_unchanged": config_path.read_bytes() == disk_before,
                            "live_unchanged": (api.server._config.agent_claude_bin, api.server._config.agent_allow_remote) == ("claude", False)}

    # Secret routes never echo a value (keyring is the fail backend: 503).
    status, body = api.call("put", "/api/agents/providers/devin/secret", "/api/agents/providers/{id}/secret",
                            json={"value": devin_key})[:2]
    out["secret_route"] = {"status": status, "code": body.get("code") if isinstance(body, dict) else None}
    api.get("/api/agents/providers")
    api.get("/api/agents/status")
    api.get("/api/secrets")
    for sid in (echo, pid):
        api.get(f"{S}/{sid}")
        api.get(f"{S}/{sid}/events")
    api.get(S)
    api.get("/api/agents/changes")

    # CLI with the server down (read commands fall back to the store).
    env = {**os.environ, "PYTHONPATH": str(SUT)}
    # Operator proxies must never intercept the CLI's loopback probes.
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        env.pop(name, None)
        env.pop(name.lower(), None)
    env["NO_PROXY"] = env["no_proxy"] = "127.0.0.1,localhost"
    cli_runs = []
    cli_out = b""
    for args in (["agents", "list", "--json"], ["agents", "show", echo, "--json"], ["agents", "logs", echo, "--json"],
                 ["agents", "show", pid], ["agents", "status"], ["agents", "rules", "list", "--json"]):
        proc = subprocess.run([sys.executable, "-m", "localflow.cli", *args], env=env, cwd=str(TMP),
                              capture_output=True, timeout=120, check=False)
        cli_out += proc.stdout + proc.stderr
        cli_runs.append({"args": args[:2], "exit": proc.returncode, "stdout_bytes": len(proc.stdout)})
    values = [v.encode() for v in PLANTED.values()]
    bodies = "\n".join(api.bodies).encode()
    files = scan(values, skip=HARNESS)
    out["cli"] = cli_runs
    out["leaks"] = {"files_scanned": files["files"], "file_leaks": files["leaks"],
                    "http_bodies": len(api.bodies), "http_leak": any(v in bodies for v in values),
                    "cli_leak": any(v in cli_out for v in values),
                    "cli_shows_redacted": b"[redacted:agent_devin_api_key]" in cli_out}
    out["http"] = api.log
    return out


# ================================================ A6 provider adapters ===

class _ProviderBench:
    """A real AgentManager on its own store, driven by an injected clock."""

    def __init__(self, base_config, http=None):
        from localflow.agents.manager import AgentManager
        from localflow.agents.providers.base import ProviderContext
        from localflow.agents.store import AgentSessionStore
        from localflow.secrets import get_keyring_secret, get_secret

        self.config = dataclasses.replace(base_config, agent_sandbox=False, agent_poll_seconds=2)
        self.clock = FakeClock()
        kwargs = {"get_secret": get_secret, "get_stored_secret": get_keyring_secret, "kill_grace_seconds": 0.5}
        if http is not None:
            kwargs["http"] = http
        (TMP / "home" / ".localflow").mkdir(parents=True, exist_ok=True)
        root = Path(tempfile.mkdtemp(prefix="agents-a6-", dir=TMP / "home" / ".localflow"))
        self.manager = AgentManager(lambda: self.config, store=AgentSessionStore(root), supervise=False,
                                    inline_start=True, clock=self.clock, ctx=ProviderContext(**kwargs))
        self.store = self.manager.store

    def configure(self, **changes):
        self.config = dataclasses.replace(self.config, **changes)

    def launch(self, body: dict) -> dict:
        from localflow.agents.models import AgentError

        try:
            session, _ = self.manager.launch(body)
            return {"id": session.id}
        except AgentError as exc:
            return {"error": exc.code, "detail": exc.detail,
                    "readiness": (exc.extra.get("readiness") or {}).get("code")}

    def wait(self, sid: str, timeout: float = 20.0):
        wait_for(lambda: self.store.get(sid).state in TERMINAL, timeout)
        return self.store.get(sid)

    def tick(self, seconds: float = 61.0) -> str:
        self.clock.advance(seconds)
        self.manager.tick()
        return self.store.get(self._last).state if getattr(self, "_last", None) else ""

    def view(self, sid: str) -> dict:
        s = self.store.get(sid)
        events = [e.to_dict() for e in self.store.events(sid, limit=500)]
        return {"id": sid, "state": s.state, "error": s.error, "result_text": s.result_text,
                "external_id": s.external_id, "external_url": s.external_url, "usage": s.usage,
                "states": [e["state"] for e in events if e["type"] == "state"],
                "outputs": [e["text"] for e in events if e["type"] == "output"],
                "seqs_contiguous": [e["seq"] for e in events] == list(range(1, s.last_seq + 1)),
                "prompt": s.prompt, "title": s.title, "raw_status": s.internal.get("raw_status"),
                "event_log": event_log_text(events),
                # The product's own list-summary preview (AgentSession.to_summary), never re-derived here.
                "result_preview": s.to_summary()["result_preview"]}


def cell_providers(cell: dict) -> dict:
    import requests

    from localflow import config as config_module
    from localflow.agents.providers.http import validate_profile

    spec = cell["providers"]
    base = config_module.load_config()
    out: dict = {"claude": {}, "devin": {}, "http": {}, "jev_error_items": [], "jev_result_items": []}

    # -- claude against the fake recorder --------------------------------------
    bench = _ProviderBench(base)

    def claude_run(name: str, prompt: str, *, mode: str = "ok", config: dict | None = None, **body):
        defaults = {"agent_claude_wrapper": "none", "agent_claude_stream": False,
                    "agent_claude_permission_mode": "default", "agent_claude_model": ""}
        bench.config = dataclasses.replace(bench.config, **{**defaults, **(config or {})})
        os.environ["FAKE_CLAUDE_MODE"] = mode
        seen = len(records())
        launched = bench.launch({"prompt": prompt, "provider": "claude", **body})
        if "id" not in launched:
            out["claude"][name] = launched
            return launched
        session = bench.wait(launched["id"])
        new = records()[seen:]
        out["claude"][name] = {**bench.view(session.id), "record": record_view(new[-1]) if new else None,
                               "invocations": len(new)}
        return out["claude"][name]

    ok = claude_run("base", spec["claude_prompt"])
    claude_run("resume", "Continue where you left off", resume_of=ok["id"])
    claude_run("headroom", spec["claude_prompt"], config={"agent_claude_wrapper": "headroom"})
    claude_run("permission_plan", spec["claude_prompt"], config={"agent_claude_permission_mode": "plan"})
    claude_run("model_config", spec["claude_prompt"], config={"agent_claude_model": "claude-sonnet-4-5"})
    claude_run("model_option", spec["claude_prompt"], options={"model": "claude-opus-4-1"})
    claude_run("stream", spec["claude_prompt"], mode="stream", config={"agent_claude_stream": True})
    for i, prompt in enumerate(spec["injection_prompts"]):
        claude_run(f"injection_{i}", prompt)
    for mode in ("is_error", "garbage", "nonzero"):
        claude_run(f"mode_{mode}", f"Scripted {mode} run", mode=mode)
    os.environ["FAKE_CLAUDE_MODE"] = "ok"
    bench.configure(agent_claude_bin=str(TMP / "nowhere" / "claude"))
    out["claude"]["missing_binary"] = bench.launch({"prompt": "Fix the tests", "provider": "claude"})
    bench.configure(agent_claude_bin="claude", agent_claude_wrapper="headroom")
    # PATH is ONLY the private shim dir here, so no real headroom on the host can satisfy the probe.
    saved_path = os.environ["PATH"]
    (BIN / "headroom").rename(HARNESS / "headroom.off")
    try:
        os.environ["PATH"] = str(BIN)
        out["claude"]["missing_wrapper_which"] = shutil.which("headroom")
        out["claude"]["missing_wrapper"] = bench.launch({"prompt": "Fix the tests", "provider": "claude"})
    finally:
        os.environ["PATH"] = saved_path
        (HARNESS / "headroom.off").rename(BIN / "headroom")
    bench.configure(agent_claude_wrapper="none")
    out["claude"]["repo_path"] = str(REPO.resolve())
    os.environ.pop("FAKE_CLAUDE_MODE", None)

    # -- devin and http against the loopback stub ------------------------------
    stub = Stub()
    os.environ["LOCALFLOW_DEVIN_BASE_URL"] = stub.url
    try:
        devin = _ProviderBench(base)

        def devin_case(name: str, routes: list, *, ticks: int, message: str = "", cancel: bool = False,
                       retry_after_probe: bool = False):
            for method, path, responses in routes:
                stub.script(method, path, *[tuple(r) for r in responses])
            stub.take()
            launched = devin.launch({"prompt": spec["devin_prompt"], "provider": "devin", "title": f"sim {name}"})
            row: dict = {"launch": launched, "states_after_tick": []}
            if "id" in launched:
                sid = launched["id"]
                devin._last = sid
                for i in range(ticks):
                    state = devin.tick()
                    row["states_after_tick"].append(state)
                    if retry_after_probe and i == 0:
                        tracked = devin.manager._tracked.get(sid) or {}
                        row["next_poll_in"] = (tracked.get("next_poll") or 0) - devin.clock()
                    if message and state == "waiting_input":
                        devin.manager.send_message(sid, message)
                        message = ""
                    if state in TERMINAL:
                        break
                if cancel:
                    devin.manager.cancel(sid)
                row.update(devin.view(sid))
            row["requests"] = stub.take()
            out["devin"][name] = row
            return row

        for case in spec["devin_cases"]:
            devin_case(case["name"], case["routes"], ticks=case["ticks"], message=case.get("message", ""),
                       cancel=case.get("cancel", False), retry_after_probe=case.get("retry_after_probe", False))
        # Timeouts: a transport that never answers a poll (retryable, then upstream_error).

        class TimeoutOnGet(requests.Session):
            def request(self, method, url, *args, **kwargs):
                if method == "GET":
                    raise requests.exceptions.ReadTimeout("sim: no answer")
                return super().request(method, url, *args, **kwargs)

        devin_t = _ProviderBench(base, http=TimeoutOnGet)
        stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-to", "url": ""}))
        stub.take()
        launched = devin_t.launch({"prompt": spec["devin_prompt"], "provider": "devin"})
        devin_t._last = launched["id"]
        states = [devin_t.tick() for _ in range(6)]
        out["devin"]["timeout"] = {**devin_t.view(launched["id"]), "states_after_tick": states,
                                   "requests": stub.take()}
        os.environ.pop("LOCALFLOW_AGENT_DEVIN_API_KEY")
        out["devin"]["missing_key"] = devin.launch({"prompt": "Fix the tests", "provider": "devin"})
        os.environ["LOCALFLOW_AGENT_DEVIN_API_KEY"] = PLANTED["agent_devin_api_key"]

        http = _ProviderBench(base)
        for case in spec["http_cases"]:
            profile_body = json.loads(json.dumps(case["profile"]).replace("{{stub}}", stub.url))
            profile = {"id": secrets.token_hex(4), **validate_profile(profile_body)}
            with http.manager.profiles.mutate() as profiles:
                profiles.append(profile)
            os.environ[http_token_env(profile["id"])] = PLANTED["agent_http_token"]
            for method, path, responses in case["routes"]:
                stub.script(method, path, *[tuple(r) for r in responses])
            stub.take()
            launched = http.launch({"prompt": case.get("prompt", spec["http_prompt"]), "provider": "http",
                                    "profile": profile["id"], "title": f"sim {case['name']}"})
            row = {"launch": launched, "states_after_tick": []}
            if "id" in launched:
                sid = launched["id"]
                http._last = sid
                message = case.get("message", "")
                for _ in range(case["ticks"]):
                    state = http.tick()
                    row["states_after_tick"].append(state)
                    if message and state == "waiting_input":
                        http.manager.send_message(sid, message)
                        message = ""
                    if state in TERMINAL:
                        break
                if case.get("cancel"):
                    http.manager.cancel(sid)
                row.update(http.view(sid))
                row["session_id"] = sid
            row["requests"] = stub.take()
            out["http"][case["name"]] = row
    finally:
        os.environ.pop("LOCALFLOW_DEVIN_BASE_URL", None)
        stub.close()

    # Jev JA3 (error helpfulness) and JA4 (result faithfulness) inputs.
    for key, cause, row in (
            ("claude:missing_binary", "The claude command is not installed / not on PATH.", out["claude"]["missing_binary"]),
            ("claude:missing_wrapper", "The headroom wrapper command is not installed.", out["claude"]["missing_wrapper"]),
            ("devin:missing_key", "No Devin API key is configured.", out["devin"]["missing_key"])):
        out["jev_error_items"].append({"id": key, "cause": cause, "detail": row.get("detail", ""), "error_message": ""})
    for key, cause, row in (
            ("claude:mode_garbage", "claude printed output that is not the expected JSON result.", out["claude"]["mode_garbage"]),
            ("claude:mode_nonzero", "claude exited with status 3 because the CLI is not logged in.", out["claude"]["mode_nonzero"]),
            ("claude:mode_is_error", "The Claude account has too little credit to run.", out["claude"]["mode_is_error"]),
            ("devin:auth_failed", "The Devin API rejected the API key (HTTP 401).", out["devin"]["auth_failed"]),
            ("http:auth_failed", "The HTTP agent rejected the token (HTTP 401).", out["http"]["auth_failed"])):
        out["jev_error_items"].append({"id": key, "cause": cause, "detail": "",
                                       "error_message": (row.get("error") or {}).get("message", "")})
    for key, row in (("claude:base", out["claude"]["base"]), ("devin:lifecycle", out["devin"]["lifecycle"]),
                     ("http:lifecycle", out["http"]["lifecycle"])):
        out["jev_result_items"].append({"id": key, "prompt": row["prompt"], "event_log": row["event_log"],
                                        "result_text": row["result_text"], "result_preview": row["result_preview"]})
    return out


# ===================================================== A7 idle & efficiency ===

_IDLE_CHILD = r"""
import json, sys, threading
import numpy as np
from unittest.mock import Mock
from localflow import app
from localflow.config import load_config
from localflow.dictionary import DictionaryStore
from localflow.history import HistoryStore
from localflow.hotphrases import HotPhraseStore
from localflow.jobs import Job
tmp = sys.argv[1]
loaded = lambda: sorted(m for m in sys.modules if m.startswith("localflow.agents"))
config = load_config()
app.paste_text = lambda text, method: None
text, error = app._process_job(
    Job(kind="dictate", audio=np.full(16000, 0.1, np.float32)), config,
    Mock(transcribe=Mock(return_value="launch claude to fix the flaky test")),
    Mock(clean=Mock(side_effect=lambda v, **k: v)), Mock(), (),
    HotPhraseStore(tmp + "/phrases.json"), DictionaryStore(tmp + "/dictionary.json"),
    HistoryStore(tmp + "/history.sqlite", tmp + "/clips"))
after_job = loaded()
from fastapi.testclient import TestClient
from localflow import server
client = TestClient(server.app, base_url="http://127.0.0.1:%d" % server._config.server_port,
                    client=("127.0.0.1", 1), headers={"X-Localflow-Token": server._api_token})
statuses = {p: client.get(p).status_code for p in ("/healthz", "/api/config", "/api/secrets", "/prompts")}
print(json.dumps({"agents_enabled": config.agents_enabled, "text": text, "after_job": after_job,
                  "after_server": loaded(), "statuses": statuses,
                  "manager_module": "localflow.agents.manager" in sys.modules,
                  "threads": [t.name for t in threading.enumerate() if t.name.startswith("lf-agents")]}))
"""


def cell_idle(cell: dict) -> dict:
    from localflow.agents import hotcommands
    from localflow.agents.handoff import HotCommandLauncher

    perf = cell["perf"]
    out: dict = {}
    # Feature off: a fresh process runs a dictation job and non-agent routes.
    off = TMP / "config-off.toml"
    text = Path(os.environ["LOCALFLOW_CONFIG"]).read_text(encoding="utf-8")
    off.write_text(text.replace("enabled = true\nhot_commands", "enabled = false\nhot_commands"), encoding="utf-8")
    env = {**os.environ, "LOCALFLOW_CONFIG": str(off), "PYTHONPATH": str(SUT)}
    (TMP / "off").mkdir(exist_ok=True)
    proc = subprocess.run([sys.executable, "-c", _IDLE_CHILD, str(TMP / "off")], env=env, cwd=str(TMP),
                          capture_output=True, text=True, timeout=180, check=False)
    try:
        out["feature_off"] = json.loads(proc.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        out["feature_off"] = {"exit": proc.returncode, "stderr": proc.stderr[-1500:]}
    out["feature_off_agents_dir"] = (TMP / "home" / ".localflow" / "agents").exists()

    # Feature on, no sessions: nothing runs until the first launch.
    def agent_threads() -> list[str]:
        return sorted(t.name for t in threading.enumerate() if t.name.startswith("lf-agents"))

    api = Api()
    for path in ("/api/agents/status", "/api/agents/providers", S, "/api/agents/changes"):
        api.get(path)
    manager = api.manager
    out["feature_on_idle"] = {"threads": agent_threads(), "providers": len(manager._providers),
                              "supervisor": manager.supervisor_alive}
    sid = api.mock("ok", prompt="one short run")
    api.wait_state(sid, TERMINAL)
    ended = time.monotonic()
    wait_for(lambda: not manager.supervisor_alive, 3.0, 0.005)
    exit_secs = time.monotonic() - ended
    wait_for(lambda: not [t for t in agent_threads() if t.startswith("lf-agents-supervisor")], 3.0, 0.005)
    out["supervisor_exit_secs"] = round(exit_secs, 3)
    out["threads_after"] = [t for t in agent_threads() if t.startswith("lf-agents-supervisor")]

    # match_hot_command latency: 32 rules, 1000 utterances.
    rules = [{"id": f"{i:08x}", "trigger": f"launch agent {word}", "provider": "claude", "enabled": True,
              "prompt_template": "{prompt}", "min_prompt_words": 3}
             for i, word in enumerate(perf["rule_words"][:perf["rules"]])]
    compiled = hotcommands.compile_rules(rules, [f"{REPO_ALIAS}={REPO}", "web app=/tmp/web"], REPO_ALIAS)
    utterances = []
    for i in range(perf["utterances"]):
        word = perf["rule_words"][i % len(perf["rule_words"])]
        utterances.append([f"launch agent {word} in simrepo to fix bug number {i} in the parser",
                           f"so I was thinking about item {i} and the launch agent {word} idea",
                           f"launch agent {word} to {i}"][i % 3])
    samples, matched = [], 0
    for text in utterances:
        started = time.perf_counter()
        matched += hotcommands.match_hot_command(text, compiled) is not None
        samples.append((time.perf_counter() - started) * 1000)
    samples.sort()
    out["match"] = {"n": len(samples), "rules": len(rules), "matched": matched,
                    "p50_ms": round(statistics.median(samples), 4),
                    "p95_ms": round(samples[int(0.95 * len(samples)) - 1], 4), "max_ms": round(samples[-1], 4)}

    # HotCommandLauncher.submit with the server down: returns at once.
    down = f"http://127.0.0.1:{free_port()}"
    cues: list[str] = []
    launcher = HotCommandLauncher(base_url=lambda: down, token=lambda: None, play=cues.append,
                                  sounds_enabled=lambda: True)
    timings = []
    for i in range(5):
        started = time.perf_counter()
        launcher.submit(hotcommands.HotCommandMatch("r1", "claude", "", REPO_ALIAS, f"fix bug number {i} now"))
        timings.append((time.perf_counter() - started) * 1000)
    launcher.join(timeout=30)
    out["submit"] = {"ms": [round(t, 3) for t in timings], "max_ms": round(max(timings), 3), "cues": cues}
    return out


# ================================================ A8 recovery & cross-process ===

_SERVER_CHILD = r"""
import json, os, sys, time
from fastapi.testclient import TestClient
from localflow import server
import dataclasses
server._config = dataclasses.replace(server._config, agent_sandbox=False, agent_max_concurrent=2,
                                     agent_poll_seconds=2)
client = TestClient(server.app, base_url="http://127.0.0.1:%d" % server._config.server_port,
                    client=("127.0.0.1", 1), headers={"X-Localflow-Token": server._api_token})
S = "/api/agents/sessions"
ids = {}
ids["claude"] = client.post(S, json={"prompt": "Run until killed", "provider": "claude"}).json()["session"]["id"]
ids["devin"] = client.post(S, json={"prompt": "Long cloud task", "provider": "devin"}).json()["session"]["id"]
ids["queued"] = client.post(S, json={"prompt": "Wait for a slot", "provider": "claude"}).json()["session"]["id"]
def state(sid):
    return client.get(S + "/" + sid).json()["session"]["state"]
deadline = time.monotonic() + 30
while time.monotonic() < deadline:
    states = {k: state(v) for k, v in ids.items()}
    if states == {"claude": "running", "devin": "running", "queued": "queued"}:
        break
    time.sleep(0.05)
print(json.dumps({"ids": ids, "states": states}), flush=True)
time.sleep(600)
"""

_RULE_WRITER = r"""
import json, os, sys, time
from pathlib import Path
from localflow.agents.store import HotCommandStore
path, me, count, go = sys.argv[1], sys.argv[2], int(sys.argv[3]), Path(sys.argv[4])
store = HotCommandStore(Path(path))
(go.parent / (go.name + "." + me + ".ready")).write_text("ready", encoding="utf-8")  # start barrier, part 1
while not go.exists():  # start barrier, part 2: the parent releases both writers at once
    time.sleep(0.0005)
bad = 0
for i in range(count):
    with store.mutate() as rules:
        rules.append({"id": f"{me}-{i:03d}", "trigger": f"sim writer {me} {i}", "provider": "claude",
                      "enabled": True})
    try:
        raw = Path(path).read_text(encoding="utf-8")
        if not isinstance(json.loads(raw), list):
            bad += 1
    except ValueError:
        bad += 1
print(json.dumps({"writer": me, "unreadable": bad}))
"""


def cell_recovery(cell: dict) -> dict:
    from unittest.mock import Mock

    import numpy as np

    from localflow import app
    from localflow.agents.handoff import HotCommandLauncher, HotCommandSeam
    from localflow.agents.store import HotCommandStore
    from localflow.dictionary import DictionaryStore
    from localflow.history import HistoryStore
    from localflow.hotphrases import HotPhraseStore
    from localflow.jobs import Job

    out: dict = {}
    stub = Stub()
    env = {**os.environ, "PYTHONPATH": str(SUT), "LOCALFLOW_DEVIN_BASE_URL": stub.url, "FAKE_CLAUDE_MODE": "hang"}
    stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-r1", "url": "https://app.devin.ai/sessions/devin-r1"}))
    stub.script("GET", "/v1/sessions/devin-r1", (200, {"status_enum": "working", "messages": []}))
    seen = len(records())

    def spawned() -> list[dict]:
        return [r for r in records()[seen:] if r.get("pid")]

    def reap(recs: list[dict]) -> None:
        """SIGKILL each fake claude's own process group and wait until it is gone (never orphaned)."""
        for rec in recs:
            pgid = rec.get("pgid") or rec["pid"]
            with contextlib.suppress(OSError):
                if pgid == os.getpgrp():  # never our own group: fall back to the single process
                    os.kill(rec["pid"], signal.SIGKILL)
                else:
                    os.killpg(pgid, signal.SIGKILL)
        for rec in recs:
            wait_for(lambda rec=rec: not _pid_alive(rec["pid"]), 10, 0.02)

    try:
        # 1. A real server process is SIGKILLed mid-run, then a new manager recovers.
        child = subprocess.Popen([sys.executable, "-c", _SERVER_CHILD], env=env, cwd=str(TMP),
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                 start_new_session=True)
        try:  # the child prints one JSON line once its three sessions are in place
            wait_for(lambda: child.poll() is not None or select_line(child), 60, 0.05)
            line = _LINES.get(child.pid, "")
            if line:  # the claude session is "running": kill only once fake claude has recorded its PID
                wait_for(lambda: spawned() or child.poll() is not None, 15, 0.02)
        finally:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=10)
            # A spawn already in flight at SIGKILL time still records itself: poll briefly for it.
            wait_for(spawned, 0 if spawned() else 2, 0.05)
            orphans = spawned()
            reap(orphans)  # fake claude ran in its own group; the dead server cannot reap it
        before = json.loads(line) if line else {"ids": {}, "states": {}}
        out["before_kill"] = before["states"]
        out["killed_exit"] = child.returncode
        out["claude_spawned"] = len(orphans)
        out["claude_orphans_alive"] = sum(1 for r in orphans if _pid_alive(r["pid"]))
        stub.script("GET", "/v1/sessions/devin-r1", (200, {"status_enum": "finished", "messages": [
            {"type": "devin_message", "message": "Recovered after the restart and finished."}]}))
        stub.take()
        os.environ["LOCALFLOW_DEVIN_BASE_URL"] = stub.url
        api = Api()
        api.set_config(agent_sandbox=False, agent_poll_seconds=2)
        ids = before["ids"]
        api.get("/api/agents/status")  # first agents request: the manager is built and recovers
        finals = {}
        for key, sid in ids.items():
            final = api.wait_state(sid, TERMINAL, timeout=15)
            finals[key] = {"state": final["state"], "error": (final["error"] or {}).get("code")}
        out["after_restart"] = finals
        out["devin_requests"] = [(r["method"], r["path"], r["auth"]) for r in stub.take()]
        out["devin_result"] = api.session(ids["devin"])["result_text"] if ids.get("devin") else ""
    finally:
        os.environ.pop("LOCALFLOW_DEVIN_BASE_URL", None)
        stub.close()

    # 2. Two processes, 200 interleaved rule writes through the store's locks.
    rules_path = TMP / "home" / ".localflow" / "agents" / "hot_commands-xproc.json"
    go = TMP / "xproc.go"
    ready = [go.parent / f"{go.name}.{name}.ready" for name in ("w1", "w2")]
    for path in (go, *ready):
        path.unlink(missing_ok=True)
    count = cell["xproc"]["writes_per_process"]
    writers = [subprocess.Popen([sys.executable, "-c", _RULE_WRITER, str(rules_path), name, str(count), str(go)],
                                env={**os.environ, "PYTHONPATH": str(SUT)}, cwd=str(TMP),
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for name in ("w1", "w2")]
    # Start barrier: release only once BOTH writers are imported and parked, so they genuinely contend.
    barrier = bool(wait_for(lambda: all(p.exists() for p in ready) or any(w.poll() is not None for w in writers),
                            60, 0.005)) and all(p.exists() for p in ready)
    go.write_text("go", encoding="utf-8")
    reports = []
    for proc in writers:
        stdout, stderr = proc.communicate(timeout=120)
        try:
            reports.append({"exit": proc.returncode, **json.loads(stdout.strip().splitlines()[-1])})
        except (ValueError, IndexError):
            reports.append({"exit": proc.returncode, "stderr": stderr[-800:]})
    raw = rules_path.read_text(encoding="utf-8")
    entries = json.loads(raw)
    ids_written = [e["id"] for e in entries]
    owners = [i.split("-")[0] for i in ids_written]
    out["xproc"] = {"writers": reports, "barrier": barrier, "entries": len(entries), "unique": len(set(ids_written)),
                    "expected": 2 * count,
                    "missing": sorted({f"{w}-{i:03d}" for w in ("w1", "w2") for i in range(count)} - set(ids_written)),
                    "alternations": sum(1 for a, b in itertools.pairwise(owners) if a != b),
                    "per_writer_in_order": all(
                        [i for i in ids_written if i.startswith(w)] == [f"{w}-{n:03d}" for n in range(count)]
                        for w in ("w1", "w2"))}

    # 3. A hot command while the server is down: no paste, history row, warning, no exception.
    cfg = dataclasses.replace(api.server._config, agents_enabled=True, agent_hot_commands=True, sounds_enabled=True)
    store = HotCommandStore(TMP / "a8-hot_commands.json")
    with store.mutate() as rules:
        rules.append({"id": "a8000001", "trigger": "launch claude", "provider": "claude", "profile": "",
                      "repo": "", "prompt_template": "{prompt}", "min_prompt_words": 3, "enabled": True})
    cues: list[str] = []
    down = f"http://127.0.0.1:{free_port()}"
    launcher = HotCommandLauncher(base_url=lambda: down, token=lambda: "sim", play=cues.append,
                                  sounds_enabled=lambda: True)
    seam = HotCommandSeam(store, launcher, known_provider=api.manager.known_provider)
    records_seen: list[logging.LogRecord] = []

    class Collect(logging.Handler):
        def emit(self, record):
            records_seen.append(record)

    handler = Collect(level=logging.DEBUG)
    logging.getLogger("localflow").addHandler(handler)
    logging.getLogger("localflow").setLevel(logging.DEBUG)
    history = HistoryStore(TMP / "a8-history.sqlite", TMP / "a8-clips")
    pasted: list[str] = []
    app.paste_text = lambda text, method: pasted.append(text)
    utterance = "Launch Claude to fix the flaky hot phrase test"
    try:
        result = app._process_job(
            Job(kind="dictate", audio=np.full(16000, 0.1, np.float32)), cfg,
            Mock(transcribe=Mock(return_value=utterance)), Mock(clean=Mock(side_effect=lambda v, **_k: v)),
            Mock(), (), HotPhraseStore(TMP / "a8-phrases.json"), DictionaryStore(TMP / "a8-dictionary.json"),
            history, hot_command_seam=seam)
        raised = ""
    except Exception as exc:  # noqa: BLE001 - recorded, judged by A8
        result, raised = None, f"{type(exc).__name__}: {exc}"
    launcher.join(timeout=30)
    logging.getLogger("localflow").removeHandler(handler)
    row = history.recent(limit=1)[0] if history.recent(limit=1) else {}
    messages = [r.getMessage() for r in records_seen if r.name.startswith("localflow.agents")]
    out["server_down"] = {
        "returned": list(result) if result else None, "raised": raised, "pasted": pasted, "cues": cues,
        "history": {k: row.get(k) for k in ("kind", "raw_text", "final_text", "status")},
        "warning": any(r.levelno == logging.WARNING and "server unreachable" in r.getMessage() for r in records_seen),
        "exception_logged": any(r.exc_info for r in records_seen if r.name.startswith("localflow.agents")),
        "prompt_in_logs": any("flaky hot phrase" in m for m in messages),
    }
    return out


_LINES: dict[int, str] = {}


def _pid_alive(pid: int) -> bool:
    """True while `pid` exists and is not a zombie."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    with contextlib.suppress(OSError):
        return Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1][:1] != "Z"
    return True


def select_line(proc) -> bool:
    """Non-blocking read of the child's first stdout line."""
    import select

    if proc.pid in _LINES:
        return True
    ready, _, _ = select.select([proc.stdout], [], [], 0)
    if ready:
        line = proc.stdout.readline()
        if line.strip():
            _LINES[proc.pid] = line.strip()
            return True
    return False


RUNNERS = {
    "agents_lifecycle": cell_lifecycle, "agents_hotcommands": cell_hotcommands,
    "agents_concurrency": cell_concurrency, "agents_api": cell_api, "agents_secrets": cell_secrets,
    "agents_providers": cell_providers, "agents_idle": cell_idle, "agents_recovery": cell_recovery,
}
