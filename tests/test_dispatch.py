import json
import os
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

import pytest

from localflow import dispatch as D
from localflow.dispatch import PromptQueue, WorkPrompt

SAMPLE = """### Migrate session storage to Redis
> Replace the local-file session storage in the app with Redis, keeping the
> current session API unchanged. Done means existing session tests pass.

### Update deploy docs for Redis sessions
> After the migration lands, update the deployment documentation.
"""

POSIX_QUEUE_LOCK_TEST = pytest.mark.skipif(
    sys.platform == "win32" or os.environ.get("LOCALFLOW_PLATFORM") == "win32",
    reason="POSIX cross-process locking tests are not supported on Windows",
)

POSIX_QUEUE_LOCK_TEST = pytest.mark.skipif(
    sys.platform == "win32" or os.environ.get("LOCALFLOW_PLATFORM") == "win32",
    reason="POSIX cross-process locking tests are not supported on Windows",
)


# ------------------------------------------------------------------ parsing ---

def test_parse_happy_path():
    prompts = D.parse_work_prompts(SAMPLE)
    assert [p.title for p in prompts] == [
        "Migrate session storage to Redis",
        "Update deploy docs for Redis sessions",
    ]
    assert prompts[0].body == (
        "Replace the local-file session storage in the app with Redis, keeping the "
        "current session API unchanged. Done means existing session tests pass."
    )
    assert "\n" not in prompts[0].body


def test_parse_fallback_returns_empty():
    assert D.parse_work_prompts("(No new work identified.)") == []
    assert D.parse_work_prompts("\n(No new work identified.)\n  ") == []


def test_parse_empty_input():
    assert D.parse_work_prompts("") == []
    assert D.parse_work_prompts("   \n\n\t") == []


def test_parse_tolerates_noise():
    md = """

###    Spaced heading
> body one

### Heading with no body

### Second real block
>
>   body two
"""
    prompts = D.parse_work_prompts(md)
    assert [(p.title, p.body) for p in prompts] == [
        ("Spaced heading", "body one"),
        ("Second real block", "body two"),
    ]


def test_parse_ignores_quote_without_heading():
    assert D.parse_work_prompts("> orphan quote line") == []


# ----------------------------------------------------------------- slugify ---

@pytest.mark.parametrize(
    "text,expected",
    [
        ("Migrate session storage to Redis", "migrate-session-storage-to-redis"),
        ("Hello,   World!! (again)", "hello-world-again"),
        ("--leading and trailing--", "leading-and-trailing"),
        ("", "untitled"),
        ("!!!", "untitled"),
        ("café ☕ naïve", "caf-na-ve"),
    ],
)
def test_slugify_cases(text, expected):
    assert D.slugify(text) == expected


def test_slugify_truncates_at_word_boundary():
    slug = D.slugify("word " * 30)
    assert len(slug) <= 50
    assert not slug.endswith("-")
    assert slug.split("-")[0] == "word"


def test_slugify_long_single_token_still_bounded():
    slug = D.slugify("x" * 200)
    assert slug == "x" * 50


# ------------------------------------------------------------ write_prompts ---

def test_write_prompts_layout_and_frontmatter(tmp_path):
    started = datetime(2026, 7, 30, 14, 5, 0).timestamp()
    prompts = D.parse_work_prompts(SAMPLE)
    paths = D.write_prompts(prompts, "Weekly Sync: Platform", started, prompts_dir=tmp_path)

    folder = tmp_path / "2026-07-30-weekly-sync-platform"
    assert [p.parent for p in paths] == [folder, folder]
    assert [p.name for p in paths] == [
        "01-migrate-session-storage-to-redis.md",
        "02-update-deploy-docs-for-redis-sessions.md",
    ]

    text = paths[0].read_text()
    assert text.startswith("---\n")
    assert "title: Migrate session storage to Redis\n" in text
    assert "meeting: Weekly Sync: Platform\n" in text
    assert f"created: {datetime.fromtimestamp(started).isoformat()}\n" in text
    assert "status: pending\n" in text
    assert text.endswith(prompts[0].body + "\n")


def test_write_prompts_accepts_datetime(tmp_path):
    when = datetime(2026, 7, 30, 14, 5, 0)
    prompts = D.parse_work_prompts(SAMPLE)
    from_dt = D.write_prompts(prompts, "Sync", when, prompts_dir=tmp_path / "a")
    from_ts = D.write_prompts(prompts, "Sync", when.timestamp(), prompts_dir=tmp_path / "b")
    assert [p.name for p in from_dt] == [p.name for p in from_ts]
    assert from_dt[0].parent.name == "2026-07-30-sync"
    assert from_dt[0].read_text() == from_ts[0].read_text()


def test_write_prompts_empty_list_writes_nothing(tmp_path):
    assert D.write_prompts([], "Sync", time.time(), prompts_dir=tmp_path) == []
    assert list(tmp_path.iterdir()) == []


# ------------------------------------------------------------------- queue ---

@pytest.fixture
def queue(tmp_path):
    return PromptQueue(tmp_path / "state" / "queue.json")


@pytest.fixture
def seeded(queue, tmp_path):
    prompts = D.parse_work_prompts(SAMPLE)
    paths = D.write_prompts(prompts, "Weekly Sync", time.time(), prompts_dir=tmp_path)
    entries = queue.add(prompts, paths, "Weekly Sync")
    return queue, entries, paths


def test_queue_missing_file_is_empty(queue):
    assert queue.list() == []
    assert queue.get("nope") is None


def test_queue_add_returns_entries(seeded):
    queue, entries, paths = seeded
    assert len(entries) == 2
    assert {e["status"] for e in entries} == {"pending"}
    assert entries[0]["path"] == str(paths[0])
    assert entries[0]["meeting"] == "Weekly Sync"
    # handle is in the template, so a never-dispatched entry is not a KeyError trap
    assert entries[0]["worktree"] == "" and entries[0]["repo"] == "" and entries[0]["error"] == ""
    assert entries[0]["handle"] == ""
    assert PromptQueue(queue.path).list()[0]["handle"] == ""
    assert entries[0]["id"] != entries[1]["id"]
    assert queue.path.exists()


def test_queue_get_and_status_filter(seeded):
    queue, entries, _ = seeded
    assert queue.get(entries[1]["id"])["title"] == entries[1]["title"]
    assert len(queue.list("pending")) == 2
    assert queue.list("dispatched") == []


def test_queue_set_status_round_trip(seeded):
    queue, entries, _ = seeded
    updated = queue.set_status(entries[0]["id"], "dispatched", worktree="wt-1", repo="path:/x")
    assert updated["status"] == "dispatched"
    assert updated["worktree"] == "wt-1" and updated["repo"] == "path:/x"
    assert len(queue.list("pending")) == 1
    assert len(queue.list("dispatched")) == 1


def test_queue_survives_reload(seeded):
    queue, entries, _ = seeded
    queue.set_status(entries[0]["id"], "approved")
    reloaded = PromptQueue(queue.path)
    assert [e["status"] for e in reloaded.list()] == ["approved", "pending"]
    assert reloaded.get(entries[0]["id"])["title"] == entries[0]["title"]


def test_queue_set_status_empty_kwargs_clear_fields(seeded):
    """Re-approving a failed entry must actually wipe the stale failure data."""
    queue, entries, _ = seeded
    eid = entries[0]["id"]
    queue.set_status(eid, "failed", error="orca exited 2: repo not found",
                     worktree="repo::/wt/x", handle="term-42")
    assert queue.get(eid)["error"]

    cleared = queue.set_status(eid, "approved", error="", worktree="", handle="")
    assert cleared["status"] == "approved"
    assert cleared["error"] == "" and cleared["worktree"] == "" and cleared["handle"] == ""
    # and it survives the round trip to disk, not just in the returned dict
    reloaded = PromptQueue(queue.path).get(eid)
    assert reloaded["error"] == "" and reloaded["worktree"] == "" and reloaded["handle"] == ""


_WORKER = """
import os, sys
sys.path.insert(0, {repo!r})
from localflow import dispatch as D
if os.environ.get("LF_NO_FLOCK"):
    D.fcntl = None            # simulate the pre-flock implementation
q = D.PromptQueue({queue!r})
for eid in sys.argv[1:]:
    for _ in range(15):
        q.set_status(eid, "dispatched", worktree="wt-" + eid)
"""


@POSIX_QUEUE_LOCK_TEST
def test_queue_survives_concurrent_processes(tmp_path):
    """The server and the CLI are separate processes over one queue file.

    Without a cross-process lock, one process's whole-file rewrite silently
    clobbers the other's update. Threads cannot reproduce this, so this test
    spawns real interpreters.
    """
    import subprocess as sp
    import sys

    repo = str(Path(__file__).resolve().parents[1])
    queue_path = tmp_path / "queue.json"
    q = PromptQueue(queue_path)

    prompts = [WorkPrompt(title=f"Task {i}", body=f"body {i}") for i in range(40)]
    paths = [tmp_path / f"{i:02d}.md" for i in range(40)]
    entries = q.add(prompts, paths, "Weekly Sync")
    ids = [e["id"] for e in entries]

    script = _WORKER.format(repo=repo, queue=str(queue_path))
    chunks = [ids[0:10], ids[10:20], ids[20:30], ids[30:40]]
    procs = [sp.Popen([sys.executable, "-c", script, *chunk]) for chunk in chunks]
    for proc in procs:
        assert proc.wait(timeout=60) == 0

    final = PromptQueue(queue_path).list()
    assert len(final) == 40, "entries were lost entirely"
    stragglers = [e["id"] for e in final if e["status"] != "dispatched"]
    assert not stragglers, f"lost updates for {len(stragglers)} entries: {stragglers[:5]}"
    assert all(e["worktree"] == "wt-" + e["id"] for e in final)


@POSIX_QUEUE_LOCK_TEST
def test_queue_write_survives_unavailable_lock(seeded, monkeypatch):
    """An unusable lock must degrade to unlocked, never swallow the write."""
    queue, entries, _ = seeded

    def no_lock(*a, **k):
        raise OSError("flock unsupported on this filesystem")

    monkeypatch.setattr(D.fcntl, "flock", no_lock)
    updated = queue.set_status(entries[0]["id"], "approved")
    assert updated["status"] == "approved"
    monkeypatch.undo()
    assert PromptQueue(queue.path).get(entries[0]["id"])["status"] == "approved"


def test_queue_set_status_unknown_id(seeded):
    queue, _, _ = seeded
    assert queue.set_status("deadbeef", "approved") is None


# -- compare-and-set --

def test_cas_allows_expected_transition(seeded):
    queue, entries, _ = seeded
    out = queue.set_status(entries[0]["id"], "approved", expect=("pending", "failed"))
    assert out["status"] == "approved"


def test_cas_conflict_raises_and_does_not_write(seeded):
    """A dispatch that slips in between must not be silently overwritten."""
    queue, entries, _ = seeded
    eid = entries[0]["id"]
    queue.set_status(eid, "dispatched", worktree="wt-live")

    with pytest.raises(D.StatusConflict) as excinfo:
        queue.set_status(eid, "approved", expect=("pending", "failed"))

    # the exception carries what the caller needs to build a 409 message
    assert excinfo.value.current == "dispatched"
    assert excinfo.value.expected == ("pending", "failed")
    assert excinfo.value.entry_id == eid
    # and the live dispatch is untouched, on disk
    survived = PromptQueue(queue.path).get(eid)
    assert survived["status"] == "dispatched" and survived["worktree"] == "wt-live"


def test_cas_unknown_id_returns_none_not_conflict(seeded):
    """404 and 409 must stay distinguishable for the HTTP layer."""
    queue, _, _ = seeded
    assert queue.set_status("deadbeef", "approved", expect=("pending",)) is None


def test_cas_absent_expect_keeps_old_behaviour(seeded):
    queue, entries, _ = seeded
    eid = entries[0]["id"]
    queue.set_status(eid, "dispatched")
    assert queue.set_status(eid, "approved")["status"] == "approved"


_CAS_WORKER = """
import sys
sys.path.insert(0, {repo!r})
from localflow import dispatch as D
q = D.PromptQueue({queue!r})
won = 0
for eid in sys.argv[2:]:
    try:
        if q.set_status(eid, "approved", expect=("pending",)):
            won += 1
    except D.StatusConflict:
        pass
open(sys.argv[1], "w").write(str(won))
"""


@POSIX_QUEUE_LOCK_TEST
def test_cas_exactly_one_winner_across_processes(tmp_path):
    """Four processes race to approve the same 30 entries.

    Check-then-write in two separate lock acquisitions would let more than one
    process win the same entry; a real compare-and-set cannot exceed 30 total.
    """
    import subprocess as sp
    import sys

    repo = str(Path(__file__).resolve().parents[1])
    queue_path = tmp_path / "queue.json"
    q = PromptQueue(queue_path)

    prompts = [WorkPrompt(title=f"Task {i}", body=f"body {i}") for i in range(30)]
    paths = [tmp_path / f"{i:02d}.md" for i in range(30)]
    ids = [e["id"] for e in q.add(prompts, paths, "Weekly Sync")]

    script = _CAS_WORKER.format(repo=repo, queue=str(queue_path))
    counters = [tmp_path / f"won-{i}" for i in range(4)]
    procs = [sp.Popen([sys.executable, "-c", script, str(c), *ids]) for c in counters]
    for proc in procs:
        assert proc.wait(timeout=60) == 0

    wins = sum(int(c.read_text()) for c in counters)
    assert wins == 30, f"{wins} wins for 30 entries: compare-and-set is not atomic"
    assert all(e["status"] == "approved" for e in PromptQueue(queue_path).list())


def test_queue_set_status_rejects_unknown_status(seeded):
    """A typo'd status used to write silently, then match no filter forever."""
    queue, entries, _ = seeded
    eid = entries[0]["id"]
    with pytest.raises(ValueError):
        queue.set_status(eid, "dispatchd")
    assert queue.get(eid)["status"] == "pending"  # unchanged on disk


@pytest.mark.parametrize("status", list(D.STATUSES))
def test_queue_set_status_accepts_every_declared_status(seeded, status):
    queue, entries, _ = seeded
    assert queue.set_status(entries[0]["id"], status)["status"] == status


def test_queue_set_status_validates_before_lookup(seeded):
    """Validation is a programming-error guard, so it fires even for a bad id."""
    queue, _, _ = seeded
    with pytest.raises(ValueError):
        queue.set_status("deadbeef", "bogus")


def test_queue_bare_approve_clears_previous_attempt(seeded):
    """The server approves with no kwargs; the stale failure must not survive."""
    queue, entries, _ = seeded
    eid = entries[0]["id"]
    queue.set_status(eid, "failed", error="orca exited 2: repo not found",
                     worktree="repo::/wt/x", handle="term-42")

    approved = queue.set_status(eid, "approved")  # no kwargs at all
    assert approved["status"] == "approved"
    assert approved["error"] == "" and approved["worktree"] == "" and approved["handle"] == ""
    reloaded = PromptQueue(queue.path).get(eid)
    assert reloaded["error"] == "" and reloaded["worktree"] == "" and reloaded["handle"] == ""


def test_queue_only_approved_clears_attempt_fields(seeded):
    """failed must keep its error; dispatched must keep worktree and handle."""
    queue, entries, _ = seeded
    a, b = entries[0]["id"], entries[1]["id"]

    queue.set_status(a, "failed", error="boom", worktree="wt-a", handle="h-a")
    assert queue.set_status(a, "failed")["error"] == "boom"
    assert queue.set_status(a, "rejected")["error"] == "boom"

    queue.set_status(b, "dispatched", worktree="wt-b", handle="h-b")
    still = queue.set_status(b, "dispatched")
    assert still["worktree"] == "wt-b" and still["handle"] == "h-b"


def test_queue_approve_clear_is_overridable_by_kwargs(seeded):
    """The clear happens before **fields, so an explicit value still wins."""
    queue, entries, _ = seeded
    eid = entries[0]["id"]
    queue.set_status(eid, "failed", error="boom")
    out = queue.set_status(eid, "approved", error="", repo="path:/x", handle="kept")
    assert out["error"] == "" and out["repo"] == "path:/x"
    assert out["handle"] == "kept"


def test_repeat_add_is_idempotent_not_suffixed(seeded, tmp_path):
    """Re-adding the same (path, title) must reuse the entry, not mint a new one.

    This previously asserted the opposite — that a repeat add produced a second
    suffixed id. That behaviour was the bug: the two entries pointed at one
    file, so approving both dispatched the same prompt twice.
    """
    queue, entries, paths = seeded
    prompts = D.parse_work_prompts(SAMPLE)
    again = queue.add(prompts, paths, "Weekly Sync")
    ids = [e["id"] for e in queue.list()]
    assert len(ids) == len(set(ids)) == 2
    assert again[0]["id"] == entries[0]["id"]


# ----------------------------------------------------------- orca_available ---

class FakeProc:
    def __init__(self, returncode=0, stdout="", stderr=""):
        self.returncode, self.stdout, self.stderr = returncode, stdout, stderr


def _status_json(running=True, reachable=True):
    return json.dumps({
        "id": "local-status", "ok": True,
        "result": {
            "app": {"running": running},
            "runtime": {"state": "ready" if reachable else "stale_bootstrap",
                        "reachable": reachable},
        },
    })


def test_orca_available_ok(monkeypatch):
    noisy = "SecCodeCheckValidity: Error\n" + _status_json()
    monkeypatch.setattr(subprocess, "run",
                        lambda *a, **k: FakeProc(stdout=noisy, stderr="SecCodeCheckValidity: Error"))
    assert D.orca_available() == (True, "")


@pytest.mark.parametrize("running,reachable", [(False, True), (True, False), (False, False)])
def test_orca_available_down(monkeypatch, running, reachable):
    monkeypatch.setattr(subprocess, "run",
                        lambda *a, **k: FakeProc(stdout=_status_json(running, reachable)))
    ok, reason = D.orca_available()
    assert ok is False and "orca open" in reason


def test_orca_available_headless_reason(monkeypatch):
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: FakeProc(stdout=_status_json(False, False)))
    monkeypatch.setenv("SSH_CONNECTION", "10.0.0.1 22 10.0.0.2 22")
    ok, reason = D.orca_available()
    assert ok is False and "SSH" in reason


def test_orca_available_local_reason_has_no_ssh_note(monkeypatch):
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: FakeProc(stdout=_status_json(False, False)))
    monkeypatch.delenv("SSH_CONNECTION", raising=False)
    monkeypatch.delenv("SSH_TTY", raising=False)
    ok, reason = D.orca_available()
    assert ok is False and "SSH" not in reason


def test_orca_available_binary_missing(monkeypatch):
    def boom(*a, **k):
        raise FileNotFoundError("orca")
    monkeypatch.setattr(subprocess, "run", boom)
    ok, reason = D.orca_available()
    assert ok is False and "PATH" in reason


def test_orca_available_bad_json(monkeypatch):
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: FakeProc(stdout="not json at all"))
    ok, reason = D.orca_available()
    assert ok is False and reason


def test_orca_available_timeout(monkeypatch):
    def boom(*a, **k):
        raise subprocess.TimeoutExpired(cmd="orca", timeout=15)
    monkeypatch.setattr(subprocess, "run", boom)
    assert D.orca_available() == (False, "orca status timed out")


# ---------------------------------------------------------------- dispatch ---

@pytest.fixture
def entry(tmp_path):
    prompts = D.parse_work_prompts(SAMPLE)
    paths = D.write_prompts(prompts, "Weekly Sync", time.time(), prompts_dir=tmp_path)
    q = PromptQueue(tmp_path / "queue.json")
    return q.add(prompts, paths, "Weekly Sync")[0]


SUCCESS_JSON = json.dumps({
    "ok": True,
    "result": {
        "worktree": {"worktreeId": "repo-abc::/Users/taj/wt/migrate"},
        "agentTerminalHandle": "term-42",
    },
})

# Older runtimes: no agentTerminalHandle, only startupTerminal.handle.
LEGACY_JSON = json.dumps({
    "ok": True,
    "result": {
        "worktree": {"worktreeId": "repo-abc::/Users/taj/wt/migrate"},
        "startupTerminal": {"handle": "term-legacy"},
    },
})


def test_dispatch_argv_and_success(monkeypatch, entry):
    seen = {}

    def fake_run(argv, **kwargs):
        seen["argv"], seen["kwargs"] = argv, kwargs
        return FakeProc(stdout="SecCodeCheckValidity noise\n" + SUCCESS_JSON)

    monkeypatch.setattr(subprocess, "run", fake_run)
    out = D.dispatch(entry, "path:/Users/taj/projs/localflow", agent="claude")

    assert out["ok"] is True
    assert out["worktree"] == "repo-abc::/Users/taj/wt/migrate"
    assert out["handle"] == "term-42"
    assert out["raw"]["ok"] is True

    argv = seen["argv"]
    assert isinstance(argv, list)
    assert argv[:3] == ["orca", "worktree", "create"]
    assert argv[argv.index("--repo") + 1] == "path:/Users/taj/projs/localflow"
    assert argv[argv.index("--agent") + 1] == "claude"
    assert "--no-parent" in argv
    assert argv[argv.index("--setup") + 1] == "run"
    assert "--json" in argv
    name = argv[argv.index("--name") + 1]
    assert name == D.slugify(entry["title"])[:40] and len(name) <= 40
    body = argv[argv.index("--prompt") + 1]
    assert body.startswith("Replace the local-file session storage")
    assert "---" not in body and "title:" not in body  # frontmatter stripped
    assert seen["kwargs"].get("timeout") == 120
    assert seen["kwargs"].get("shell") is None  # never shell=True


def test_dispatch_repo_selector_passed_through_verbatim(monkeypatch, entry):
    seen = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        return FakeProc(stdout=SUCCESS_JSON)

    monkeypatch.setattr(subprocess, "run", fake_run)
    for selector in ("path:/abs/repo", "name:localflow", "id:repo-abc", "/bare/path"):
        D.dispatch(entry, selector)
        assert seen["argv"][seen["argv"].index("--repo") + 1] == selector


def test_dispatch_legacy_startup_terminal_handle(monkeypatch, entry):
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: FakeProc(stdout=LEGACY_JSON))
    out = D.dispatch(entry, "path:/x")
    assert out["ok"] is True
    assert out["worktree"] == "repo-abc::/Users/taj/wt/migrate"
    assert out["handle"] == "term-legacy"


def test_dispatch_handle_absent_is_still_ok(monkeypatch, entry):
    body = json.dumps({"ok": True, "result": {"worktree": {"worktreeId": "r::/p"}}})
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: FakeProc(stdout=body))
    out = D.dispatch(entry, "path:/x")
    assert out["ok"] is True and out["handle"] == "" and out["worktree"] == "r::/p"


def test_dispatch_nonzero_exit(monkeypatch, entry):
    monkeypatch.setattr(subprocess, "run",
                        lambda *a, **k: FakeProc(returncode=2, stderr="warn\nrepo not found\n"))
    out = D.dispatch(entry, "path:/nope")
    assert out["ok"] is False
    assert "2" in out["error"] and "repo not found" in out["error"]


def test_dispatch_bad_json(monkeypatch, entry):
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: FakeProc(stdout="totally not json"))
    out = D.dispatch(entry, "path:/x")
    assert out["ok"] is False and "unreadable" in out["error"]


def test_dispatch_orca_reported_failure(monkeypatch, entry):
    monkeypatch.setattr(subprocess, "run",
                        lambda *a, **k: FakeProc(stdout='{"ok": false, "error": "no such repo"}'))
    out = D.dispatch(entry, "path:/x")
    assert out["ok"] is False and "no such repo" in out["error"]


def test_dispatch_timeout(monkeypatch, entry):
    def boom(*a, **k):
        raise subprocess.TimeoutExpired(cmd="orca", timeout=120)
    monkeypatch.setattr(subprocess, "run", boom)
    out = D.dispatch(entry, "path:/x")
    assert out["ok"] is False and "timed out" in out["error"]


def test_dispatch_missing_file(monkeypatch, tmp_path):
    def boom(*a, **k):
        raise AssertionError("orca must not be invoked when the file is missing")
    monkeypatch.setattr(subprocess, "run", boom)
    out = D.dispatch({"title": "x", "path": str(tmp_path / "gone.md")}, "path:/x")
    assert out["ok"] is False and "cannot read" in out["error"]


def test_dispatch_does_not_touch_queue(monkeypatch, tmp_path):
    prompts = D.parse_work_prompts(SAMPLE)
    paths = D.write_prompts(prompts, "Weekly Sync", time.time(), prompts_dir=tmp_path)
    q = PromptQueue(tmp_path / "queue.json")
    e = q.add(prompts, paths, "Weekly Sync")[0]
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: FakeProc(stdout='{"ok": true}'))
    D.dispatch(e, "path:/x")
    assert q.get(e["id"])["status"] == "pending"


def test_module_defaults_point_at_home():
    assert D.QUEUE_PATH == Path("~/.localflow/queue.json").expanduser()
    assert D.DEFAULT_PROMPTS_DIR == Path("~/projs/prompts/meetings").expanduser()


# ------------------------------------------------- compare-and-set semantics ---

def _seed(tmp_path, status="pending"):
    q = D.PromptQueue(tmp_path / "queue.json")
    prompts = [D.WorkPrompt(title="A task", body="do it")]
    paths = D.write_prompts(prompts, "Standup", 1753900000.0, tmp_path / "p")
    entry = q.add(prompts, paths, "Standup")[0]
    if status != "pending":
        q.set_status(entry["id"], status)
    return q, entry["id"]


def test_set_status_expect_allows_matching_current(tmp_path):
    q, eid = _seed(tmp_path)
    updated = q.set_status(eid, "approved", expect=("pending", "failed"))
    assert updated["status"] == "approved"


def test_set_status_expect_raises_on_mismatch(tmp_path):
    q, eid = _seed(tmp_path, status="dispatched")
    with pytest.raises(D.StatusConflict) as excinfo:
        q.set_status(eid, "approved", expect=("pending", "failed"))
    assert excinfo.value.current == "dispatched"
    # The losing write must not have landed.
    assert q.get(eid)["status"] == "dispatched"


def test_set_status_expect_unknown_id_returns_none(tmp_path):
    q, _ = _seed(tmp_path)
    assert q.set_status("nope", "approved", expect=("pending",)) is None


def test_set_status_without_expect_is_unguarded(tmp_path):
    q, eid = _seed(tmp_path, status="dispatched")
    assert q.set_status(eid, "approved")["status"] == "approved"


# ------------------------------------------------------ concurrency guards ---

def test_concurrent_set_status_across_threads_keeps_every_write(tmp_path):
    """Interleaved read-modify-write must not clobber unrelated entries."""
    import threading as _t

    q = D.PromptQueue(tmp_path / "queue.json")
    prompts = [D.WorkPrompt(title=f"Task {i}", body="b") for i in range(20)]
    paths = D.write_prompts(prompts, "Standup", 1753900000.0, tmp_path / "p")
    ids = [e["id"] for e in q.add(prompts, paths, "Standup")]

    barrier = _t.Barrier(len(ids))

    def flip(entry_id):
        barrier.wait()
        q.set_status(entry_id, "approved")

    threads = [_t.Thread(target=flip, args=(i,)) for i in ids]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    reloaded = D.PromptQueue(tmp_path / "queue.json").list()
    assert len(reloaded) == 20
    assert all(e["status"] == "approved" for e in reloaded), "a write was lost"


def test_only_one_writer_wins_a_compare_and_set(tmp_path):
    """Two racing approvals: exactly one succeeds, the other sees the conflict."""
    import threading as _t

    q, eid = _seed(tmp_path)
    results, lock = [], _t.Lock()
    barrier = _t.Barrier(2)

    def race():
        barrier.wait()
        try:
            q.set_status(eid, "dispatched", expect=("pending",))
            outcome = "won"
        except D.StatusConflict:
            outcome = "lost"
        with lock:
            results.append(outcome)

    threads = [_t.Thread(target=race) for _ in range(2)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert sorted(results) == ["lost", "won"]
    assert q.get(eid)["status"] == "dispatched"


# --------------------------------------------------------- add idempotency ---

def test_rerunning_a_meeting_reuses_entries_not_duplicates(tmp_path):
    """Same meeting, same day: files are overwritten, so entries must not double.

    Two entries pointing at one file are two dispatchable handles on the same
    prompt — approving both spawns two worktrees for identical work.
    """
    q = D.PromptQueue(tmp_path / "queue.json")
    prompts = [
        D.WorkPrompt(title="Migrate to redis", body="do it"),
        D.WorkPrompt(title="Update docs", body="write them"),
    ]
    out = tmp_path / "p"

    p1 = D.write_prompts(prompts, "Standup", 1753900000.0, out)
    first = q.add(prompts, p1, "Standup")
    p2 = D.write_prompts(prompts, "Standup", 1753900000.0, out)
    second = q.add(prompts, p2, "Standup")

    assert p1 == p2, "re-run must target the same files"
    assert [e["id"] for e in first] == [e["id"] for e in second]
    assert len(q.list()) == 2, "re-running a meeting duplicated the queue"
    # No id may carry a collision suffix.
    assert all("-" not in e["id"] for e in q.list())
    # Every queued entry points at a distinct file.
    paths = [e["path"] for e in q.list()]
    assert len(set(paths)) == len(paths)


def test_add_preserves_existing_entry_state_on_rerun(tmp_path):
    """A re-run must not silently reset an entry the user already actioned."""
    q = D.PromptQueue(tmp_path / "queue.json")
    prompts = [D.WorkPrompt(title="Migrate to redis", body="do it")]
    out = tmp_path / "p"

    paths = D.write_prompts(prompts, "Standup", 1753900000.0, out)
    eid = q.add(prompts, paths, "Standup")[0]["id"]
    q.set_status(eid, "rejected")

    again = q.add(prompts, D.write_prompts(prompts, "Standup", 1753900000.0, out), "Standup")
    assert again[0]["id"] == eid
    assert q.get(eid)["status"] == "rejected", "re-add resurrected a rejected prompt"
    assert len(q.list()) == 1


def test_distinct_prompts_still_get_distinct_ids(tmp_path):
    q = D.PromptQueue(tmp_path / "queue.json")
    prompts = [
        D.WorkPrompt(title="Task one", body="a"),
        D.WorkPrompt(title="Task two", body="b"),
    ]
    paths = D.write_prompts(prompts, "Standup", 1753900000.0, tmp_path / "p")
    made = q.add(prompts, paths, "Standup")
    assert len({e["id"] for e in made}) == 2
