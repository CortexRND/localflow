"""Stand-in for `claude` (and `headroom`): records argv/stdin/cwd/env, then
answers with canned output chosen by FAKE_CLAUDE_MODE.

Modes: ok (default), is_error, garbage, nonzero, banner, stream, hang.
FAKE_CLAUDE_RECORD names a JSON-lines file each invocation appends to.
"""

import json
import os
import sys
import time


def main() -> int:
    args = sys.argv[1:]
    name = os.environ.get("FAKE_CLAUDE_NAME") or os.path.basename(sys.argv[0])
    if args == ["--version"]:
        print("9.9.9 (Claude Code)")
        return 0
    stdin = sys.stdin.read()
    record = os.environ.get("FAKE_CLAUDE_RECORD")
    if record:
        with open(record, "a", encoding="utf-8") as handle:
            handle.write(json.dumps({
                "name": name, "argv": args, "stdin": stdin, "cwd": os.getcwd(),
                "env": dict(os.environ), "pid": os.getpid(), "pgid": os.getpgid(0)
                if hasattr(os, "getpgid") else None,
            }) + "\n")
    mode = os.environ.get("FAKE_CLAUDE_MODE", "ok")
    result = {
        "type": "result", "subtype": "success", "is_error": False,
        "result": f"fake result for: {stdin[:40]}", "session_id": "fake-session-123",
        "total_cost_usd": 0.0123, "duration_ms": 456, "num_turns": 3,
    }
    if mode == "hang":
        sys.stdout.write("working\n")
        sys.stdout.flush()
        while True:
            time.sleep(1)
    if mode == "garbage":
        print("this is not json")
        print("oops: unexpected token", file=sys.stderr)
        return 0
    if mode == "nonzero":
        print("fatal: not logged in", file=sys.stderr)
        return 3
    if mode == "is_error":
        result.update(is_error=True, subtype="error_during_execution",
                      result="Credit balance is too low")
        print(json.dumps(result))
        return 1
    if mode == "banner":
        print("Headroom wrap v1 — savings profile: coding")
        print("[headroom] proxy ready")
    if mode == "stream":
        print(json.dumps({"type": "system", "subtype": "init", "session_id": "fake-stream-9"}))
        print(json.dumps({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "thinking about it"}]}}))
        result["session_id"] = "fake-stream-9"
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
