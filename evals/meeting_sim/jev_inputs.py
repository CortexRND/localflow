import json, sys
from pathlib import Path
sys.path.insert(0, str(Path.home() / "work/sim-sut"))
from localflow.meetings import read_transcript_log, format_transcript  # noqa
RUN = Path(sys.argv[1]); items = []
for f in sorted(RUN.glob("r*/results/*.json")):
    r = json.loads(f.read_text()); rep = f.parent.parent.name
    log = Path(r["stop"][1]["log_path"]) if isinstance(r["stop"][1], dict) else None
    tx = read_transcript_log(log) if log and log.exists() else ""
    for c in r["chats"]:
        items.append({"type": "chat", "repeat": rep, "scenario": r["scenario"], "q": c["q"], "kind": c["kind"], "truth": c["truth"], "answer": c["answer"], "transcript": tx})
    body = r["notes_text"].split("\n---\n", 1)[-1].split("\nRaw transcript: [[")[0]
    items.append({"type": "notes", "repeat": rep, "scenario": r["scenario"], "transcript": tx, "user_notes": r["notes_final"], "notes": body})
(RUN / "jev_inputs.json").write_text(json.dumps(items)); print(len(items))
