import json, os, sys, tempfile
from pathlib import Path
sys.path.insert(0, os.environ.get("SIM_SUT", str(Path.home() / "work/sim-sut")))
from localflow.meetings import read_transcript_log  # noqa


def transcript(r):
    with tempfile.TemporaryDirectory() as d:
        log = Path(d) / "log.md"
        log.write_text(r["log_text"], encoding="utf-8")
        return read_transcript_log(log)

RUN = Path(sys.argv[1]); items = []
for f in sorted(RUN.glob("r*/results/*.json")):
    r = json.loads(f.read_text()); rep = f.parent.parent.name
    tx = transcript(r)
    for c in r["chats"]:
        items.append({"type": "chat", "repeat": rep, "scenario": r["scenario"], "q": c["q"], "kind": c["kind"], "truth": c["truth"], "answer": c["answer"], "transcript": tx})
    body = r["notes_text"].split("\n---\n", 1)[-1].split("\nRaw transcript: [[")[0]
    items.append({"type": "notes", "repeat": rep, "scenario": r["scenario"], "transcript": tx, "user_notes": r["notes_final"], "notes": body})
(RUN / "jev_inputs.json").write_text(json.dumps(items)); print(len(items))
