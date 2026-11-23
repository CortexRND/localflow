"""Wire parity between the Python and Rust LocalFlow MCP stdio processes."""

import json
import os
import sqlite3
import subprocess
import sys
from datetime import datetime, timedelta
from pathlib import Path

import pytest

from localflow.history import HistoryStore

ROOT = Path(__file__).resolve().parents[1]
RUST = ROOT / "rust"
RUST_MCP = RUST / Path(os.environ.get("CARGO_TARGET_DIR") or "target") / "debug" / (
    "localflow-mcp.exe" if os.name == "nt" else "localflow-mcp"
)


@pytest.fixture(scope="module", autouse=True)
def require_rust_binary():
    if not RUST_MCP.is_file():
        reason = "Build the Rust MCP server with cargo build --bin localflow-mcp in rust/"
        if os.environ.get("LF_RUST_PARITY_REQUIRED") == "1":
            pytest.fail(reason)
        pytest.skip(reason)


@pytest.fixture
def sandbox(tmp_path):
    home = tmp_path / "profile"
    home.mkdir()
    config = tmp_path / "config.toml"
    inherited = ("PATH", "SYSTEMROOT", "WINDIR", "SYSTEMDRIVE", "TEMP", "TMP", "TMPDIR", "TZ")
    env = {
        **{key: os.environ[key] for key in inherited if key in os.environ},
        "HOME": str(home),
        "USERPROFILE": str(home),
        "XDG_CONFIG_HOME": str(tmp_path / "xdg"),
        "APPDATA": str(tmp_path / "appdata"),
        "LOCALAPPDATA": str(tmp_path / "localappdata"),
        "LOCALFLOW_CONFIG": str(config),
        "PYTHONPATH": str(ROOT),
        "PYTHONIOENCODING": "utf-8",
        # Importing Python recall can initialize PortAudio. Don't open the real
        # PulseAudio socket or let the Pulse client create a runtime directory.
        "PULSE_SERVER": f"unix:{tmp_path / 'no-pulse.sock'}",
    }
    return home, config, env


def request(method, **params):
    return {"jsonrpc": "2.0", "id": method, "method": method, "params": params}


def call(name, **args):
    return {**request("tools/call", name=name, arguments=args), "id": name}


def snapshot(root):
    return {
        p.relative_to(root): (p.stat().st_mode, p.stat().st_mtime_ns, p.read_bytes())
        if p.is_file() else None
        for p in root.rglob("*")
    }


def compare(sandbox, *requests):
    home, _, env = sandbox
    before = snapshot(home.parent)
    payload = b"\n".join(
        msg if isinstance(msg, bytes) else json.dumps(msg).encode("utf-8")
        for msg in requests
    )  # EOF immediately after the last message is valid too.
    outputs = []
    for command in ([sys.executable, "-m", "localflow.mcp_server"], [str(RUST_MCP)]):
        run = subprocess.run(
            command, input=payload, capture_output=True, cwd=ROOT, env=env, timeout=20,
            check=False,
        )
        assert run.returncode == 0, run.stderr.decode("utf-8", errors="replace")
        assert not run.stdout or run.stdout.endswith(b"\n")
        replies = [json.loads(line) for line in run.stdout.splitlines()]
        for reply in replies:
            info = reply.get("result", {}).get("serverInfo")
            if info is not None:
                assert isinstance(info.pop("version"), str)
            result = reply.get("result", {})
            if "structuredContent" in result:
                content, = result["content"]
                assert content["type"] == "text"
                assert json.loads(content["text"]) == result["structuredContent"]
                # JSON number formatting is not part of the text payload contract.
                content["text"] = json.loads(content["text"])
        outputs.append(replies)
        assert snapshot(home.parent) == before, f"{command} modified its sandbox"
    python, rust = outputs
    assert rust == python
    return [reply.get("result", reply.get("error")) for reply in rust]


def test_rpc_and_tool_schemas(sandbox):
    replies = compare(
        sandbox,
        b"   ",
        request("initialize", protocolVersion="2024-11-05"),
        request("initialize", protocolVersion="2025-03-26"),
        request("initialize", protocolVersion="2099-12-31"),
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 29, "result": {}},
        b"{malformed",
        b'"\xff"',
        b"[]",
        12,
        request("tools/list"),
        request("tools/call", name="no_such_tool"),
        request("tools/call", name=["no_such_tool"]),
        request("tools/call", name="get_dictation", arguments=[]),
        request("tools/call", name="get_dictation"),
        call("get_dictation", id=None),
        call("get_dictation", id=True),
        call("get_dictation", id=1.0),
        call("get_prompt", id=1),
        call("search_dictations", kind="typo"),
        call("search_prompts", status="typo"),
        call("search_prompts", z=1, a=2),
        {**request("ping"), "params": ["not an object"]},
        request("resources/list"),
        request("ping"),
    )
    assert len(replies) == 22
    assert {tool["name"] for tool in replies[7]["tools"]} == {
        "search_dictations", "get_dictation", "search_prompts", "get_prompt",
    }
    assert [reply["protocolVersion"] for reply in replies[:3]] == [
        "2024-11-05", "2025-03-26", "2025-06-18",
    ]


def test_dictation_search_and_get(sandbox):
    home, _, _ = sandbox
    path = home / ".localflow" / "history.sqlite"
    history = HistoryStore(path)
    today = datetime.now().replace(hour=12, minute=0, second=0, microsecond=0)
    day = today - timedelta(days=3)
    pricing = history.add(
        kind="dictate", app_name="Slack", bundle_id="com.example.slack",
        category="work", raw_text="pricing page", final_text="Café pricing page",
        ts=day.timestamp(), duration_s=3.75,
    )
    command = history.add(
        kind="command", app_name="Mail", raw_text="haiku instructions cucumber",
        final_text="Zucchini supplies", ts=(today - timedelta(days=10)).timestamp(),
    )
    failed = history.add(
        kind="dictate", app_name="Slack", status="failed", final_text="private",
        ts=today.timestamp(),
    )
    long = history.add(
        kind="transform", final_text="🌿" * 20_001, ts=day.timestamp(),
    )
    history.add(kind="dictate", raw_text="  ", ts=today.timestamp())
    if os.name != "nt":
        path.chmod(0o640)  # A reader must not adopt HistoryStore's 0600 policy.
    results = compare(
        sandbox,
        call("search_dictations"),
        call("search_dictations", limit=0),
        call("search_dictations", query='"pricing page"', app=" SLACK "),
        call("search_dictations", query="haiku", kind="command"),
        call("search_dictations", query="cumb"),
        call("search_dictations", query="no such content", limit=None),
        call("search_dictations", app="com.example.slack"),
        call("search_dictations", since=day.date().isoformat(), until=day.date().isoformat()),
        call("search_dictations", since="7 D", until="today"),
        call("search_dictations", until="7d"),
        call("search_dictations", since="last Tuesday"),
        call("search_dictations", until="soon"),
        call("get_dictation", id=pricing),
        call("get_dictation", id=failed),
        call("get_dictation", id=long),
        call("get_dictation", id=-1),
    )
    assert [item["id"] for item in results[0]["structuredContent"]["results"]] == [
        long, pricing, command,
    ]
    assert results[4]["structuredContent"]["match"] == "substring"
    assert results[13]["structuredContent"]["status"] == "failed"
    assert results[14]["structuredContent"]["truncated"]
    assert len(results[14]["structuredContent"]["final_text"]) == 20_000


def test_ranked_limit_counts_dictations_not_passages(sandbox):
    home, _, _ = sandbox
    history = HistoryStore(home / ".localflow" / "history.sqlite")
    long_id = history.add(kind="dictate", final_text=("zebra " * 160 + "\n") * 5, ts=1_000)
    short_id = history.add(kind="dictate", final_text="a zebra once", ts=500)
    result, = compare(sandbox, call("search_dictations", query="zebra", limit=2))
    assert {hit["id"] for hit in result["structuredContent"]["results"]} == {long_id, short_id}


@pytest.mark.parametrize("state", ["v2-disabled", "legacy-disabled", "missing", "uninitialized", "newer"])
def test_history_state(sandbox, state):
    home, config, _ = sandbox
    path = home / ".localflow" / "history.sqlite"
    if state in {"uninitialized", "newer"}:
        path.parent.mkdir()
        conn = sqlite3.connect(path)
        try:
            conn.execute(f"PRAGMA user_version = {2 if state == 'newer' else 0}")
        finally:
            conn.close()
    elif state == "legacy-disabled":
        (home / ".localflow.toml").write_text(
            'history_enabled = false\nfireworks_api_key = "test-key"\n', encoding="utf-8",
        )
    elif state == "v2-disabled":
        config.write_text("config_version = 2\n[history]\nenabled = false\n", encoding="utf-8")
    search, get = compare(sandbox, call("search_dictations", query="budget"), call("get_dictation", id=1))
    assert get["isError"]
    if state in {"missing", "uninitialized"}:
        assert search["structuredContent"] == {"count": 0, "match": "ranked", "results": []}
    else:
        assert search["isError"]


def test_prompt_search_and_get(sandbox):
    home, _, _ = sandbox
    data = home / ".localflow"
    data.mkdir()
    (data / "brief.MD").write_bytes(b"---\r\ntitle: Private header\r\n---\r\n\r\nReview cucumber budget")
    (data / "too-big.md").write_bytes(b" " * (256 * 1024 + 1))
    (data / "bounded.md").write_text("🌿" * 20_001, encoding="utf-8")
    (data / "notes.txt").write_text("not markdown", encoding="utf-8")
    (data / "broken.md").write_bytes(b"\xff")
    (data / "queue.json").write_text(json.dumps([
        {"id": "budget", "title": "Budget review", "meeting": "Roadmap", "status": "pending",
         "created": "2024-06-20T13:04:00", "path": "~/.localflow/brief.MD"},
        {"id": "travel", "title": "Travel review", "status": "approved",
         "created": "2024-06-21T13:04:00+02:00", "path": "~/.localflow/missing.md", "repo": "local"},
        {"id": "large", "path": "~/.localflow/too-big.md"},
        {"id": "truncated", "path": "~/.localflow/bounded.md"},
        {"id": "not-md", "path": "~/.localflow/notes.txt"},
        {"id": "invalid-utf8", "path": "~/.localflow/broken.md"},
        {"id": 2},
        3,
    ]), encoding="utf-8")
    results = compare(
        sandbox,
        call("search_prompts", limit=1_000),
        call("search_prompts", status="approved"),
        call("search_prompts", query='"cucumber budget" roadmap'),
        call("search_prompts", query="cumb"),
        call("get_prompt", id="budget"),
        call("get_prompt", id="truncated"),
        *(call("get_prompt", id=id_) for id_ in ("travel", "large", "not-md", "invalid-utf8", "doesn't exist")),
    )
    assert results[0]["structuredContent"]["count"] == 6
    assert results[3]["structuredContent"]["match"] == "substring"
    assert results[4]["structuredContent"]["body"] == "Review cucumber budget"
    assert results[5]["structuredContent"]["truncated"]
    assert all(r["structuredContent"]["note"] == "prompt file missing" for r in results[6:10])


def test_missing_and_malformed_queue(sandbox):
    home, _, _ = sandbox
    req = call("search_prompts")
    expected = {"count": 0, "match": "recent", "results": []}
    assert compare(sandbox, req)[0]["structuredContent"] == expected
    queue = home / ".localflow" / "queue.json"
    queue.parent.mkdir()
    queue.write_text("[not json", encoding="utf-8")
    assert compare(sandbox, req)[0]["structuredContent"] == expected
