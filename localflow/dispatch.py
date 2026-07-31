"""Turn generated work prompts into files on disk, a reviewable queue, and
Orca worktrees.

Pipeline: WorkPromptGenerator emits a Markdown block list -> parse_work_prompts
-> write_prompts (one file per prompt under ~/projs/prompts/meetings) ->
PromptQueue (pending, awaiting the user's approval) -> dispatch (hand the
approved prompt to a fresh Orca worktree running an agent).
"""

import hashlib
import json
import logging
import os
import re
import subprocess
import threading
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

try:
    import fcntl
except ImportError:  # non-POSIX; the queue falls back to in-process locking
    fcntl = None

log = logging.getLogger("localflow.dispatch")

DEFAULT_PROMPTS_DIR = Path("~/projs/prompts/meetings").expanduser()
QUEUE_PATH = Path("~/.localflow/queue.json").expanduser()

STATUSES = ("pending", "approved", "rejected", "dispatched", "failed")

_FALLBACK = "(No new work identified.)"


class StatusConflict(Exception):
    """A compare-and-set lost: the entry was not in an expected status."""

    def __init__(self, entry_id: str, current: str, expected: tuple[str, ...]):
        self.entry_id = entry_id
        self.current = current
        self.expected = expected
        super().__init__(
            f"cannot move {entry_id} from {current!r}; expected one of {', '.join(expected)}"
        )


# ------------------------------------------------------------------ parsing ---

@dataclass
class WorkPrompt:
    title: str
    body: str


def parse_work_prompts(md: str) -> list[WorkPrompt]:
    """Parse the WorkPromptGenerator output into (title, body) pairs.

    Tolerant by design: the input is model output, so stray blank lines, extra
    spaces after '###', and headings with no body are shrugged off rather than
    raising.
    """
    if not md or not md.strip():
        return []
    if md.strip() == _FALLBACK:
        return []

    prompts: list[WorkPrompt] = []
    title: str | None = None
    lines: list[str] = []

    def flush() -> None:
        nonlocal title, lines
        if title and lines:
            prompts.append(WorkPrompt(title=title, body=" ".join(lines)))
        title, lines = None, []

    for raw in md.splitlines():
        line = raw.strip()
        if line.startswith("#"):
            stripped = line.lstrip("#").strip()
            if stripped:
                flush()
                title = stripped
            continue
        if line.startswith(">"):
            if title is None:
                continue  # quote with no heading: not a block we understand
            text = line[1:].strip()
            if text:
                lines.append(text)
            continue
        # Anything else (blank lines, the fallback line, prose) ends nothing on
        # its own; a block is terminated by the next heading or by EOF.

    flush()
    return prompts


def slugify(text: str) -> str:
    """Filesystem- and branch-safe slug, at most 50 chars, never empty."""
    slug = re.sub(r"[^a-z0-9]+", "-", (text or "").lower()).strip("-")
    if not slug:
        return "untitled"
    if len(slug) > 50:
        cut = slug[:50]
        # Prefer a word boundary, but not one that throws away most of the slug.
        if "-" in cut[20:]:
            cut = cut[: cut.rindex("-")]
        slug = cut.strip("-") or slug[:50].strip("-")
    return slug or "untitled"


# ------------------------------------------------------------------ writing ---

def write_prompts(
    prompts: list[WorkPrompt],
    meeting_title: str,
    started_at: float | datetime,
    prompts_dir: Path = DEFAULT_PROMPTS_DIR,
) -> list[Path]:
    """Write one Markdown file per prompt into a per-meeting directory.

    started_at is a UNIX timestamp or a datetime; either way the date and the
    `created` stamp are local time.
    """
    if not prompts:
        return []
    stamp = started_at if isinstance(started_at, datetime) else datetime.fromtimestamp(started_at)
    folder = Path(prompts_dir) / f"{stamp.strftime('%Y-%m-%d')}-{slugify(meeting_title)}"
    folder.mkdir(parents=True, exist_ok=True)

    written: list[Path] = []
    for i, prompt in enumerate(prompts, start=1):
        path = folder / f"{i:02d}-{slugify(prompt.title)}.md"
        path.write_text(
            "---\n"
            f"title: {prompt.title}\n"
            f"meeting: {meeting_title}\n"
            f"created: {stamp.isoformat()}\n"
            "status: pending\n"
            "---\n\n"
            f"{prompt.body}\n",
            encoding="utf-8",
        )
        written.append(path)
    return written


def _strip_frontmatter(text: str) -> str:
    if text.startswith("---"):
        parts = text.split("\n---", 1)
        if len(parts) == 2:
            return parts[1].lstrip("-").lstrip("\n")
    return text


# -------------------------------------------------------------------- queue ---

class PromptQueue:
    """A JSON list of prompt entries awaiting approval and dispatch.

    Two writers are guaranteed here by design: the server owns approve/reject
    while dispatch is CLI-only, so a separate process is mutating this file
    while the server runs. Every read-modify-write therefore takes both a
    threading.Lock (threads within one process) and an flock on a sidecar lock
    file (across processes). The sidecar exists because _write replaces the
    queue file's inode, which would drop a lock held on the file itself.
    Writes remain atomic against readers via os.replace.
    """

    def __init__(self, path: Path = QUEUE_PATH):
        self.path = Path(path)
        self._lock = threading.Lock()
        self._lock_path = self.path.with_suffix(self.path.suffix + ".lock")

    # -- storage --

    @contextmanager
    def _exclusive(self):
        """Hold the in-process lock and the cross-process flock together."""
        with self._lock:
            handle = None
            if fcntl is not None:  # non-POSIX: in-process safety only
                try:
                    self.path.parent.mkdir(parents=True, exist_ok=True)
                    handle = open(self._lock_path, "a+")
                    fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
                except OSError:
                    # A lock we cannot take (read-only parent, NFS without lockd)
                    # must not cost us the write itself: degrade to unlocked.
                    log.warning("queue lock unavailable, proceeding unlocked: %s",
                                self._lock_path, exc_info=True)
                    if handle is not None:
                        handle.close()
                        handle = None
            try:
                yield
            finally:
                if handle is not None:
                    try:
                        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
                    finally:
                        handle.close()

    def _read(self) -> list[dict]:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return []
        except Exception:
            log.exception("queue unreadable; treating as empty: %s", self.path)
            return []
        return data if isinstance(data, list) else []

    def _write(self, entries: list[dict]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        # Per-pid temp name: two processes sharing one temp path race to
        # os.replace it and the loser dies on a missing file.
        tmp = self.path.with_suffix(f"{self.path.suffix}.{os.getpid()}.tmp")
        tmp.write_text(json.dumps(entries, indent=2), encoding="utf-8")
        os.replace(tmp, self.path)

    # -- api --

    def add(self, prompts: list[WorkPrompt], paths: list[Path], meeting_title: str) -> list[dict]:
        """Queue prompts, idempotently on (path, title).

        Re-running a meeting on the same day rewrites the SAME files — the
        folder is date+meeting and the filename is the within-run index, so
        every component is deterministic. Suffixing a colliding id would turn
        one prompt file into two independently approvable entries, and
        approving both dispatches the same prompt to two worktrees. The digest
        is a dedupe key, so a collision means "already queued", not "new".
        """
        created: list[dict] = []
        now = datetime.now().isoformat()
        with self._exclusive():
            entries = self._read()
            by_id = {e.get("id"): e for e in entries}
            for prompt, path in zip(prompts, paths):
                entry_id = _entry_id(path, prompt.title)
                existing = by_id.get(entry_id)
                if existing is not None:
                    created.append(existing)
                    continue
                entry = {
                    "id": entry_id,
                    "title": prompt.title,
                    "path": str(path),
                    "meeting": meeting_title,
                    "created": now,
                    "status": "pending",
                    "worktree": "",
                    "handle": "",
                    "repo": "",
                    "error": "",
                }
                by_id[entry_id] = entry
                entries.append(entry)
                created.append(entry)
            self._write(entries)
        return created

    def list(self, status: str | None = None) -> list[dict]:
        with self._exclusive():
            entries = self._read()
        return [e for e in entries if status is None or e.get("status") == status]

    def get(self, entry_id: str) -> dict | None:
        with self._exclusive():
            for entry in self._read():
                if entry.get("id") == entry_id:
                    return entry
        return None

    def set_status(
        self,
        entry_id: str,
        status: str,
        expect: tuple[str, ...] | None = None,
        **fields,
    ) -> dict | None:
        """Move an entry to `status`. Returns None if the id is unknown.

        `expect` makes this a compare-and-set: the move only happens if the
        entry's current status is in `expect`, otherwise StatusConflict is
        raised. Checking the status in a separate get() first is not equivalent
        — another process can dispatch the entry in between, and the second
        writer would silently undo the first.
        """
        if status not in STATUSES:
            raise ValueError(
                f"unknown status {status!r}; expected one of {', '.join(sorted(STATUSES))}"
            )
        with self._exclusive():
            entries = self._read()
            for entry in entries:
                if entry.get("id") == entry_id:
                    current = entry.get("status", "")
                    if expect is not None and current not in expect:
                        raise StatusConflict(entry_id, current, expect)
                    entry["status"] = status
                    # Re-approving clears the previous attempt so a retried entry
                    # never shows a stale error beside a fresh worktree.
                    if status == "approved":
                        entry["error"] = ""
                        entry["worktree"] = ""
                        entry["handle"] = ""
                    entry.update(fields)
                    self._write(entries)
                    return entry
        return None


def _entry_id(path: Path, title: str) -> str:
    """Stable dedupe key. Same (path, title) always yields the same id."""
    return hashlib.sha256(f"{path}\n{title}".encode("utf-8")).hexdigest()[:8]


# --------------------------------------------------------------------- orca ---

def _parse_json_stdout(stdout: str) -> dict:
    """Orca's stdout can carry leading noise; take everything from the first {."""
    start = stdout.find("{")
    if start < 0:
        raise ValueError("no JSON in output")
    return json.loads(stdout[start:])


def orca_available() -> tuple[bool, str]:
    """Is the Orca app up with a reachable runtime? Returns (ok, reason)."""
    try:
        proc = subprocess.run(
            ["orca", "status", "--json"],
            capture_output=True, text=True, timeout=15,
        )
    except FileNotFoundError:
        return False, "orca CLI not found on PATH"
    except subprocess.TimeoutExpired:
        return False, "orca status timed out"
    except Exception as exc:
        return False, f"orca status failed: {exc}"

    if proc.returncode != 0:
        return False, f"orca status exited {proc.returncode}"
    try:
        # stderr carries a benign SecCodeCheckValidity line; ignore it entirely.
        data = _parse_json_stdout(proc.stdout or "")
    except Exception as exc:
        return False, f"unreadable orca status output: {exc}"

    result = data.get("result") or {}
    if not (result.get("app") or {}).get("running"):
        return False, f"Orca app is not running — run `orca open`{_headless_note()}"
    if not (result.get("runtime") or {}).get("reachable"):
        state = (result.get("runtime") or {}).get("state") or "unknown"
        return False, f"Orca runtime not reachable ({state}) — run `orca open`{_headless_note()}"
    return True, ""


def _headless_note() -> str:
    """`orca open` launches a GUI desktop app, so it is a dead end without one."""
    if os.environ.get("SSH_CONNECTION") or os.environ.get("SSH_TTY"):
        return " (not possible over SSH: it launches a GUI app on the Mac's own display)"
    return ""


def dispatch(entry: dict, repo: str, agent: str = "claude") -> dict:
    """Create an Orca worktree running `agent` on this entry's prompt.

    Never mutates the queue — the caller records the outcome.
    """
    path = Path(entry.get("path", ""))
    try:
        body = _strip_frontmatter(path.read_text(encoding="utf-8")).strip()
    except Exception as exc:
        return {"ok": False, "error": f"cannot read prompt file {path}: {exc}"}
    if not body:
        return {"ok": False, "error": f"prompt file is empty: {path}"}

    name = slugify(entry.get("title", ""))[:40].strip("-") or "untitled"
    argv = [
        "orca", "worktree", "create",
        # --repo takes a selector (id:/name:/path:); passed through verbatim, the
        # caller owns the form. --no-parent keeps this worktree out of whatever
        # happens to be active, and --setup run runs the repo's setup hooks:
        # both matter when nobody is watching the dispatch.
        "--repo", repo,
        "--name", name,
        "--agent", agent,
        "--prompt", body,
        "--no-parent",
        "--setup", "run",
        "--json",
    ]
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, timeout=120)
    except FileNotFoundError:
        return {"ok": False, "error": "orca CLI not found on PATH"}
    except subprocess.TimeoutExpired:
        return {"ok": False, "error": "orca worktree create timed out after 120s"}
    except Exception as exc:
        return {"ok": False, "error": f"orca worktree create failed: {exc}"}

    if proc.returncode != 0:
        return {"ok": False, "error": f"orca exited {proc.returncode}: {_last_line(proc.stderr)}"}
    try:
        data = _parse_json_stdout(proc.stdout or "")
    except Exception as exc:
        return {"ok": False, "error": f"unreadable orca output: {exc}"}
    if data.get("ok") is False:
        return {"ok": False, "error": f"orca reported failure: {_last_line(str(data.get('error') or data))}"}
    return {
        "ok": True,
        "worktree": _worktree_id(data, name),
        "handle": _terminal_handle(data),
        "raw": data,
    }


def _worktree_id(data: dict, fallback: str) -> str:
    """The worktree id has the form <repoId>::<path>; fall back to the name."""
    result = data.get("result")
    if isinstance(result, dict):
        worktree = result.get("worktree")
        source = worktree if isinstance(worktree, dict) else result
        for key in ("worktreeId", "id", "path", "name", "branch"):
            value = source.get(key)
            if isinstance(value, str) and value:
                return value
    return fallback


def _terminal_handle(data: dict) -> str:
    """Terminal handle for reading agent output later.

    Newer runtimes put it at result.agentTerminalHandle; older ones only expose
    result.startupTerminal.handle, and folder-based repos may return neither.
    """
    result = data.get("result")
    if not isinstance(result, dict):
        return ""
    handle = result.get("agentTerminalHandle")
    if isinstance(handle, str) and handle:
        return handle
    startup = result.get("startupTerminal")
    if isinstance(startup, dict):
        handle = startup.get("handle")
        if isinstance(handle, str) and handle:
            return handle
    return ""


def _last_line(text: str) -> str:
    lines = [line for line in (text or "").splitlines() if line.strip()]
    return lines[-1].strip()[:200] if lines else "no output"
