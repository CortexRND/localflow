"""One scenario, one repeat, against the real LocalFlow FastAPI app (see PLAN.md)."""
import argparse, json, os, subprocess, sys, tempfile, time
from pathlib import Path
import numpy as np

SUT = Path(os.environ.get("SIM_SUT", str(Path.home() / "work/sim-sut")))
ap = argparse.ArgumentParser()
ap.add_argument("--scenario", required=True)
ap.add_argument("--out", required=True)
args = ap.parse_args()
sc = next(s for s in json.load(open(Path(__file__).with_name("scenarios.json")))["scenarios"] if s["id"] == args.scenario)

tmp = Path(tempfile.mkdtemp(prefix=f"lfsim-{sc['id']}-"))
cfg = tmp / "config.toml"
cfg.write_text(f'''[llm]
provider = "ollama"
base_url = "http://127.0.0.1:11434"
model = "llama3.2:3b"
[hotkey]
sounds = false
[features]
meetings = true
[meetings]
vault_path = "{tmp / 'vault'}"
watch = false
chunk_seconds = 3600
work_prompts = false
capture_system_audio = true
templates_dir = "{tmp / 'templates'}"
''')
os.environ.update(LOCALFLOW_CONFIG=str(cfg), XDG_CONFIG_HOME=str(tmp / "xdg"), XDG_DATA_HOME=str(tmp / "xdgdata"))
sys.path.insert(0, str(SUT))

import localflow  # noqa: E402
assert Path(localflow.__file__).resolve().is_relative_to(SUT.resolve()), localflow.__file__
import localflow.meetings as meetings  # noqa: E402
import localflow.server as server  # noqa: E402
from fastapi.testclient import TestClient  # noqa: E402

RATE = 16000
LINE_SECONDS = 5
lines = sc["lines"]


class FakeStream:
    def __init__(self, *a, **k): pass
    def start(self): pass
    def stop(self): pass
    def close(self): pass


class ScriptTranscriber:
    def transcribe(self, audio):
        idx = round((float(np.median(audio)) - 0.1) / 0.001)
        return lines[idx][1]


class FakeSystemAudio:
    label = "sim-loopback"
    def start(self, callback): self.callback = callback
    def stop(self): pass


def open_ok(*a, **k): return FakeSystemAudio()
def open_fail(*a, **k): raise RuntimeError("sim: no loopback device")


meetings.sd.InputStream = FakeStream
server._get_transcriber = lambda: ScriptTranscriber()
server.open_system_audio = open_ok if sc["system_audio"] == "ok" else open_fail

llm_calls = []
_orig_complete = server._summarizer.llm.complete
def recording_complete(system, prompt, *a, **k):
    t = time.monotonic()
    out = _orig_complete(system, prompt, *a, **k)
    llm_calls.append({"prompt": prompt, "output": out, "secs": round(time.monotonic() - t, 2)})
    return out
server._summarizer.llm.complete = recording_complete

http = []
def call(client, method, path, **kw):
    t = time.monotonic()
    r = getattr(client, method)(path, **kw)
    try:
        body = r.json()
    except Exception:
        body = r.text
    http.append({"call": f"{method.upper()} {path}", "status": r.status_code, "secs": round(time.monotonic() - t, 2)})
    return r.status_code, body

tone = lambda i: np.full(RATE * LINE_SECONDS, 0.1 + i * 0.001, dtype=np.float32)
silence = np.zeros(RATE * LINE_SECONDS, dtype=np.float32)
notes_final = sc["notes_final"] + sc.get("notes_final_extra", "")
res = {"scenario": sc["id"], "commit": subprocess.check_output(["git", "-C", str(SUT), "rev-parse", "HEAD"], text=True).strip(),
       "started": time.strftime("%Y-%m-%dT%H:%M:%S"), "notes_final": notes_final}
with TestClient(server.app, base_url=f"http://127.0.0.1:{server._config.server_port}") as client:
    client.headers["X-Localflow-Token"] = server._api_token
    res["templates"] = call(client, "get", "/meeting/templates")
    res["start"] = call(client, "post", "/meeting/start", json={"title": sc["title"], "template": sc["template"], "user_notes": sc["notes_initial"]})
    session = server._session
    assert session is not None, res["start"]
    for i, (src, _text) in enumerate(lines):
        if src == "me":
            session._mic.append(tone(i)); session._others.append(silence)
        elif sc["system_audio"] == "ok":
            session._others.append(tone(i)); session._mic.append(silence)
        else:
            session._mic.append(silence)
        session._drain_once()
        if i == len(lines) // 2:
            res["status_mid"] = call(client, "get", "/meeting/status")
            res["notes_save"] = call(client, "post", "/meeting/notes", json={"notes": notes_final})
    res["stop"] = call(client, "post", "/meeting/stop")
    stop = res["stop"][1] if isinstance(res["stop"][1], dict) else {}
    res["notes_text"] = Path(stop["notes_path"]).read_text(encoding="utf-8") if stop.get("notes_path") else ""
    res["log_text"] = Path(stop["log_path"]).read_text(encoding="utf-8") if stop.get("log_path") else ""
    res["summarize_calls"] = len(llm_calls)
    res["chats"] = []
    for q in sc["chat"]:
        st, body = call(client, "post", "/meeting/chat", json={"question": q["q"]})
        res["chats"].append({**q, "status": st, "answer": body.get("answer", "") if isinstance(body, dict) else str(body)})
    with server._session_lock:
        server._last_saved = None
    st, body = call(client, "post", "/meeting/chat", json={"question": sc["chat"][0]["q"]})
    res["restart"] = {"status": st, "log_path": body.get("log_path") if isinstance(body, dict) else None,
                      "expected": stop.get("log_path")}
res["http"] = http
res["llm_calls"] = llm_calls
res["finished"] = time.strftime("%Y-%m-%dT%H:%M:%S")
Path(args.out).parent.mkdir(parents=True, exist_ok=True)
Path(args.out).write_text(json.dumps(res, indent=1))
print(json.dumps({"scenario": sc["id"], "stop": res["stop"][0], "llm_calls": len(llm_calls), "secs": sum(c["secs"] for c in llm_calls)}))
