import json
import os
import subprocess
import sys
import threading

import pytest

from localflow.agents import store as store_module
from localflow.agents.models import AgentSession, SessionConflict
from localflow.agents.store import AgentSessionStore, HotCommandStore


def _session(i: int = 0, **kw) -> AgentSession:
    return AgentSession(id=f"{i:012x}", provider="mock", prompt="p", title="t", **kw)


def test_create_transition_compare_and_set(tmp_path):
    store = AgentSessionStore(tmp_path)
    created = store.create(_session(1))
    assert created.state == "queued" and created.revision == 1 and created.last_seq == 1
    store.transition(created.id, "starting", expect=("queued",))
    with pytest.raises(SessionConflict) as lost:
        store.transition(created.id, "starting", expect=("queued",))
    assert lost.value.current == "starting" and lost.value.expected == ("queued",)
    with pytest.raises(SessionConflict):  # illegal move: starting -> completed
        store.transition(created.id, "completed")
    done = store.transition(created.id, "running")
    done = store.transition(done.id, "completed", result_text="ok")
    assert done.finished_at and done.started_at
    with pytest.raises(SessionConflict):  # terminal states are never left
        store.transition(done.id, "running")
    states = [e.state for e in store.events(done.id) if e.type == "state"]
    assert states == ["queued", "starting", "running", "completed"]
    seqs = [e.seq for e in store.events(done.id)]
    assert seqs == list(range(1, len(seqs) + 1))


def test_reload_keeps_sessions_and_last_seq(tmp_path):
    store = AgentSessionStore(tmp_path)
    session = store.create(_session(2))
    store.append_event(session.id, "output", stream="agent", text="hello")
    again = AgentSessionStore(tmp_path)
    assert again.get(session.id).last_seq == 2
    assert again.revision == store.revision


def test_output_is_bounded_with_one_truncated_marker(tmp_path):
    store = AgentSessionStore(tmp_path)
    session = store.create(_session(3))
    limit = 16 * 1024
    for _ in range(40):
        store.append_event(session.id, "output", text="x" * 2000, limit_bytes=limit)
    store.append_event(session.id, "error", text="boom", limit_bytes=limit)
    store.transition(session.id, "failed")
    events = store.events(session.id, limit=1000)
    assert [e.type for e in events].count("truncated") == 1
    assert events[-1].type == "state" and events[-1].state == "failed"
    assert events[-2].type == "error"
    size = (tmp_path / "events" / f"{session.id}.jsonl").stat().st_size
    assert size <= limit + store_module.EVENT_RESERVE_BYTES
    assert [e.seq for e in events] == list(range(1, len(events) + 1))


def test_long_text_is_split_into_8k_events(tmp_path):
    store = AgentSessionStore(tmp_path)
    session = store.create(_session(4))
    store.append_event(session.id, "output", text="y" * 20000)
    outputs = [e for e in store.events(session.id) if e.type == "output"]
    assert [len(e.text) for e in outputs] == [8192, 8192, 20000 - 16384]


def test_prune_drops_oldest_terminal_never_active(tmp_path):
    store = AgentSessionStore(tmp_path)
    for i in range(6):
        store.create(_session(10 + i))
    active = f"{10:012x}"
    for i in range(1, 6):
        sid = f"{10 + i:012x}"
        store.transition(sid, "cancelled")
    removed = store.prune(3)
    assert removed == [f"{11:012x}", f"{12:012x}", f"{13:012x}"]
    assert store.get(active) is not None
    assert not (tmp_path / "events" / f"{11:012x}.jsonl").exists()
    assert store.prune(1) == [f"{14:012x}", f"{15:012x}"]  # the active one stays
    assert [s.id for s in store.list()] == [active]


def test_changes_report_updates_deletions_and_reset(tmp_path):
    store = AgentSessionStore(tmp_path)
    a = store.create(_session(20))
    b = store.create(_session(21))
    since = store.revision
    store.transition(a.id, "cancelled")
    store.delete(b.id, require_terminal=False)
    changes = store.changes(since)
    assert [s.id for s in changes["sessions"]] == [a.id]
    assert changes["deleted"] == [b.id] and changes["reset"] is False
    assert store.changes(store.revision + 5)["reset"] is True
    restarted = AgentSessionStore(tmp_path)
    assert restarted.changes(since)["reset"] is True  # tombstones did not survive
    assert restarted.changes(restarted.revision)["reset"] is False


def test_delete_refuses_active_sessions(tmp_path):
    store = AgentSessionStore(tmp_path)
    session = store.create(_session(30))
    with pytest.raises(SessionConflict):
        store.delete(session.id)


@pytest.mark.skipif(os.name != "posix", reason="POSIX permissions")
def test_files_are_private(tmp_path):
    root = tmp_path / "agents"
    store = AgentSessionStore(root)
    session = store.create(_session(31))
    assert (root.stat().st_mode & 0o777) == 0o700
    assert ((root / "sessions.json").stat().st_mode & 0o777) == 0o600
    assert ((root / "events" / f"{session.id}.jsonl").stat().st_mode & 0o777) == 0o600


def test_unreadable_files_read_as_empty(tmp_path):
    (tmp_path / "sessions.json").write_text("{not json", encoding="utf-8")
    assert AgentSessionStore(tmp_path).list() == []
    rules = HotCommandStore(tmp_path / "hot_commands.json")
    rules.path.write_text("[[[", encoding="utf-8")
    assert rules.list() == []


def test_without_fcntl_the_store_still_works(tmp_path, monkeypatch):
    monkeypatch.setattr(store_module, "fcntl", None)
    store = AgentSessionStore(tmp_path)
    store.create(_session(40))
    rules = HotCommandStore(tmp_path / "hot_commands.json")
    with rules.mutate() as entries:
        entries.append({"id": "aaaaaaaa", "trigger": "launch claude"})
    assert rules.get("aaaaaaaa")["trigger"] == "launch claude"


def test_rule_cache_follows_file_signature(tmp_path):
    rules = HotCommandStore(tmp_path / "hot_commands.json")
    assert rules.list() == []
    with rules.mutate() as entries:
        entries.append({"id": "1", "trigger": "a b"})
    assert len(rules.list()) == 1
    other = HotCommandStore(tmp_path / "hot_commands.json")
    with other.mutate() as entries:
        entries.append({"id": "2", "trigger": "c d"})
    assert [r["id"] for r in rules.list()] == ["1", "2"]


def test_threads_never_lose_rule_writes(tmp_path):
    path = tmp_path / "hot_commands.json"

    def writer(prefix):
        store = HotCommandStore(path)
        for i in range(50):
            with store.mutate() as entries:
                entries.append({"id": f"{prefix}{i}"})

    threads = [threading.Thread(target=writer, args=(p,)) for p in "ab"]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert len(HotCommandStore(path).list()) == 100


@pytest.mark.skipif(store_module.fcntl is None, reason="cross-process locking needs fcntl")
def test_two_processes_never_lose_rule_writes(tmp_path):
    path = tmp_path / "hot_commands.json"
    code = (
        "import sys\n"
        "from localflow.agents.store import HotCommandStore\n"
        "store = HotCommandStore(sys.argv[1])\n"
        "for i in range(100):\n"
        "    with store.mutate() as entries:\n"
        "        entries.append({'id': sys.argv[2] + str(i)})\n"
    )
    procs = [subprocess.Popen([sys.executable, "-c", code, str(path), p]) for p in "xy"]
    assert [p.wait(timeout=120) for p in procs] == [0, 0]
    entries = json.loads(path.read_text(encoding="utf-8"))
    assert len(entries) == 200 and len({e["id"] for e in entries}) == 200
