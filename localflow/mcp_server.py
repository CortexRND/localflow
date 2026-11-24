"""MCP server: lets agents search the user's past dictations and work prompts.

A dependency-free JSON-RPC 2.0 server over stdio (one JSON object per line).
Tools-only and read-only: it never writes under ``~/.localflow``, never opens a
socket and never calls an LLM. History is opened with SQLite ``mode=ro`` and
the prompt queue is read without taking its lock, so running a tool has no
side effects on disk.

Run it with ``localflow-mcp``, ``lf mcp`` or ``python -m localflow.mcp_server``.
"""

from __future__ import annotations

import json
import logging
import os
import sqlite3
import sys
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path
from typing import BinaryIO
from urllib.parse import quote

from localflow import recall
from localflow.dispatch import QUEUE_PATH, STATUSES, _strip_frontmatter
from localflow.history import history_path

log = logging.getLogger("localflow.mcp_server")

SERVER_NAME = "localflow"
PROTOCOL_VERSIONS = ("2025-06-18", "2025-03-26", "2024-11-05")
INSTRUCTIONS = (
    "Search the user's past LocalFlow dictations, spoken commands and meeting work prompts.\n"
    "This is the user's private local history; quote it only as needed for the task."
)

KINDS = ("dictate", "command", "transform")
DEFAULT_LIMIT = 10
MAX_LIMIT = 50
MAX_TEXT_CHARS = 20_000
MAX_PROMPT_FILE_BYTES = 256 * 1024
_HISTORY_SCHEMA_VERSION = 1
HISTORY_DISABLED = "Dictation history is disabled (`[history] enabled = false`)"

PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602


class ToolError(Exception):
    """A tool failed; the message is returned to the client with ``isError``."""


class InvalidParams(Exception):
    """A ``tools/call`` named an unknown tool or passed bad arguments."""


# ----------------------------------------------------------------- sources ---

PROMPTS_CWD_ENV = "LOCALFLOW_MCP_PROMPTS_CWD"


@dataclass(frozen=True)
class Sources:
    history_path: Path
    queue_path: Path
    history_enabled: Callable[[], bool]
    # ``LOCALFLOW_MCP_PROMPTS_CWD``: the launching app's cwd, the base for
    # relative queue prompt paths. None (unset) keeps this process's cwd;
    # set but not absolute leaves relative paths unread.
    prompts_cwd: str | None = None


def _config_history_enabled() -> bool:
    """``[history] enabled`` without ``load_config``'s legacy migration side effects.

    ``load_config`` saves a migrated config and may write the keyring when only
    ``~/.localflow.toml`` exists; here the legacy file is converted in memory only.
    """
    from localflow import config
    from localflow.config_migrate import migrate_v1

    target = config.config_path()
    if target.exists():
        return config._config_from_v2(config._parse(target)).history_enabled
    legacy = config.legacy_config_path()
    if legacy.exists():
        data, _ = migrate_v1(config._parse(legacy))
        return config._config_from_v2(data).history_enabled
    return config.Config().history_enabled


def default_sources() -> Sources:
    return Sources(
        history_path=history_path(),
        queue_path=QUEUE_PATH,
        history_enabled=_config_history_enabled,
        prompts_cwd=os.environ.get(PROMPTS_CWD_ENV),
    )


# ----------------------------------------------------------------- helpers ---

def _clamp_limit(limit: object) -> int:
    try:
        value = int(limit)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        value = DEFAULT_LIMIT
    return max(1, min(MAX_LIMIT, value))


def _when(ts: float | None) -> str:
    if ts is None:
        return ""
    return datetime.fromtimestamp(ts).isoformat(timespec="seconds")


def _cut(text: str | None) -> tuple[str, bool]:
    text = text or ""
    if len(text) > MAX_TEXT_CHARS:
        return text[:MAX_TEXT_CHARS], True
    return text, False


def _query_terms(query: str) -> set[str]:
    plain, _ = recall.extract_phrases(query)
    return set(recall.tokenize(plain))


def _bound(value: str | None, name: str, *, end: bool) -> float | None:
    if value is None or not str(value).strip():
        return None
    try:
        start = recall.parse_since(value)
    except ValueError as exc:
        raise ToolError(str(exc).replace("since", name, 1)) from None
    if end and not recall._RELATIVE_RE.match(str(value).strip()):
        start = start + timedelta(days=1)
    return start.timestamp()


def _rank_refs(
    query: str, passages: list[recall.Passage], limit: int,
) -> list[recall.Passage]:
    """Best passage per ``ref``, at most ``limit`` refs.

    Ranks every passage: a fixed ``k`` lets one entry with many matching chunks
    crowd out other matching entries before the per-ref dedupe.
    """
    best: dict[str, recall.Passage] = {}
    for passage in recall.rank(query, passages, k=len(passages)):
        best.setdefault(passage.ref, passage)
        if len(best) >= limit:
            break
    return list(best.values())


# ----------------------------------------------------------------- history ---

def _open_history(src: Sources) -> sqlite3.Connection | None:
    if not src.history_enabled():
        raise ToolError(HISTORY_DISABLED)
    path = Path(src.history_path)
    if not path.is_file():
        return None
    try:
        connection = sqlite3.connect(
            f"file:{quote(str(path.resolve()))}?mode=ro", uri=True, timeout=30,
        )
        connection.row_factory = sqlite3.Row
        version = connection.execute("PRAGMA user_version").fetchone()[0]
    except sqlite3.Error as exc:
        raise ToolError(f"cannot read dictation history: {exc}") from None
    if version > _HISTORY_SCHEMA_VERSION:
        connection.close()
        raise ToolError("history written by a newer LocalFlow; update LocalFlow to read it")
    if version == 0:  # file exists but was never initialized
        connection.close()
        return None
    return connection


def search_dictations(
    src: Sources,
    *,
    query: str = "",
    since: str | None = None,
    until: str | None = None,
    app: str | None = None,
    kind: str | None = None,
    limit: int = DEFAULT_LIMIT,
) -> dict:
    limit = _clamp_limit(limit)
    if kind is not None and kind not in KINDS:
        raise ToolError(f"invalid kind {kind!r}; expected one of {', '.join(KINDS)}")
    start = _bound(since, "since", end=False)
    stop = _bound(until, "until", end=True)
    query = (query or "").strip()
    connection = _open_history(src)
    if connection is None:
        return {"count": 0, "match": "ranked" if query else "recent", "results": []}

    sql = ["SELECT * FROM dictations WHERE status = 'ok'"]
    params: list[object] = []
    if start is not None:
        sql.append("AND ts >= ?")
        params.append(start)
    if stop is not None:
        sql.append("AND ts < ?")
        params.append(stop)
    if kind:
        sql.append("AND kind = ?")
        params.append(kind)
    if app and app.strip():
        sql.append("AND (instr(lower(coalesce(app_name, '')), lower(?)) > 0 OR bundle_id = ?)")
        params.extend((app.strip(), app.strip()))
    sql.append("ORDER BY ts DESC, id DESC LIMIT ?")
    params.append(recall.HISTORY_SCAN_LIMIT)
    try:
        with connection:
            rows = [dict(r) for r in connection.execute(" ".join(sql), params).fetchall()]
    except sqlite3.Error as exc:
        raise ToolError(f"cannot read dictation history: {exc}") from None
    finally:
        connection.close()
    rows = [
        r for r in rows
        if (r.get("final_text") or "").strip() or (r.get("raw_text") or "").strip()
    ]
    by_id = {str(r["id"]): r for r in rows}

    def result(row: dict, text: str, terms: set[str]) -> dict:
        return {
            "id": row["id"],
            "when": _when(row["ts"]),
            "kind": row["kind"],
            "app": row.get("app_name") or "",
            "category": row.get("category") or "",
            "words": row.get("words") or 0,
            "duration_s": row.get("duration_s") or 0,
            "snippet": recall._snippet(text, terms),
        }

    def best_text(row: dict) -> str:
        return (row.get("final_text") or "").strip() or (row.get("raw_text") or "").strip()

    if not query:
        results = [result(r, best_text(r), set()) for r in rows[:limit]]
        return {"count": len(results), "match": "recent", "results": results}

    passages: list[recall.Passage] = []
    for row in rows:
        final = (row.get("final_text") or "").strip()
        raw = (row.get("raw_text") or "").strip()
        texts = [final] + ([raw] if raw and raw != final else [])
        app_name = row.get("app_name") or ""
        for text in texts:
            for chunk in recall.chunk_lines(text):
                passages.append(recall.Passage(
                    text=chunk, source=recall.SOURCE_DICTATION, title=app_name,
                    app=app_name, ts=float(row["ts"]), ref=str(row["id"]),
                ))
    terms = _query_terms(query)
    top = _rank_refs(query, passages, limit)
    if top:
        results = [result(by_id[p.ref], p.text, terms) for p in top]
        return {"count": len(results), "match": "ranked", "results": results}

    needle = query.replace('"', "").strip().lower()
    results = []
    if needle:
        for row in rows:
            for text in (row.get("final_text") or "", row.get("raw_text") or ""):
                if needle in text.lower():
                    results.append(result(row, _centre(text, needle), set()))
                    break
            if len(results) >= limit:
                break
    return {"count": len(results), "match": "substring", "results": results}


def _centre(text: str, needle: str) -> str:
    """Snippet centred on a substring hit (``recall._snippet`` matches whole words)."""
    flat = " ".join(text.split())
    if len(flat) <= recall.SNIPPET_CHARS:
        return flat
    at = flat.lower().find(needle)
    start = max(0, at - 60) if at >= 0 else 0
    piece = flat[start:start + recall.SNIPPET_CHARS].strip()
    return (
        ("…" if start else "") + piece
        + ("…" if start + recall.SNIPPET_CHARS < len(flat) else "")
    )


def get_dictation(src: Sources, *, id: int) -> dict:
    connection = _open_history(src)
    if connection is None:
        raise ToolError(f"no dictation with id {id}")
    try:
        with connection:
            row = connection.execute(
                "SELECT * FROM dictations WHERE id = ?", (int(id),)
            ).fetchone()
    except sqlite3.Error as exc:
        raise ToolError(f"cannot read dictation history: {exc}") from None
    finally:
        connection.close()
    if row is None:
        raise ToolError(f"no dictation with id {id}")
    row = dict(row)
    final_text, cut_final = _cut(row.get("final_text"))
    raw_text, cut_raw = _cut(row.get("raw_text"))
    return {
        "id": row["id"],
        "when": _when(row["ts"]),
        "kind": row["kind"],
        "status": row["status"],
        "app": row.get("app_name") or "",
        "bundle_id": row.get("bundle_id") or "",
        "category": row.get("category") or "",
        "words": row.get("words") or 0,
        "duration_s": row.get("duration_s") or 0,
        "final_text": final_text,
        "raw_text": raw_text,
        "truncated": cut_final or cut_raw,
    }


# ----------------------------------------------------------------- prompts ---

def _read_queue(path: Path) -> list[dict]:
    """Same tolerance as ``PromptQueue._read``, without the lock file."""
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except FileNotFoundError:
        return []
    except (OSError, ValueError):
        log.warning("queue unreadable; treating as empty: %s", path)
        return []
    if not isinstance(data, list):
        return []
    return [e for e in data if isinstance(e, dict) and isinstance(e.get("id"), str)]


def _read_prompt_body(path_value: object, cwd: str | None = None) -> str | None:
    """Prompt body without frontmatter, or None if the file is missing/unsafe.

    A relative path is read against ``cwd`` (``Sources.prompts_cwd``) when set.
    """
    if not path_value:
        return None
    path = Path(str(path_value)).expanduser()
    if cwd is not None and not path.is_absolute():
        base = Path(cwd)
        if not base.is_absolute():
            return None
        path = base / path
        if not path.is_absolute():  # e.g. a Windows drive-relative `C:x.md`
            return None
    try:
        if path.suffix.lower() != ".md" or not path.is_file():
            return None
        if path.stat().st_size > MAX_PROMPT_FILE_BYTES:
            return None
        return _strip_frontmatter(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError):
        return None


def _created_ts(value: object) -> float | None:
    try:
        return datetime.fromisoformat(str(value)).timestamp()
    except (TypeError, ValueError):
        return None


def search_prompts(
    src: Sources,
    *,
    query: str = "",
    status: str | None = None,
    limit: int = DEFAULT_LIMIT,
) -> dict:
    limit = _clamp_limit(limit)
    if status is not None and status not in STATUSES:
        raise ToolError(f"invalid status {status!r}; expected one of {', '.join(STATUSES)}")
    query = (query or "").strip()
    entries = [
        e for e in _read_queue(src.queue_path)
        if status is None or e.get("status") == status
    ]
    entries.sort(key=lambda e: str(e.get("created") or ""), reverse=True)
    by_id = {e["id"]: e for e in entries}

    def result(entry: dict, snippet: str) -> dict:
        return {
            "id": entry["id"],
            "title": entry.get("title") or "",
            "meeting": entry.get("meeting") or "",
            "status": entry.get("status") or "",
            "created": entry.get("created") or "",
            "snippet": snippet,
        }

    def full_text(entry: dict) -> str:
        body = _read_prompt_body(entry.get("path"), src.prompts_cwd) or ""
        return "\n".join(
            part for part in (entry.get("title") or "", entry.get("meeting") or "", body)
            if part
        )

    if not query:
        results = [
            result(e, recall._snippet(
                _read_prompt_body(e.get("path"), src.prompts_cwd) or "", set(),
            ))
            for e in entries[:limit]
        ]
        return {"count": len(results), "match": "recent", "results": results}

    texts = {e["id"]: full_text(e) for e in entries}
    passages = [
        recall.Passage(
            text=chunk, source="prompt", title=e.get("title") or "",
            ts=_created_ts(e.get("created")), ref=e["id"],
        )
        for e in entries
        for chunk in recall.chunk_lines(texts[e["id"]])
    ]
    terms = _query_terms(query)
    top = _rank_refs(query, passages, limit)
    if top:
        results = [result(by_id[p.ref], recall._snippet(p.text, terms)) for p in top]
        return {"count": len(results), "match": "ranked", "results": results}

    needle = query.replace('"', "").strip().lower()
    results = []
    if needle:
        for entry in entries:
            text = texts[entry["id"]]
            if needle in text.lower():
                results.append(result(entry, _centre(text, needle)))
                if len(results) >= limit:
                    break
    return {"count": len(results), "match": "substring", "results": results}


def get_prompt(src: Sources, *, id: str) -> dict:
    entry = next((e for e in _read_queue(src.queue_path) if e["id"] == id), None)
    if entry is None:
        raise ToolError(f"no work prompt with id {id!r}")
    body = _read_prompt_body(entry.get("path"), src.prompts_cwd)
    text, truncated = _cut(body)
    out = {
        "id": entry["id"],
        "title": entry.get("title") or "",
        "meeting": entry.get("meeting") or "",
        "status": entry.get("status") or "",
        "created": entry.get("created") or "",
        "path": entry.get("path") or "",
        "worktree": entry.get("worktree") or "",
        "repo": entry.get("repo") or "",
        "error": entry.get("error") or "",
        "body": text,
        "truncated": truncated,
    }
    if body is None:
        out["note"] = "prompt file missing"
    return out


# ------------------------------------------------------------------- tools ---

_READ_ONLY = {"readOnlyHint": True, "openWorldHint": False}
_LIMIT_SCHEMA = {
    "type": "integer",
    "description": f"Maximum results (1-{MAX_LIMIT}, default {DEFAULT_LIMIT}).",
}
_DATE_HINT = "today, yesterday, 7d, 30d or YYYY-MM-DD"

TOOLS: list[dict] = [
    {
        "name": "search_dictations",
        "description": (
            "Search or list the user's past LocalFlow dictations, spoken commands and "
            "transforms. Without a query, returns the newest first. Quote a phrase to "
            "boost exact matches."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Words to find; omit to list recent."},
                "since": {"type": "string", "description": f"Start: {_DATE_HINT}."},
                "until": {
                    "type": "string",
                    "description": f"End: {_DATE_HINT} (a named day is inclusive).",
                },
                "app": {
                    "type": "string",
                    "description": "App name substring (case-insensitive) or exact bundle id.",
                },
                "kind": {"type": "string", "enum": list(KINDS)},
                "limit": _LIMIT_SCHEMA,
            },
            "additionalProperties": False,
        },
        "annotations": _READ_ONLY,
        "handler": search_dictations,
    },
    {
        "name": "get_dictation",
        "description": "Full text of one dictation by id (from search_dictations).",
        "inputSchema": {
            "type": "object",
            "properties": {"id": {"type": "integer"}},
            "required": ["id"],
            "additionalProperties": False,
        },
        "annotations": _READ_ONLY,
        "handler": get_dictation,
    },
    {
        "name": "search_prompts",
        "description": (
            "Search or list meeting work prompts (title, meeting and prompt body). "
            "Without a query, returns the newest first."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Words to find; omit to list recent."},
                "status": {"type": "string", "enum": list(STATUSES)},
                "limit": _LIMIT_SCHEMA,
            },
            "additionalProperties": False,
        },
        "annotations": _READ_ONLY,
        "handler": search_prompts,
    },
    {
        "name": "get_prompt",
        "description": "Full body and status of one work prompt by id (from search_prompts).",
        "inputSchema": {
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
            "additionalProperties": False,
        },
        "annotations": _READ_ONLY,
        "handler": get_prompt,
    },
]
_TOOLS_BY_NAME = {tool["name"]: tool for tool in TOOLS}
_JSON_TYPES = {
    "string": lambda v: isinstance(v, str),
    "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
}


def _validate(tool: dict, arguments: object) -> dict:
    if arguments is None:
        arguments = {}
    if not isinstance(arguments, dict):
        raise InvalidParams("arguments must be an object")
    schema = tool["inputSchema"]
    props = schema["properties"]
    unknown = sorted(set(arguments) - set(props))
    if unknown:
        raise InvalidParams(f"unknown argument(s): {', '.join(unknown)}")
    missing = [key for key in schema.get("required", []) if key not in arguments]
    if missing:
        raise InvalidParams(f"missing argument(s): {', '.join(missing)}")
    for key, value in arguments.items():
        spec = props[key]
        if value is None and key not in schema.get("required", []):
            continue
        if not _JSON_TYPES[spec["type"]](value):
            raise InvalidParams(f"{key} must be a {spec['type']}")
        if "enum" in spec and value not in spec["enum"]:
            raise InvalidParams(f"{key} must be one of {', '.join(spec['enum'])}")
    return {k: v for k, v in arguments.items() if v is not None}


def call_tool(src: Sources, name: object, arguments: object) -> dict:
    tool = _TOOLS_BY_NAME.get(name) if isinstance(name, str) else None
    if tool is None:
        raise InvalidParams(f"unknown tool: {name!r}")
    kwargs = _validate(tool, arguments)
    try:
        data = tool["handler"](src, **kwargs)
    except ToolError as exc:
        return {"content": [{"type": "text", "text": str(exc)}], "isError": True}
    except Exception as exc:  # a broken tool must not kill the server
        log.exception("tool %s failed", name)
        return {
            "content": [{"type": "text", "text": f"{name} failed: {exc}"}],
            "isError": True,
        }
    text = json.dumps(data, ensure_ascii=False, separators=(",", ":"))
    return {"content": [{"type": "text", "text": text}], "structuredContent": data}


# ---------------------------------------------------------------- protocol ---

def _version() -> str:
    from importlib.metadata import PackageNotFoundError, version

    try:
        return version("localflow")
    except PackageNotFoundError:
        return "0"


def _error(request_id: object, code: int, message: str) -> dict:
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def _result(request_id: object, result: dict) -> dict:
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def handle_message(src: Sources, message: object) -> dict | None:
    """One parsed JSON-RPC message -> reply, or None for notifications."""
    if isinstance(message, list):
        return _error(None, INVALID_REQUEST, "batch requests are not supported")
    if not isinstance(message, dict) or not isinstance(message.get("method"), str):
        if isinstance(message, dict) and "method" not in message and "id" in message:
            return None  # a response to something we never sent; ignore
        request_id = message.get("id") if isinstance(message, dict) else None
        return _error(request_id, INVALID_REQUEST, "invalid request")
    method = message["method"]
    if "id" not in message:
        return None  # notification (notifications/initialized, cancelled, ...)
    request_id = message["id"]
    params = message.get("params") or {}
    if not isinstance(params, dict):
        return _error(request_id, INVALID_PARAMS, "params must be an object")

    if method == "initialize":
        requested = params.get("protocolVersion")
        version = requested if requested in PROTOCOL_VERSIONS else PROTOCOL_VERSIONS[0]
        return _result(request_id, {
            "protocolVersion": version,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "version": _version()},
            "instructions": INSTRUCTIONS,
        })
    if method == "ping":
        return _result(request_id, {})
    if method == "tools/list":
        tools = [{k: v for k, v in tool.items() if k != "handler"} for tool in TOOLS]
        return _result(request_id, {"tools": tools})
    if method == "tools/call":
        try:
            return _result(
                request_id, call_tool(src, params.get("name"), params.get("arguments")),
            )
        except InvalidParams as exc:
            return _error(request_id, INVALID_PARAMS, str(exc))
    return _error(request_id, METHOD_NOT_FOUND, f"method not found: {method}")


def serve(src: Sources, stdin: BinaryIO, stdout: BinaryIO) -> None:
    """Read JSON-RPC lines from ``stdin`` until EOF, writing replies to ``stdout``."""
    for raw in stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except (ValueError, UnicodeDecodeError):
            reply = _error(None, PARSE_ERROR, "parse error")
        else:
            try:
                reply = handle_message(src, message)
            except Exception as exc:  # never let one message kill the loop
                log.exception("unhandled error")
                request_id = message.get("id") if isinstance(message, dict) else None
                reply = _error(request_id, -32603, f"internal error: {exc}")
        if reply is not None:
            stdout.write(json.dumps(reply, ensure_ascii=False).encode("utf-8") + b"\n")
            stdout.flush()


def main() -> None:
    out = sys.stdout.buffer
    # Anything that prints by accident goes to stderr, never into the protocol stream.
    sys.stdout = sys.stderr
    logging.basicConfig(
        level=logging.WARNING, stream=sys.stderr,
        format="localflow-mcp: %(levelname)s %(message)s",
    )
    try:
        serve(default_sources(), sys.stdin.buffer, out)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
