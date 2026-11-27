"""Shared helpers for tests/test_agents_*.py (no network, no vendor CLIs)."""

from __future__ import annotations

import json
import re
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
SCHEMA_PATH = ROOT / "docs" / "contracts" / "agents-api.v1.schema.json"
SCHEMA = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
FIXTURES = Path(__file__).resolve().parent
TOKEN = "test-token"


# -- a small JSON Schema checker (the subset the contract uses) ------------------------

_TYPES = {
    "object": lambda v: isinstance(v, dict),
    "array": lambda v: isinstance(v, list),
    "string": lambda v: isinstance(v, str),
    "boolean": lambda v: isinstance(v, bool),
    "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
    "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
    "null": lambda v: v is None,
}


def _errors(value, schema: dict, path: str) -> list[str]:
    if "$ref" in schema:
        name = schema["$ref"].rsplit("/", 1)[-1]
        return _errors(value, SCHEMA["$defs"][name], path)
    errors: list[str] = []
    if "const" in schema and value != schema["const"]:
        return [f"{path}: {value!r} != const {schema['const']!r}"]
    if "enum" in schema and value not in schema["enum"]:
        return [f"{path}: {value!r} not in {schema['enum']}"]
    if "oneOf" in schema:
        passing = [s for s in schema["oneOf"] if not _errors(value, s, path)]
        if len(passing) != 1:
            return [f"{path}: matches {len(passing)} of oneOf"]
    if "type" in schema:
        types = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
        if not any(_TYPES[t](value) for t in types):
            return [f"{path}: {type(value).__name__} is not {types}"]
    if isinstance(value, str):
        if "pattern" in schema and not re.search(schema["pattern"], value):
            errors.append(f"{path}: {value!r} does not match {schema['pattern']}")
        if "maxLength" in schema and len(value) > schema["maxLength"]:
            errors.append(f"{path}: longer than {schema['maxLength']}")
    if _TYPES["number"](value):
        if "minimum" in schema and value < schema["minimum"]:
            errors.append(f"{path}: {value} < {schema['minimum']}")
        if "maximum" in schema and value > schema["maximum"]:
            errors.append(f"{path}: {value} > {schema['maximum']}")
    if isinstance(value, dict):
        props = schema.get("properties", {})
        for name in schema.get("required", []):
            if name not in value:
                errors.append(f"{path}: missing {name}")
        extra = schema.get("additionalProperties", True)
        for name, item in value.items():
            if name in props:
                errors += _errors(item, props[name], f"{path}.{name}")
            elif extra is False:
                errors.append(f"{path}: unexpected property {name}")
            elif isinstance(extra, dict):
                errors += _errors(item, extra, f"{path}.{name}")
    if isinstance(value, list) and "items" in schema:
        for index, item in enumerate(value):
            errors += _errors(item, schema["items"], f"{path}[{index}]")
    return errors


def validate(value, name: str) -> None:
    errors = _errors(value, SCHEMA["$defs"][name], "$")
    assert not errors, f"{name} schema violations: {errors[:8]}"


def validate_response(response, method: str, route: str) -> dict:
    """Check `response` against the x-routes entry for METHOD ROUTE."""
    status = str(response.status_code)
    table = SCHEMA["x-routes"][f"{method} {route}"]
    assert status in table, f"{method} {route}: unexpected status {status}: {response.text}"
    body = response.json()
    validate(body, table[status])
    return body


# -- environment ----------------------------------------------------------------------


class FakeKeyring:
    def __init__(self):
        self.values: dict[tuple[str, str], str] = {}

    def get_password(self, service, name):
        return self.values.get((service, name))

    def set_password(self, service, name, value):
        self.values[(service, name)] = value

    def delete_password(self, service, name):
        if (service, name) not in self.values:
            raise KeyError(name)
        del self.values[(service, name)]


def isolate(tmp_path: Path, monkeypatch) -> FakeKeyring:
    """Private HOME/config/agents dir, no real secrets, an in-memory keyring."""
    from localflow import secrets as secrets_module

    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("USERPROFILE", str(home))
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))
    monkeypatch.setenv("LOCALFLOW_AGENTS_DIR", str(home / ".localflow" / "agents"))
    for name in ("DEVIN_API_KEY", "ANTHROPIC_API_KEY", "LOCALFLOW_AGENT_DEVIN_API_KEY",
                 "LOCALFLOW_AGENT_CLAUDE_API_KEY", "LOCALFLOW_DEVIN_BASE_URL"):
        monkeypatch.delenv(name, raising=False)
    keyring = FakeKeyring()
    monkeypatch.setattr(secrets_module, "keyring", keyring)
    return keyring


def make_repo(tmp_path: Path, name: str = "proj") -> Path:
    repo = tmp_path / "repos" / name
    repo.mkdir(parents=True, exist_ok=True)
    return repo


def agents_config(tmp_path: Path, **overrides):
    from localflow.config import Config

    repo = make_repo(tmp_path)
    values = {
        "meetings_enabled": False, "meeting_watch": False, "agents_enabled": True,
        "agent_sandbox": True, "agent_repos": [f"proj={repo}"], "agent_default_repo": "proj",
        "agent_default_provider": "claude",
    }
    values.update(overrides)
    return Config(**values)


def make_manager(tmp_path: Path, config_ref, **kwargs):
    """A manager over a private store. `config_ref` is a Config or a callable."""
    from localflow.agents.manager import AgentManager
    from localflow.agents.providers.base import ProviderContext
    from localflow.agents.store import AgentSessionStore
    from localflow.secrets import get_secret

    getter = config_ref if callable(config_ref) else (lambda: config_ref)
    root = tmp_path / "home" / ".localflow" / "agents"
    ctx = kwargs.pop("ctx", None) or ProviderContext(get_secret=get_secret,
                                                     mock_poll_seconds=0.01,
                                                     kill_grace_seconds=0.5)
    return AgentManager(getter, store=kwargs.pop("store", None) or AgentSessionStore(root),
                        ctx=ctx, **kwargs)


class FakeClock:
    def __init__(self, start: float = 1000.0):
        self.now = start

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


def drive(manager, clock: FakeClock, session_id: str, *, steps: int = 200, step: float = 1.0):
    """Advance the fake clock and tick until the session ends."""
    for _ in range(steps):
        session = manager.store.get(session_id)
        if session is not None and session.state in ("completed", "failed", "cancelled",
                                                      "timed_out"):
            return session
        clock.advance(step)
        manager.tick()
    return manager.store.get(session_id)


def wait_for(predicate, timeout: float = 10.0, interval: float = 0.02):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    raise AssertionError("condition not reached in time")


def api_client(monkeypatch, config, manager):
    """TestClient on the real server app, loopback peer and token."""
    from fastapi.testclient import TestClient

    from localflow import server
    from localflow.agents import api as agents_api

    monkeypatch.setattr(server, "_config", config)
    monkeypatch.setattr(server, "_api_token", TOKEN)
    monkeypatch.setattr(server, "load_or_create", lambda: TOKEN)
    agents_api.set_manager(manager)
    return TestClient(server.app, base_url="http://127.0.0.1:8756",
                      client=("127.0.0.1", 50000), headers={"X-Localflow-Token": TOKEN})


def fake_bin(tmp_path: Path, *names: str) -> Path:
    """A PATH dir whose NAMES run fake_claude.py with this interpreter."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir(exist_ok=True)
    script = FIXTURES / "fake_claude.py"
    for name in names:
        path = bin_dir / name
        path.write_text(f"#!{sys.executable}\nimport os, runpy\n"
                        f"os.environ['FAKE_CLAUDE_NAME'] = {name!r}\n"
                        f"runpy.run_path({str(script)!r}, run_name='__main__')\n",
                        encoding="utf-8")
        path.chmod(0o755)
    return bin_dir


def read_records(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line]
