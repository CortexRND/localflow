"""One scenario, one repeat, against the real LocalFlow FastAPI app (see PROTOCOL.md).

Paths and the LLM come from flags or env (flag wins):

  --sut        / SIM_SUT        LocalFlow checkout under test (default: this repo)
  --scenarios  (default: scenarios.json next to this file)
  --llm-url    / SIM_LLM_URL    Ollama base URL (default http://127.0.0.1:11434)
  --llm-model  / SIM_LLM_MODEL  Ollama model (default llama3.2:3b)

The result JSON keeps the original fields (``http``/``llm_calls`` with
``secs``) and adds millisecond timings: ``ms`` on every HTTP and LLM call,
plus a ``timings`` dict (``stop_to_notes_ms``, ``summarize_ms``,
``chat_ms``, ``restart_chat_ms``, ``llm_call_ms``, ``llm_calls_count``).
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_LLM_URL = "http://127.0.0.1:11434"
DEFAULT_LLM_MODEL = "llama3.2:3b"

ap = argparse.ArgumentParser()
ap.add_argument("--scenario", required=True)
ap.add_argument("--out", required=True)
ap.add_argument("--sut", default=os.environ.get("SIM_SUT") or str(REPO_ROOT), help="LocalFlow checkout under test (env SIM_SUT)")
ap.add_argument("--scenarios", default=str(Path(__file__).with_name("scenarios.json")), help="scenarios JSON file")
ap.add_argument("--llm-url", default=os.environ.get("SIM_LLM_URL") or DEFAULT_LLM_URL, help="Ollama base URL (env SIM_LLM_URL)")
ap.add_argument("--llm-model", default=os.environ.get("SIM_LLM_MODEL") or DEFAULT_LLM_MODEL, help="Ollama model (env SIM_LLM_MODEL)")
ap.add_argument("--cleanup", action="store_true", help="delete the temp config/vault dir afterwards (default: keep it, path recorded as 'tmp')")
args = ap.parse_args()
SUT = Path(args.sut).expanduser()
with open(args.scenarios, encoding="utf-8") as fh:
    sc = next((s for s in json.load(fh)["scenarios"] if s["id"] == args.scenario), None)
if sc is None:
    sys.exit(f"driver: scenario {args.scenario!r} not in {args.scenarios}")

import numpy as np

tmp = Path(tempfile.mkdtemp(prefix=f"lfsim-{sc['id']}-"))
cfg = tmp / "config.toml"
cfg.write_text(f'''[llm]
provider = "ollama"
base_url = {json.dumps(args.llm_url)}
model = {json.dumps(args.llm_model)}
[hotkey]
sounds = false
[features]
meetings = true
[meetings]
vault_path = {json.dumps((tmp / 'vault').as_posix())}
watch = false
chunk_seconds = 3600
work_prompts = false
capture_system_audio = true
templates_dir = {json.dumps((tmp / 'templates').as_posix())}
''')
os.environ.update(LOCALFLOW_CONFIG=str(cfg), XDG_CONFIG_HOME=str(tmp / "xdg"), XDG_DATA_HOME=str(tmp / "xdgdata"))
sys.path.insert(0, str(SUT))

import localflow

assert Path(localflow.__file__).resolve().is_relative_to(SUT.resolve()), localflow.__file__
from fastapi.testclient import TestClient

from localflow import meetings, server

RATE = 16000
LINE_SECONDS = 5
lines = sc["lines"]


def ms_since(t0):
    return round((time.perf_counter() - t0) * 1000, 3)


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
    t = time.perf_counter()
    try:
        out = _orig_complete(system, prompt, *a, **k)
    except Exception as exc:
        ms = ms_since(t)
        llm_calls.append({"prompt": prompt, "output": "", "secs": round(ms / 1000, 2), "ms": ms, "error": str(exc)})
        raise
    ms = ms_since(t)
    llm_calls.append({"prompt": prompt, "output": out, "secs": round(ms / 1000, 2), "ms": ms})
    return out
server._summarizer.llm.complete = recording_complete

http = []
def call(client, method, path, **kw):
    t = time.perf_counter()
    r = getattr(client, method)(path, **kw)
    ms = ms_since(t)
    try:
        body = r.json()
    except Exception:  # noqa: BLE001 - a non-JSON body is recorded as text
        body = r.text
    http.append({"call": f"{method.upper()} {path}", "status": r.status_code, "secs": round(ms / 1000, 2), "ms": ms})
    return r.status_code, body


def sut_commit():
    try:
        return subprocess.check_output(["git", "-C", str(SUT), "rev-parse", "HEAD"], text=True, stderr=subprocess.DEVNULL).strip()
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


tone = lambda i: np.full(RATE * LINE_SECONDS, 0.1 + i * 0.001, dtype=np.float32)
silence = np.zeros(RATE * LINE_SECONDS, dtype=np.float32)
notes_final = sc["notes_final"] + sc.get("notes_final_extra", "")
res = {"scenario": sc["id"], "commit": sut_commit(), "started": time.strftime("%Y-%m-%dT%H:%M:%S"),
       "notes_final": notes_final, "llm": {"url": args.llm_url, "model": args.llm_model}, "tmp": str(tmp)}
timings = {"stop_to_notes_ms": None, "summarize_ms": None, "chat_ms": [], "restart_chat_ms": None}
try:
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
        # POST /meeting/stop summarizes and writes the notes before returning.
        timings["summarize_ms"] = timings["stop_to_notes_ms"] = http[-1]["ms"]
        stop = res["stop"][1] if isinstance(res["stop"][1], dict) else {}
        res["notes_text"] = Path(stop["notes_path"]).read_text(encoding="utf-8") if stop.get("notes_path") else ""
        res["log_text"] = Path(stop["log_path"]).read_text(encoding="utf-8") if stop.get("log_path") else ""
        res["summarize_calls"] = len(llm_calls)
        res["chats"] = []
        for q in sc["chat"]:
            st, body = call(client, "post", "/meeting/chat", json={"question": q["q"]})
            timings["chat_ms"].append(http[-1]["ms"])
            res["chats"].append({**q, "status": st, "answer": body.get("answer", "") if isinstance(body, dict) else str(body)})
        with server._session_lock:
            server._last_saved = None
        st, body = call(client, "post", "/meeting/chat", json={"question": sc["chat"][0]["q"]})
        timings["restart_chat_ms"] = http[-1]["ms"]
        res["restart"] = {"status": st, "log_path": body.get("log_path") if isinstance(body, dict) else None,
                          "expected": stop.get("log_path")}
finally:
    if args.cleanup:
        shutil.rmtree(tmp, ignore_errors=True)
timings["llm_call_ms"] = [c["ms"] for c in llm_calls]
timings["llm_calls_count"] = len(llm_calls)
res["http"] = http
res["llm_calls"] = llm_calls
res["timings"] = timings
res["finished"] = time.strftime("%Y-%m-%dT%H:%M:%S")
Path(args.out).parent.mkdir(parents=True, exist_ok=True)
Path(args.out).write_text(json.dumps(res, indent=1))
print(json.dumps({"scenario": sc["id"], "stop": res["stop"][0], "llm_calls": len(llm_calls),
                  "secs": sum(c["secs"] for c in llm_calls), "stop_to_notes_ms": timings["stop_to_notes_ms"]}))
