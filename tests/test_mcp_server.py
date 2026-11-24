import json
import os
import subprocess
import sys
from datetime import datetime, timedelta
from pathlib import Path

import pytest
from click.testing import CliRunner

from localflow import cli as cli_module
from localflow import mcp_server as mcp
from localflow.dispatch import PromptQueue, WorkPrompt, write_prompts
from localflow.history import HistoryStore
from localflow.mcp_server import Sources, ToolError

NOON = datetime.now().replace(hour=12, minute=0, second=0, microsecond=0)


def _ts(days: int, hour: int = 12) -> float:
    return (NOON - timedelta(days=days)).replace(hour=hour).timestamp()


@pytest.fixture
def history(tmp_path):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    ids = {
        "pricing": store.add(
            kind="dictate", app_name="Slack", bundle_id="com.tinyspeck.slackmacgap",
            raw_text="lets ship the pricing page redesign on friday",
            final_text="Let's ship the pricing page redesign on Friday.",
            duration_s=3.0, ts=_ts(1),
        ),
        "refactor": store.add(
            kind="transform", app_name="Cursor", bundle_id="com.todesktop.cursor",
            raw_text="refactor pricing",
            final_text="Refactor the pricing service so the page loads the new budget API.",
            duration_s=4.0, ts=_ts(1, hour=11),
        ),
        "budget": store.add(
            kind="dictate", app_name="Mail", bundle_id="com.apple.mail",
            raw_text="the quarterly budget review moved to thursday",
            final_text="The quarterly budget review moved to Thursday.",
            duration_s=2.0, ts=_ts(3),
        ),
        "haiku": store.add(
            kind="command", app_name="Claude", bundle_id="com.anthropic.claudefordesktop",
            raw_text="rewrite this as a haiku about kubernetes",
            final_text="Clusters hum softly / pods drift like autumn leaves / the scheduler sleeps",
            duration_s=2.5, ts=_ts(10),
        ),
        "failed": store.add(
            kind="dictate", app_name="Slack", raw_text="", final_text="pricing secret",
            status="failed", audio_path=tmp_path / "clips" / "x.wav", ts=_ts(1, hour=13),
        ),
        "silent": store.add(
            kind="dictate", app_name="Slack", final_text="", status="silent", ts=_ts(1, hour=14),
        ),
    }
    return ids


def _sources(tmp_path, *, enabled=True, queue=None) -> Sources:
    return Sources(
        history_path=tmp_path / "history.sqlite",
        queue_path=queue or tmp_path / "queue.json",
        history_enabled=lambda: enabled,
    )


def _ids(result: dict) -> list[int]:
    return [r["id"] for r in result["results"]]


# ------------------------------------------------------------ dictations ---

def test_ranked_search_finds_best_row_first(tmp_path, history):
    result = mcp.search_dictations(_sources(tmp_path), query="quarterly budget")
    assert result["match"] == "ranked"
    assert _ids(result)[0] == history["budget"]
    assert result["results"][0]["app"] == "Mail"
    assert "quarterly budget" in result["results"][0]["snippet"]


def test_quoted_phrase_wins(tmp_path, history):
    result = mcp.search_dictations(_sources(tmp_path), query='"pricing page" redesign')
    assert result["match"] == "ranked"
    assert _ids(result)[0] == history["pricing"]
    plain = mcp.search_dictations(_sources(tmp_path), query='"budget api"')
    assert _ids(plain)[0] == history["refactor"]


def test_no_query_lists_newest_first_and_limit_clamps(tmp_path, history):
    src = _sources(tmp_path)
    result = mcp.search_dictations(src)
    assert result["match"] == "recent"
    assert _ids(result) == [
        history["pricing"], history["refactor"], history["budget"], history["haiku"],
    ]
    assert result["count"] == 4
    assert len(mcp.search_dictations(src, limit=0)["results"]) == 1
    for i in range(60):
        HistoryStore(tmp_path / "history.sqlite").add(
            kind="dictate", final_text=f"filler {i}", ts=_ts(20),
        )
    assert len(mcp.search_dictations(src, limit=999)["results"]) == 50


def test_filters(tmp_path, history):
    src = _sources(tmp_path)
    assert _ids(mcp.search_dictations(src, kind="command")) == [history["haiku"]]
    assert _ids(mcp.search_dictations(src, app="slack")) == [history["pricing"]]
    assert _ids(mcp.search_dictations(src, app="com.apple.mail")) == [history["budget"]]
    assert history["haiku"] not in _ids(mcp.search_dictations(src, since="7d"))
    day = (NOON - timedelta(days=3)).strftime("%Y-%m-%d")
    assert _ids(mcp.search_dictations(src, until=day)) == [history["budget"], history["haiku"]]
    assert _ids(mcp.search_dictations(src, since=day, until=day)) == [history["budget"]]
    with pytest.raises(ToolError, match="since"):
        mcp.search_dictations(src, since="last tuesday")
    with pytest.raises(ToolError, match="until"):
        mcp.search_dictations(src, until="soon")


def test_command_found_by_spoken_instruction(tmp_path, history):
    result = mcp.search_dictations(_sources(tmp_path), query="haiku", kind="command")
    assert _ids(result) == [history["haiku"]]
    assert "haiku" in result["results"][0]["snippet"]


def test_many_matching_chunks_do_not_hide_other_entries(tmp_path):
    store = HistoryStore(tmp_path / "history.sqlite")
    long_text = "\n".join(" ".join(["zebra"] * 160) for _ in range(10))
    long_id = store.add(kind="dictate", final_text=long_text, ts=_ts(1))
    short_id = store.add(kind="dictate", final_text="a zebra once", ts=_ts(2))
    result = mcp.search_dictations(_sources(tmp_path), query="zebra", limit=2)
    assert result["match"] == "ranked"
    assert _ids(result) == [long_id, short_id]


def test_history_flag_from_legacy_config_writes_nothing(tmp_path, monkeypatch):
    from localflow import config, secrets

    home = tmp_path / "home"
    home.mkdir()
    legacy = home / ".localflow.toml"
    legacy.write_text('history_enabled = false\nfireworks_api_key = "fw-secret"\n')
    target = tmp_path / "xdg" / "localflow" / "config.toml"
    monkeypatch.setattr(config, "config_path", lambda: target)
    monkeypatch.setattr(config, "legacy_config_path", lambda: legacy)
    secret_writes = []
    monkeypatch.setattr(secrets, "set_secret", lambda *a: secret_writes.append(a))
    monkeypatch.delenv("FIREWORKS_API_KEY", raising=False)
    before = {p: p.stat().st_mtime_ns for p in tmp_path.rglob("*")}

    assert mcp._config_history_enabled() is False

    assert {p: p.stat().st_mtime_ns for p in tmp_path.rglob("*")} == before
    assert not target.exists()
    assert secret_writes == []
    assert "FIREWORKS_API_KEY" not in os.environ

    target.parent.mkdir(parents=True)
    target.write_text("config_version = 2\n[history]\nenabled = true\n")
    assert mcp._config_history_enabled() is True
    target.unlink()
    legacy.unlink()
    assert mcp._config_history_enabled() is True


def test_substring_fallback(tmp_path, history):
    result = mcp.search_dictations(_sources(tmp_path), query="kube")
    assert result["match"] == "substring"
    assert _ids(result) == [history["haiku"]]


def test_failed_and_silent_rows_hidden_from_search_but_gettable(tmp_path, history):
    src = _sources(tmp_path)
    found = _ids(mcp.search_dictations(src, query="pricing", limit=50))
    assert history["failed"] not in found and history["silent"] not in found
    assert history["failed"] not in _ids(mcp.search_dictations(src, limit=50))
    entry = mcp.get_dictation(src, id=history["failed"])
    assert entry["status"] == "failed"
    assert entry["final_text"] == "pricing secret"
    assert "audio_path" not in entry
    assert mcp.get_dictation(src, id=history["silent"])["status"] == "silent"


def test_get_dictation_truncates_and_never_returns_audio(tmp_path, history):
    store = HistoryStore(tmp_path / "history.sqlite")
    long_id = store.add(kind="dictate", final_text="word " * 5000, audio_path="/tmp/a.wav")
    entry = mcp.get_dictation(_sources(tmp_path), id=long_id)
    assert len(entry["final_text"]) == mcp.MAX_TEXT_CHARS
    assert entry["truncated"] is True
    assert "audio_path" not in entry and "/tmp/a.wav" not in json.dumps(entry)
    short = mcp.get_dictation(_sources(tmp_path), id=history["budget"])
    assert short["truncated"] is False
    with pytest.raises(ToolError, match="no dictation"):
        mcp.get_dictation(_sources(tmp_path), id=99999)


def test_history_disabled_never_opens_database(tmp_path, monkeypatch):
    src = _sources(tmp_path, enabled=False)
    opened = []
    monkeypatch.setattr(mcp.sqlite3, "connect", lambda *a, **k: opened.append(a))
    with pytest.raises(ToolError, match="disabled"):
        mcp.search_dictations(src, query="pricing")
    with pytest.raises(ToolError, match="disabled"):
        mcp.get_dictation(src, id=1)
    assert opened == []
    assert not (tmp_path / "history.sqlite").exists()


def test_missing_files_return_empty_and_create_nothing(tmp_path):
    home = tmp_path / "home" / ".localflow"
    src = Sources(
        history_path=home / "history.sqlite",
        queue_path=home / "queue.json",
        history_enabled=lambda: True,
    )
    before = sorted(tmp_path.rglob("*"))
    assert mcp.search_dictations(src, query="pricing")["results"] == []
    assert mcp.search_dictations(src)["results"] == []
    assert mcp.search_prompts(src, query="pricing")["results"] == []
    assert mcp.search_prompts(src)["results"] == []
    with pytest.raises(ToolError):
        mcp.get_dictation(src, id=1)
    with pytest.raises(ToolError):
        mcp.get_prompt(src, id="abc")
    assert sorted(tmp_path.rglob("*")) == before


def test_reading_history_does_not_modify_it(tmp_path, history):
    db = tmp_path / "history.sqlite"
    before = (db.stat().st_mtime_ns, db.read_bytes())
    listing = sorted(p.name for p in tmp_path.iterdir())
    mcp.search_dictations(_sources(tmp_path), query="budget")
    mcp.get_dictation(_sources(tmp_path), id=history["budget"])
    assert (db.stat().st_mtime_ns, db.read_bytes()) == before
    assert sorted(p.name for p in tmp_path.iterdir()) == listing


# --------------------------------------------------------------- prompts ---

@pytest.fixture
def prompts(tmp_path):
    queue_path = tmp_path / "queue.json"
    paths = write_prompts(
        [
            WorkPrompt("Add CSV export", "Add a CSV export button to the invoices table."),
            WorkPrompt("Fix login redirect", "After OAuth the user lands on a blank page."),
        ],
        "Acme sync", NOON - timedelta(days=1), prompts_dir=tmp_path / "prompts",
    )
    queue = PromptQueue(queue_path)
    entries = queue.add(
        [WorkPrompt("Add CSV export", ""), WorkPrompt("Fix login redirect", "")],
        paths, "Acme sync",
    )
    queue.set_status(entries[1]["id"], "approved", handle="orca-term-7")
    (queue_path.parent / "queue.json.lock").unlink(missing_ok=True)
    return {"csv": entries[0]["id"], "login": entries[1]["id"], "paths": paths}


def test_search_prompts_by_body_and_status(tmp_path, prompts):
    src = _sources(tmp_path)
    result = mcp.search_prompts(src, query="invoices")
    assert result["match"] == "ranked"
    assert [r["id"] for r in result["results"]] == [prompts["csv"]]
    assert "invoices" in result["results"][0]["snippet"]
    approved = mcp.search_prompts(src, status="approved")
    assert [r["id"] for r in approved["results"]] == [prompts["login"]]
    assert approved["match"] == "recent"
    assert mcp.search_prompts(src, query="acme")["count"] == 2
    assert mcp.search_prompts(src, query="invo")["match"] == "substring"
    with pytest.raises(ToolError):
        mcp.search_prompts(src, status="bogus")
    assert not (tmp_path / "queue.json.lock").exists()


def test_get_prompt(tmp_path, prompts):
    src = _sources(tmp_path)
    entry = mcp.get_prompt(src, id=prompts["login"])
    assert entry["status"] == "approved"
    assert entry["body"].strip() == "After OAuth the user lands on a blank page."
    assert "---" not in entry["body"]
    assert "handle" not in entry and "orca-term-7" not in json.dumps(entry)
    assert entry["truncated"] is False and "note" not in entry
    Path(prompts["paths"][0]).unlink()
    missing = mcp.get_prompt(src, id=prompts["csv"])
    assert missing["body"] == "" and missing["note"] == "prompt file missing"
    assert mcp.search_prompts(src, query="csv")["results"][0]["id"] == prompts["csv"]
    with pytest.raises(ToolError, match="no work prompt"):
        mcp.get_prompt(src, id="nope")
    assert not (tmp_path / "queue.json.lock").exists()


def test_corrupt_queue_is_empty(tmp_path):
    (tmp_path / "queue.json").write_text("{not json", encoding="utf-8")
    assert mcp.search_prompts(_sources(tmp_path))["results"] == []


def test_relative_prompt_path_reads_from_supplied_cwd(tmp_path, monkeypatch):
    """A child started in its own directory reads relative prompt paths
    against the launching app's cwd, never its own."""
    runtime, child = tmp_path / "runtime", tmp_path / "child"
    for base, body in ((runtime, "Runtime walrus body"), (child, "Child decoy")):
        (base / "prompts").mkdir(parents=True)
        (base / "prompts" / "brief.md").write_text(body, encoding="utf-8")
    absolute = tmp_path / "abs.md"
    absolute.write_text("Absolute body", encoding="utf-8")
    (tmp_path / "queue.json").write_text(json.dumps([
        {"id": "rel", "title": "Rel", "path": "prompts/brief.md"},
        {"id": "abs", "title": "Abs", "path": str(absolute)},
    ]), encoding="utf-8")
    monkeypatch.chdir(child)

    def sources(cwd):
        return Sources(
            history_path=tmp_path / "history.sqlite", queue_path=tmp_path / "queue.json",
            history_enabled=lambda: True, prompts_cwd=cwd,
        )

    src = sources(str(runtime))
    assert mcp.get_prompt(src, id="rel")["body"] == "Runtime walrus body"
    assert mcp.search_prompts(src, query="walrus")["results"][0]["id"] == "rel"
    assert mcp.get_prompt(src, id="abs")["body"] == "Absolute body"
    # Unset: standalone behaviour (this process's cwd) is unchanged.
    assert mcp.get_prompt(sources(None), id="rel")["body"] == "Child decoy"
    # Set but not absolute (the desktop couldn't read its cwd): not guessed.
    for unresolved in ("", "runtime"):
        got = mcp.get_prompt(sources(unresolved), id="rel")
        assert got["note"] == "prompt file missing", unresolved
        assert mcp.get_prompt(sources(unresolved), id="abs")["body"] == "Absolute body"
    monkeypatch.setenv(mcp.PROMPTS_CWD_ENV, str(runtime))
    assert mcp.default_sources().prompts_cwd == str(runtime)


# -------------------------------------------------------------- protocol ---

def _call(src, method, params=None, request_id=1):
    message = {"jsonrpc": "2.0", "id": request_id, "method": method}
    if params is not None:
        message["params"] = params
    return mcp.handle_message(src, message)


def test_initialize_negotiates_version(tmp_path):
    src = _sources(tmp_path)
    reply = _call(src, "initialize", {"protocolVersion": "2025-03-26"})
    result = reply["result"]
    assert result["protocolVersion"] == "2025-03-26"
    assert result["capabilities"] == {"tools": {}}
    assert result["serverInfo"]["name"] == "localflow"
    assert result["instructions"]
    fallback = _call(src, "initialize", {"protocolVersion": "1999-01-01"})
    assert fallback["result"]["protocolVersion"] == "2025-06-18"
    assert _call(src, "ping") == {"jsonrpc": "2.0", "id": 1, "result": {}}


def test_tools_list(tmp_path):
    tools = _call(_sources(tmp_path), "tools/list")["result"]["tools"]
    assert sorted(t["name"] for t in tools) == [
        "get_dictation", "get_prompt", "search_dictations", "search_prompts",
    ]
    for tool in tools:
        assert tool["inputSchema"]["type"] == "object"
        assert tool["inputSchema"]["additionalProperties"] is False
        assert tool["annotations"]["readOnlyHint"] is True
        assert "handler" not in tool
    json.dumps(tools)


def test_tools_call_success_and_errors(tmp_path, history):
    src = _sources(tmp_path)
    reply = _call(src, "tools/call", {
        "name": "search_dictations", "arguments": {"query": "budget", "limit": 2},
    })
    result = reply["result"]
    assert "isError" not in result
    assert json.loads(result["content"][0]["text"]) == result["structuredContent"]
    assert result["structuredContent"]["count"] == 2

    failed = _call(src, "tools/call", {"name": "get_dictation", "arguments": {"id": 4242}})
    assert failed["result"]["isError"] is True
    assert "no dictation" in failed["result"]["content"][0]["text"]

    disabled = _call(_sources(tmp_path, enabled=False), "tools/call", {
        "name": "search_dictations", "arguments": {},
    })
    assert disabled["result"]["isError"] is True

    for params in (
        {"name": "rm_rf", "arguments": {}},
        {"name": "get_dictation", "arguments": {}},
        {"name": "get_dictation", "arguments": {"id": "7"}},
        {"name": "search_dictations", "arguments": {"kind": "shell"}},
        {"name": "search_dictations", "arguments": {"audio_path": "x"}},
    ):
        assert _call(src, "tools/call", params)["error"]["code"] == -32602

    assert _call(src, "resources/list")["error"]["code"] == -32601
    assert mcp.handle_message(
        src, {"jsonrpc": "2.0", "method": "notifications/initialized"},
    ) is None
    assert mcp.handle_message(src, [])["error"]["code"] == -32600


def test_serve_loop_handles_garbage_and_eof(tmp_path):
    import io

    stdin = io.BytesIO(
        b"not json\n\n"
        + json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}).encode()
        + b"\n"
        + json.dumps({"jsonrpc": "2.0", "id": 5, "method": "ping"}).encode()
        + b"\n"
    )
    stdout = io.BytesIO()
    mcp.serve(_sources(tmp_path), stdin, stdout)
    lines = stdout.getvalue().decode().splitlines()
    assert [json.loads(line).get("id") for line in lines] == [None, 5]
    assert json.loads(lines[0])["error"]["code"] == -32700


def test_stdio_round_trip(tmp_path):
    lines = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"},
        }},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
    ]
    env = {**os.environ, "HOME": str(tmp_path), "USERPROFILE": str(tmp_path)}
    proc = subprocess.run(
        [sys.executable, "-m", "localflow.mcp_server"],
        input="".join(json.dumps(m) + "\n" for m in lines).encode(),
        capture_output=True, env=env, timeout=60, check=False,
    )
    assert proc.returncode == 0, proc.stderr.decode()
    out = proc.stdout.decode().splitlines()
    assert len(out) == 2
    replies = [json.loads(line) for line in out]
    assert [r["id"] for r in replies] == [1, 2]
    assert all(r["jsonrpc"] == "2.0" and "result" in r for r in replies)
    assert len(replies[1]["result"]["tools"]) == 4
    assert not (tmp_path / ".localflow").exists()


def test_lf_mcp_help():
    result = CliRunner().invoke(cli_module.cli, ["mcp", "--help"])
    assert result.exit_code == 0
    assert "MCP server" in result.output
