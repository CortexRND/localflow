"""Collect every LLM-graded item of a run into <run_dir>/jev_inputs.json for jev.ts.

    python evals/localflow_sim/jev_inputs.py <run_dir>

Item types (see PROTOCOL.md): meeting_chat and recall_ask go to the trusted
Jev question sets (abstention, answer); meeting_notes, dictation_cleanup and
work_prompts go to advisory custom questions; the agents suite adds the advisory
sets JA1-JA4 (agent_prompt_fidelity, agent_launch_intent, agent_error_helpfulness,
agent_result_faithfulness). Each item carries the gate and item id gates.py uses,
so report.py can line the two judges up.
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

import gates

sys.path.insert(0, os.environ.get("SIM_SUT", str(Path.home() / "work/sim-sut")))

CHAT_GATE = {"answerable": "M9", "unanswerable": "M10", "notes_only": "M11"}


def transcript(result: dict) -> str:
    from localflow.meetings import read_transcript_log

    with tempfile.TemporaryDirectory() as folder:
        log = Path(folder) / "log.md"
        log.write_text(result["log_text"], encoding="utf-8")
        return read_transcript_log(log)


def meeting_items(result: dict, base: dict) -> list[dict]:
    sid = result["scenario"]
    spoken = transcript(result)
    items = [
        {**base, "type": "meeting_chat", "gate": CHAT_GATE[chat["kind"]], "item": f"{sid}:{chat['q']}",
         "kind": chat["kind"], "q": chat["q"], "truth": chat["truth"], "answer": chat["answer"],
         "sources": [spoken]}
        for chat in result["chats"]
    ]
    body = result["notes_text"].split("\n---\n", 1)[-1].split("\nRaw transcript: [[")[0]
    items.append({**base, "type": "meeting_notes", "gate": "M6", "item": sid, "kind": "notes",
                  "transcript": spoken, "user_notes": result["notes_final"], "notes": body})
    return items


def recall_items(result: dict, cell: dict, base: dict) -> list[dict]:
    items = []
    for row in gates.recall_rows(result, cell):
        if row["expect"] == "answer":
            kind = "answerable"
        else:
            kind = "unanswerable" if row["kind"] == "unanswerable" else "filtered"
        items.append({
            **base, "type": "recall_ask", "gate": row["gate"],
            "item": f"{row['item']}:follow-up" if row["kind"] == "multi_turn" else row["item"],
            "kind": kind, "path": row["path"], "q": row["spec"]["q"], "truth": row["spec"]["truth"],
            "answer": row["answer"], "sources": [p["text"] for p in row["turn"]["retrieved"]],
        })
    return items


# JA1-JA4 (PROTOCOL.md "Agents"): which cell field feeds which Jev set.
AGENT_SETS = (
    ("jev_prompt_items", "agent_prompt_fidelity", "prompt"),
    ("jev_intent_items", "agent_launch_intent", "intent"),
    ("jev_error_items", "agent_error_helpfulness", "errors"),
    ("jev_result_items", "agent_result_faithfulness", "results"),
)
MAX_FIELD = 6000


def agents_items(result: dict, cell: dict, base: dict) -> list[dict]:
    """JA1-JA4 items from one agents cell, limited to the cell's frozen `jev_sample`
    (keeps a 3-repeat live Jev pass at ~30 items per repeat)."""
    sample = cell.get("jev_sample") or {}
    items = []
    for field, kind_type, key in AGENT_SETS:
        wanted = sample.get(key)
        for row in result.get(field) or []:
            if wanted is not None and row["id"] not in wanted:
                continue
            item = {**base, "type": kind_type, "gate": cell["gate"], "item": f"{cell['id']}:{row['id']}"}
            if kind_type == "agent_prompt_fidelity":
                item.update(kind="positive", transcript=row["transcript"], rules=row["rules"], prompt=row["prompt"],
                            expected=row["expected"])
            elif kind_type == "agent_launch_intent":
                item.update(kind="expect_launch" if row["expected"] else "expect_no_launch",
                            transcript=row["transcript"], rules=row["rules"], launched=row["launched"],
                            expected=row["expected"], case=row["case"])
            elif kind_type == "agent_error_helpfulness":
                message = "\n".join(x for x in (row.get("detail"), row.get("error_message")) if x)
                item.update(kind="error", cause=row["cause"], detail=row.get("detail", ""),
                            error_message=row.get("error_message", ""), message=message)
            else:
                item.update(kind="result", prompt=row["prompt"], event_log=row["event_log"][:MAX_FIELD],
                            result_text=row["result_text"][:MAX_FIELD], result_preview=row["result_preview"])
            items.append(item)
    return items


def collect(run: Path) -> list[dict]:
    manifest_path = run / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8")) if manifest_path.is_file() else {}
    index = gates.cell_index(gates.active_suites(manifest))
    items: list[dict] = []
    for path in sorted(run.glob("r*/results/*.json")):
        result = json.loads(path.read_text(encoding="utf-8"))
        cid = result.get("cell")
        if cid not in index or "error" in result:
            continue
        suite, cell = index[cid]
        base = {"repeat": path.parent.parent.name, "suite": suite, "cell": cid}
        kind = cell["kind"]
        if kind == "meeting":
            items += meeting_items(result, base)
        elif kind == "workprompt":
            items.append({**base, "type": "work_prompts", "gate": cell["gate"], "item": cid,
                          "kind": cell["expect"], "notes": result["notes"], "prompts_md": result["prompts_md"]})
        elif suite == "recall_ask":
            items += recall_items(result, cell, base)
        elif suite == "agents":
            items += agents_items(result, cell, base)
        elif kind in ("cleanup", "injection"):
            items.append({**base, "type": "dictation_cleanup", "gate": cell["gate"], "item": cid, "kind": kind,
                          "level": cell["level"], "transcript": result["input"], "output": result["output"]})
    return items


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    run = Path(sys.argv[1]).expanduser()
    items = collect(run)
    (run / "jev_inputs.json").write_text(json.dumps(items), encoding="utf-8")
    counts: dict[str, int] = {}
    for item in items:
        counts[item["type"]] = counts.get(item["type"], 0) + 1
    print(json.dumps({"items": len(items), **counts}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
