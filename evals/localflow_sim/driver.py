"""LocalFlow full-feature sim driver (see PROTOCOL.md).

Parent mode runs every cell of a suite, each in a fresh subprocess with its own
HOME / XDG / LOCALFLOW_CONFIG, so no module-level state leaks between cells:

    python evals/localflow_sim/driver.py --suite <suite|all> --out-dir <run>/rK/results
    python evals/localflow_sim/driver.py --suite meetings --scenario design-standup-nonotes --out-dir ...
    python evals/localflow_sim/driver.py --suite dictation --list

One JSON result per cell is written to <out-dir>/<cell_id>.json. API keys are
passed to the SUT through the process environment only and are scrubbed from
every file this driver writes.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import math
import os
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import tomllib
import traceback
import wave
from datetime import datetime, timedelta
from pathlib import Path

HERE = Path(__file__).resolve().parent
SUT = Path(os.environ.get("SIM_SUT", str(Path.home() / "work/sim-sut"))).expanduser()
SPEC = json.loads((HERE / "scenarios.json").read_text(encoding="utf-8"))
SUITES = SPEC["suites"]
LLM = SPEC["llm"]
SUITE_ORDER = ("dictation", "meetings", "recall_ask", "mcp", "settings_api")
SECRET_ENV = ("FIREWORKS_API_KEY", "JEV_API_Key", "LOCALFLOW_LLM_API_KEY", "LOCALFLOW_FIREWORKS_API_KEY", "TYPESAFE_API_KEY")
DEFAULT_TIMEOUT = {"meetings": 1800}
RATE = 16000
LINE_SECONDS = 5


def meeting_scenarios() -> list[dict]:
    path = (HERE / SUITES["meetings"]["meeting_scenarios"]).resolve()
    return json.loads(path.read_text(encoding="utf-8"))["scenarios"]


def suite_cells(suite: str) -> list[dict]:
    """Every cell of a suite as {"id", "kind", ...}; meeting scenarios come first."""
    cells = []
    if suite == "meetings":
        cells += [{"id": s["id"], "kind": "meeting", "gate": "M1"} for s in meeting_scenarios()]
    cells += SUITES[suite]["cells"]
    return cells


def scrub(text: str) -> str:
    for name in SECRET_ENV:
        value = os.environ.get(name) or ""
        if len(value) >= 8:
            text = text.replace(value, "[REDACTED]")
    return text


def now_stamp() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%S")


# ------------------------------------------------------------------ parent ---

def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(SUT), *args], text=True).strip()


def config_toml(tmp: Path, cell: dict) -> str:
    history = "true" if cell.get("history_enabled", True) else "false"
    work_prompts = "true" if cell.get("kind") == "workprompt" else "false"
    return f'''config_version = 2
[llm]
provider = "{LLM["provider"]}"
base_url = "{LLM["base_url"]}"
model = "{LLM["model"]}"
[hotkey]
sounds = false
[features]
meetings = true
[history]
enabled = {history}
[meetings]
vault_path = "{tmp / "vault"}"
watch = false
chunk_seconds = 3600
work_prompts = {work_prompts}
fireworks_model = "{LLM["model"]}"
prompts_dir = "{tmp / "prompts"}"
capture_system_audio = true
templates_dir = "{tmp / "templates"}"
'''


def run_cell(suite: str, cell: dict, out_dir: Path, commit: str, dirty: bool,
             timeout: int, keep_tmp: bool) -> dict:
    tmp = Path(tempfile.mkdtemp(prefix=f"lfsim-{cell['id']}-"))
    (tmp / "home").mkdir()
    (tmp / "config.toml").write_text(config_toml(tmp, cell), encoding="utf-8")
    env = {k: v for k, v in os.environ.items() if k != "PYTHONPATH"}
    env.update(
        HOME=str(tmp / "home"),
        XDG_CONFIG_HOME=str(tmp / "xdg"),
        XDG_DATA_HOME=str(tmp / "xdgdata"),
        XDG_STATE_HOME=str(tmp / "xdgstate"),
        XDG_CACHE_HOME=str(tmp / "xdgcache"),
        LOCALFLOW_CONFIG=str(tmp / "config.toml"),
        SIM_SUT=str(SUT.resolve()),
        LFSIM_TMP=str(tmp),
        LFSIM_COMMIT=commit,
        LFSIM_DIRTY="1" if dirty else "0",
        # Never touch the operator's real keyring; secrets come from the env only.
        PYTHON_KEYRING_BACKEND="keyring.backends.fail.Keyring",
        PYTHONDONTWRITEBYTECODE="1",
    )
    key = os.environ.get("FIREWORKS_API_KEY", "")
    if key:
        env["LOCALFLOW_LLM_API_KEY"] = key  # read by localflow.secrets.get_secret("llm_api_key")
    out = out_dir / f"{cell['id']}.json"
    out.unlink(missing_ok=True)
    cmd = [sys.executable, str(Path(__file__).resolve()), "--child", "--suite", suite,
           "--scenario", cell["id"], "--out-dir", str(out_dir)]
    started = time.monotonic()
    status, tail = "ok", ""
    try:
        proc = subprocess.run(cmd, env=env, cwd=str(tmp), capture_output=True, text=True,
                              timeout=timeout, check=False)
        tail = (proc.stdout[-2000:] + "\n" + proc.stderr[-6000:]).strip()
        if proc.returncode != 0:
            status = f"exit {proc.returncode}"
    except subprocess.TimeoutExpired as exc:
        status = f"timeout {timeout}s"
        tail = str(exc.stderr or "")[-6000:]
    if status == "ok" and not out.exists():
        status = "no result"
    if status == "ok" and "error" in json.loads(out.read_text(encoding="utf-8")):
        status = "cell error"
    logs = out_dir / "_logs"
    logs.mkdir(parents=True, exist_ok=True)
    (logs / f"{cell['id']}.log").write_text(scrub(tail) + "\n", encoding="utf-8")
    if not keep_tmp:
        shutil.rmtree(tmp, ignore_errors=True)
    return {"cell": cell["id"], "suite": suite, "status": status,
            "secs": round(time.monotonic() - started, 1)}


def parent(args: argparse.Namespace) -> int:
    suites = SUITE_ORDER if args.suite == "all" else (args.suite,)
    if args.list:
        for suite in suites:
            for cell in suite_cells(suite):
                print(cell["id"])
        return 0
    if not args.out_dir:
        raise SystemExit("--out-dir is required")
    out_dir = Path(args.out_dir).expanduser().resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    commit = git("rev-parse", "HEAD")
    dirty = bool(git("status", "--porcelain", "--untracked-files=no"))
    failed = 0
    matched = 0
    for suite in suites:
        for cell in suite_cells(suite):
            if args.scenario and cell["id"] != args.scenario:
                continue
            matched += 1
            timeout = args.timeout or DEFAULT_TIMEOUT.get(suite, 900)
            line = run_cell(suite, cell, out_dir, commit, dirty, timeout, args.keep_tmp)
            failed += line["status"] != "ok"
            print(json.dumps(line), flush=True)
    if not matched:
        raise SystemExit(f"no cell {args.scenario!r} in suite {args.suite!r}")
    return 1 if failed else 0


# ------------------------------------------------------------------- child ---

TMP = Path(os.environ.get("LFSIM_TMP", tempfile.gettempdir()))


def boot() -> None:
    """Put the pinned SUT first on sys.path and prove that is what got imported."""
    for name in ("LOCALFLOW_CONFIG", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "LFSIM_TMP"):
        assert os.environ.get(name), f"{name} must be set before localflow is imported"
    assert Path(os.environ["HOME"]).resolve().is_relative_to(TMP.resolve()), "HOME is not isolated"
    assert "localflow" not in sys.modules, "localflow imported before isolation"
    sys.path.insert(0, str(SUT))
    import localflow

    assert Path(localflow.__file__).resolve().is_relative_to(SUT.resolve()), localflow.__file__


def record_llm(llm, calls: list) -> None:
    """Wrap llm.complete so every real provider call is traced (never mocked)."""
    original = llm.complete

    def complete(system, user, *a, **k):
        started = time.monotonic()
        try:
            output = original(system, user, *a, **k)
        except Exception as exc:
            calls.append({"system": system, "prompt": user, "error": f"{type(exc).__name__}: {exc}",
                          "secs": round(time.monotonic() - started, 2)})
            raise
        calls.append({"system": system, "prompt": user, "output": output,
                      "secs": round(time.monotonic() - started, 2)})
        return output

    llm.complete = complete


def http_call(client, log: list, method: str, path: str, **kw):
    started = time.monotonic()
    response = client.request(method.upper(), path, **kw)
    try:
        body = response.json()
    except ValueError:
        body = response.text
    log.append({"call": f"{method.upper()} {path}", "status": response.status_code,
                "secs": round(time.monotonic() - started, 2)})
    return response.status_code, body


def test_client(server):
    from fastapi.testclient import TestClient

    return TestClient(server.app, base_url=f"http://127.0.0.1:{server._config.server_port}")


def wav_bytes(seconds: float = 1.0) -> bytes:
    frames = b"".join(
        struct.pack("<h", int(0.3 * 32767 * math.sin(2 * math.pi * 440 * i / RATE)))
        for i in range(int(RATE * seconds))
    )
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(RATE)
        handle.writeframes(frames)
    return buffer.getvalue()


# -- dictation ---------------------------------------------------------------

def cell_text(cell: dict) -> dict:
    """Cleanup, style, vocabulary and injection cells: the real Cleaner + live LLM."""
    from localflow.cleanup import Cleaner
    from localflow.config import load_config
    from localflow.dictionary import DictionaryStore, apply_replacements
    from localflow.formatting import apply_style
    from localflow.providers.factory import build_fast_llm, build_llm

    config = load_config()
    llm = build_llm(config)
    calls: list = []
    record_llm(llm, calls)
    cleaner = Cleaner(llm, build_fast_llm(config, builder=build_llm))
    text, words, trace = cell["text"], [], {"input": cell["text"]}
    if cell["kind"] == "vocab":
        store = DictionaryStore(TMP / "dictionary.json")
        for word in cell["words"]:
            store.add_word(word)
        for source, target in cell["replacements"]:
            store.add_replacement(source, target)
        words = list(store.words())
        text = apply_replacements(text, store.replacements())
        trace["dictionary"] = store.list()
        trace["replaced"] = text
    style = cell.get("style")
    cleaned = cleaner.clean(text, level=cell["level"], style=style, vocabulary=words)
    trace["cleaned"] = cleaned
    trace["output"] = apply_style(cleaned, style, words) if style else cleaned
    trace["llm_calls"] = calls
    return trace


def cell_formatting(cell: dict) -> dict:
    from localflow.formatting import apply_spoken_formatting

    return {"outputs": [{"text": item["text"], "output": apply_spoken_formatting(item["text"])}
                        for item in cell["inputs"]]}


def cell_hotphrase(cell: dict) -> dict:
    from localflow.formatting import apply_spoken_formatting, apply_style
    from localflow.hotphrases import (
        HotPhraseConflict,
        HotPhraseStore,
        expand_hot_phrases,
        render_dictation,
    )
    from localflow.symbols import apply_spoken_symbols

    store = HotPhraseStore(TMP / "hot_phrases.json")
    added = {}
    for phrase in cell["phrases"]:
        entry = store.add(phrase["trigger"], phrase["text"], phrase.get("enabled", True))
        added[phrase["trigger"]] = entry["id"]
    for trigger in cell.get("disable", []):
        store.update(added[trigger], enabled=False)
    trace: dict = {"entries": [{k: e[k] for k in ("trigger", "text", "enabled")} for e in store.list()]}
    conflict = cell.get("expect_conflict")
    if conflict:
        try:
            store.add(conflict["trigger"], conflict["text"])
            trace["conflict"] = "accepted"
        except HotPhraseConflict:
            trace["conflict"] = "HotPhraseConflict"
        trace["entries_after_conflict"] = len(store.list())
    phrases = store.enabled()
    trace["enabled"] = [list(pair) for pair in phrases]
    outputs = []
    for item in cell["inputs"]:
        if item["mode"] == "render":  # the pipeline the app runs, minus LLM cleanup
            output = render_dictation(
                item["text"], phrases,
                fmt=apply_spoken_formatting,
                symbols=lambda value: apply_spoken_symbols(value, []),
                style=lambda value: apply_style(value, "formal", []),
            )
        else:
            output = expand_hot_phrases(item["text"], phrases)
        outputs.append({"mode": item["mode"], "text": item["text"], "output": output})
    trace["outputs"] = outputs
    return trace


def cell_transcribe(cell: dict) -> dict:
    import numpy as np

    from localflow import server

    seen: list = []

    class ScriptTranscriber:
        def transcribe(self, audio, prompt=None):
            seen.append({"samples": len(audio), "prompt": prompt})
            return cell["raw"]

    server._get_transcriber = lambda: ScriptTranscriber()
    decode = "ffmpeg"
    if shutil.which("ffmpeg") is None:
        decode = "patched (ffmpeg not installed)"
        server._decode_audio = lambda data: np.full(RATE, 0.1, dtype=np.float32)
    calls: list = []
    record_llm(server._ollama, calls)
    http: list = []
    with test_client(server) as client:
        client.headers["X-Localflow-Token"] = server._api_token
        status, body = http_call(
            client, http, "post", f"/transcribe?clean={cell['clean']}",
            files={"file": ("sim.wav", wav_bytes(), "audio/wav")},
        )
    rows = server._get_history().recent(limit=5)
    keep = ("kind", "app_name", "category", "raw_text", "final_text", "status", "words", "duration_s")
    return {"status": status, "body": body, "decode": decode, "transcriber_calls": seen,
            "history": [{k: row.get(k) for k in keep} for row in rows],
            "history_path_isolated": server._get_history().path.resolve().is_relative_to(TMP.resolve()),
            "llm_calls": calls, "http": http}


# -- meetings ----------------------------------------------------------------

def cell_meeting(cell: dict) -> dict:
    """Same flow as evals/meeting_sim/driver.py, with the Fireworks provider."""
    import numpy as np

    from localflow import meetings, server

    sc = next(s for s in meeting_scenarios() if s["id"] == cell["id"])
    lines = sc["lines"]

    class FakeStream:
        def __init__(self, *a, **k):
            pass

        def start(self):
            pass

        def stop(self):
            pass

        def close(self):
            pass

    class ScriptTranscriber:
        def transcribe(self, audio, prompt=None):
            return lines[round((float(np.median(audio)) - 0.1) / 0.001)][1]

    class FakeSystemAudio:
        label = "sim-loopback"

        def start(self, callback):
            self.callback = callback

        def stop(self):
            pass

    def open_ok(*a, **k):
        return FakeSystemAudio()

    def open_fail(*a, **k):
        raise RuntimeError("sim: no loopback device")

    def tone(i):
        return np.full(RATE * LINE_SECONDS, 0.1 + i * 0.001, dtype=np.float32)

    meetings.sd.InputStream = FakeStream
    server._get_transcriber = lambda: ScriptTranscriber()
    server.open_system_audio = open_ok if sc["system_audio"] == "ok" else open_fail
    llm_calls: list = []
    record_llm(server._summarizer.llm, llm_calls)
    silence = np.zeros(RATE * LINE_SECONDS, dtype=np.float32)
    notes_final = sc["notes_final"] + sc.get("notes_final_extra", "")
    http: list = []
    res: dict = {"scenario": sc["id"], "notes_final": notes_final,
                 "provider": type(server._summarizer.llm).__name__}
    with test_client(server) as client:
        probe: list = []
        res["auth_probe"] = {
            "no_token": http_call(client, probe, "get", "/meeting/status")[0],
            "bad_token": http_call(client, probe, "get", "/meeting/status",
                                   headers={"X-Localflow-Token": "not-the-token"})[0],
        }
        client.headers["X-Localflow-Token"] = server._api_token
        res["templates"] = http_call(client, http, "get", "/meeting/templates")
        res["start"] = http_call(client, http, "post", "/meeting/start", json={
            "title": sc["title"], "template": sc["template"], "user_notes": sc["notes_initial"]})
        session = server._session
        assert session is not None, res["start"]
        for i, (src, _text) in enumerate(lines):
            if src == "me":
                session._mic.append(tone(i))
                session._others.append(silence)
            elif sc["system_audio"] == "ok":
                session._others.append(tone(i))
                session._mic.append(silence)
            else:
                session._mic.append(silence)
            session._drain_once()
            if i == len(lines) // 2:
                res["status_mid"] = http_call(client, http, "get", "/meeting/status")
                res["notes_save"] = http_call(client, http, "post", "/meeting/notes",
                                              json={"notes": notes_final})
        res["stop"] = http_call(client, http, "post", "/meeting/stop")
        stop = res["stop"][1] if isinstance(res["stop"][1], dict) else {}
        res["notes_text"] = Path(stop["notes_path"]).read_text(encoding="utf-8") if stop.get("notes_path") else ""
        res["log_text"] = Path(stop["log_path"]).read_text(encoding="utf-8") if stop.get("log_path") else ""
        res["summarize_calls"] = len(llm_calls)
        res["chats"] = []
        for q in sc["chat"]:
            status, body = http_call(client, http, "post", "/meeting/chat", json={"question": q["q"]})
            res["chats"].append({**q, "status": status,
                                 "answer": body.get("answer", "") if isinstance(body, dict) else str(body)})
        with server._session_lock:
            server._last_saved = None
        status, body = http_call(client, http, "post", "/meeting/chat", json={"question": sc["chat"][0]["q"]})
        res["restart"] = {"status": status, "log_path": body.get("log_path") if isinstance(body, dict) else None,
                          "expected": stop.get("log_path")}
    res["http"] = http
    res["llm_calls"] = llm_calls
    return res


def cell_workprompt(cell: dict) -> dict:
    from localflow import server
    from localflow.dispatch import (
        REJECT_FROM,
        StatusConflict,
        WorkPrompt,
        parse_work_prompts,
        write_prompts,
    )

    calls: list = []
    trace: dict = {"available": server._prompt_gen.available, "notes": cell["notes"]}
    if server._prompt_gen.llm is not None:
        record_llm(server._prompt_gen.llm, calls)
        trace["workprompt_model"] = server._prompt_gen.llm.model
    md = server._prompt_gen.generate(cell["notes"])
    trace["prompts_md"] = md
    trace["llm_calls"] = calls
    parsed = parse_work_prompts(md)
    trace["parsed"] = [{"title": p.title, "body": p.body} for p in parsed]
    trace["written"] = server._write_prompt_files(md, cell["title"], datetime.now()) if md else {}
    http: list = []
    steps: dict = {}
    with test_client(server) as client:
        client.headers["X-Localflow-Token"] = server._api_token
        steps["list_initial"] = http_call(client, http, "get", "/prompts")
        if cell["expect"] == "prompts":
            entries = steps["list_initial"][1].get("entries", []) if isinstance(steps["list_initial"][1], dict) else []
            trace["generated_entries"] = len(entries)
            if len(entries) < 2:
                # Queue transitions need two entries; top up with a labelled seed so W3 never
                # depends on how many prompts the model happened to emit.
                seeds = [WorkPrompt(f"Sim seeded prompt {i}", "Seeded by the sim harness for queue checks.")
                         for i in range(2 - len(entries))]
                paths = write_prompts(seeds, "Sim seed", datetime.now(), Path(server._config.prompts_dir))
                server._queue.add(seeds, paths, "Sim seed")
                steps["list_initial"] = http_call(client, http, "get", "/prompts")
                entries = steps["list_initial"][1]["entries"]
            trace["seeded"] = len(entries) - trace["generated_entries"]
            first, second = entries[0]["id"], entries[1]["id"]
            trace["files_exist"] = [Path(e["path"]).is_file() for e in entries]
            steps["list_pending"] = http_call(client, http, "get", "/prompts", params={"status": "pending"})
            steps["approve_first"] = http_call(client, http, "post", f"/prompts/{first}/approve")
            steps["approve_first_again"] = http_call(client, http, "post", f"/prompts/{first}/approve")
            steps["reject_approved"] = http_call(client, http, "post", f"/prompts/{first}/reject")
            steps["reject_second"] = http_call(client, http, "post", f"/prompts/{second}/reject")
            steps["reject_second_again"] = http_call(client, http, "post", f"/prompts/{second}/reject")
            steps["approve_rejected"] = http_call(client, http, "post", f"/prompts/{second}/approve")
            steps["approve_unknown"] = http_call(client, http, "post", "/prompts/00000000/approve")
            steps["list_approved"] = http_call(client, http, "get", "/prompts", params={"status": "approved"})
            steps["list_rejected"] = http_call(client, http, "get", "/prompts", params={"status": "rejected"})
            try:
                server._queue.set_status(first, "rejected", expect=REJECT_FROM)
                trace["direct_conflict"] = "no exception"
            except StatusConflict as exc:
                trace["direct_conflict"] = {"type": "StatusConflict", "current": exc.current,
                                            "expected": list(exc.expected)}
            trace["ids"] = {"first": first, "second": second}
            trace["queue_final"] = [{k: e.get(k) for k in ("id", "title", "status", "meeting")}
                                    for e in server._queue.list()]
        trace["queue_path_isolated"] = server._queue.path.resolve().is_relative_to(TMP.resolve())
    trace["steps"] = steps
    trace["http"] = http
    return trace


# -- recall & ask ------------------------------------------------------------

def seed_recall(server, now: datetime) -> dict:
    from localflow.meetings import ObsidianWriter, Segment

    corpus = SUITES["recall_ask"]["corpus"]
    config = server._config
    store = server._get_history()
    for i, item in enumerate(corpus["dictations"]):
        when = now - timedelta(days=item["age_days"], hours=2, minutes=i)
        store.add(kind="dictate", app_name=item["app"], category="other", duration_s=5,
                  raw_text=item["text"], final_text=item["text"], status="ok", ts=when.timestamp())
    writer = ObsidianWriter(config.vault_path, config.notes_folder, config.logs_folder)
    files = []
    for meeting in corpus["meetings"]:
        started = now - timedelta(days=meeting["age_days"], hours=3)
        segments = [Segment(i * 20.0, text, src) for i, (src, text) in enumerate(meeting["lines"])]
        saved = writer.write(title=meeting["title"], category="Other", started_at=started,
                             duration_seconds=len(segments) * 20, notes_md=meeting["notes_md"],
                             segments=segments, template="general")
        files += [saved.notes_path.name, saved.log_path.name]
    return {"dictations": len(corpus["dictations"]), "files": files}


def cell_recall(cell: dict) -> dict:
    from localflow import recall, server
    from localflow.ask_model import SOURCES, TIME_FILTERS, AskModel
    from localflow.providers.factory import build_llm

    now = datetime.now().replace(microsecond=0)
    config = server._config
    trace: dict = {"now": now.isoformat(), "seed": seed_recall(server, now), "no_answer": recall.NO_ANSWER}
    store = server._get_history()
    corpus_args = {"store": store, "vault_path": config.vault_path,
                   "notes_folder": config.notes_folder, "logs_folder": config.logs_folder}
    corpus = recall.build_corpus(**corpus_args)
    trace["corpus"] = {src: sum(p.source == src for p in corpus) for src in SOURCES}
    ask_calls: list = []
    api_calls: list = []
    ask_llm = build_llm(config, timeout=300, num_ctx=8192)  # what AskModel builds for itself
    record_llm(ask_llm, ask_calls)
    record_llm(server._summarizer.llm, api_calls)
    model = AskModel(llm=ask_llm, config=config, store=store)
    turns = cell["turns"] if cell["kind"] == "multi_turn" else [cell]
    time_filter = cell.get("time_filter", "Any time")
    sources = cell.get("sources")
    selected = set(sources or SOURCES)
    since = TIME_FILTERS[time_filter]
    floor = recall.parse_since(since, now)
    http: list = []
    out_turns = []
    with test_client(server) as client:
        client.headers["X-Localflow-Token"] = server._api_token
        for turn in turns:
            question = turn["q"]
            top = recall.rank(question, [p for p in corpus if p.source in selected], k=8, since=since, now=now)
            answer = model.ask(question, time_filter=time_filter, sources=sources, now=now)
            row: dict = {
                "q": question,
                "retrieved": [{"source": p.source, "title": p.title, "ts": p.ts, "text": p.text} for p in top],
                "stale_retrieved": sum(1 for p in top if floor is not None and (p.ts or 0) < floor.timestamp()),
                "ask": {"answer": answer.answer, "citations": answer.citations, "error": answer.error},
            }
            if sources is None:  # /api/recall has no source filter
                payload = {"question": question, "since": since, "k": 8}
                status, body = http_call(client, http, "post", "/api/recall", json={**payload, "llm": True})
                row["api"] = {"status": status, **(body if isinstance(body, dict) else {"body": str(body)})}
                status, body = http_call(client, http, "post", "/api/recall", json={**payload, "llm": False})
                row["api_search"] = {"status": status, **(body if isinstance(body, dict) else {"body": str(body)})}
            out_turns.append(row)
    trace.update(time_filter=time_filter, since=since, sources=sources, turns=out_turns,
                 model_turns=len(model.turns), llm_calls=ask_calls, api_llm_calls=api_calls, http=http)
    return trace


# -- mcp ---------------------------------------------------------------------

def tree_hash(root: Path, skip: str) -> dict:
    return {
        str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sorted(root.rglob("*"))
        if path.is_file() and skip not in path.relative_to(root).parts
    }


def read_line(fd: int, buffer: bytearray, timeout: float) -> bytes | None:
    deadline = time.monotonic() + timeout
    while b"\n" not in buffer:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
            return None
        chunk = os.read(fd, 65536)
        if not chunk:
            return None
        buffer += chunk
    line, _, rest = bytes(buffer).partition(b"\n")
    buffer[:] = rest
    return line


def drive_mcp(cmd: list[str], env: dict, requests: list[dict], label: str) -> dict:
    """Send each JSON-RPC line and read its reply (requests without an id get none)."""
    harness = TMP / "_harness"
    harness.mkdir(exist_ok=True)
    before = tree_hash(TMP, "_harness")
    exchanges = []
    with open(harness / f"{label}.stderr", "wb") as stderr:
        proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
                                env=env, cwd=str(TMP), bufsize=0)
        buffer = bytearray()
        try:
            for request in requests:
                proc.stdin.write(request["line"].encode("utf-8") + b"\n")
                proc.stdin.flush()
                raw = read_line(proc.stdout.fileno(), buffer, 20) if request["wants_reply"] else None
                text = raw.decode("utf-8", "replace") if raw is not None else None
                try:
                    reply = json.loads(text) if text else None
                except ValueError:
                    reply = {"unparseable": True}
                exchanges.append({"name": request["name"], "sent": request["line"], "raw": text, "reply": reply})
            proc.stdin.close()
            extra = b""
            line = read_line(proc.stdout.fileno(), buffer, 3)
            while line is not None:
                extra += line + b"\n"
                line = read_line(proc.stdout.fileno(), buffer, 1)
            code = proc.wait(timeout=10)
        finally:
            if proc.poll() is None:
                proc.kill()
    after = tree_hash(TMP, "_harness")
    stderr_tail = (harness / f"{label}.stderr").read_text(encoding="utf-8", errors="replace")[-1500:]
    return {"exchanges": exchanges, "extra_output": extra.decode("utf-8", "replace"), "exit_code": code,
            "hash_before": before, "hash_after": after, "stderr_tail": stderr_tail}


def cell_mcp(cell: dict) -> dict:
    from localflow.dispatch import QUEUE_PATH, PromptQueue, WorkPrompt, write_prompts
    from localflow.history import HistoryStore, history_path

    seed = SUITES["mcp"]["seed"]
    assert history_path().resolve().is_relative_to(TMP.resolve()), history_path()
    assert QUEUE_PATH.resolve().is_relative_to(TMP.resolve()), QUEUE_PATH
    now = time.time()
    store = HistoryStore()  # seeded through the product store, then closed (no open handles)
    for item in seed["dictations"]:
        store.add(kind=item["kind"], app_name=item["app"], category=item["category"], duration_s=4,
                  raw_text=item["raw"], final_text=item["final"], status=item["status"],
                  ts=now - item["age_hours"] * 3600)
    prompts = [WorkPrompt(p["title"], p["body"]) for p in seed["prompts"]]
    paths = write_prompts(prompts, seed["meeting"], datetime.now(), TMP / "prompts")
    queue = PromptQueue()
    ids = {}
    for prompt, path, item in zip(prompts, paths, seed["prompts"]):
        entry = queue.add([prompt], [path], seed["meeting"])[0]
        ids[prompt.title] = entry["id"]
        if item["status"] != "pending":
            queue.set_status(entry["id"], item["status"])
        time.sleep(0.02)  # distinct `created` stamps: newest-first order is deterministic
    requests = []
    for group in cell["groups"]:
        for spec in SUITES["mcp"]["groups"][group]:
            line = spec["raw"] if "raw" in spec else json.dumps(spec["req"])
            for title, entry_id in ids.items():
                line = line.replace("{{prompt:" + title + "}}", entry_id)
            requests.append({"name": spec["name"], "line": line,
                             "wants_reply": "raw" in spec or "id" in spec.get("req", {})})
    env = {**os.environ, "PYTHONPATH": str(SUT)}
    probe = subprocess.run([sys.executable, "-c", "import localflow; print(localflow.__file__)"],
                           env=env, cwd=str(TMP), capture_output=True, text=True, check=False)
    module_path = probe.stdout.strip()
    trace: dict = {
        "ids": ids,
        "db": str(history_path().relative_to(TMP)),
        "queue": str(QUEUE_PATH.relative_to(TMP)),
        "server_module_in_sut": bool(module_path) and Path(module_path).resolve().is_relative_to(SUT.resolve()),
        "python": drive_mcp([sys.executable, "-m", "localflow.mcp_server"], env, requests, "python"),
        "rust": None,
    }
    rust_bin = os.environ.get("MCP_RUST_BIN", "")
    if rust_bin and os.access(rust_bin, os.X_OK):
        trace["rust_bin"] = Path(rust_bin).name
        trace["rust"] = drive_mcp([rust_bin], env, requests, "rust")
    elif rust_bin:
        trace["rust_bin_error"] = "MCP_RUST_BIN is set but not an executable file"
    return trace


# -- settings & api ----------------------------------------------------------

def cell_auth(cell: dict) -> dict:
    from localflow import server

    def probe(client, spec: dict, headers: dict) -> dict:
        kw: dict = {"headers": headers}
        if "json" in spec:
            kw["json"] = spec["json"]
        if spec.get("upload"):
            kw["files"] = {"file": ("sim.wav", wav_bytes(0.2), "audio/wav")}
        response = client.request(spec["method"].upper(), spec["path"], **kw)
        return {"call": f"{spec['method'].upper()} {spec['path']}", "status": response.status_code,
                "leaks_config": "config_version" in response.text or "citations" in response.text}

    trace: dict = {}
    with test_client(server) as client:
        token = {"X-Localflow-Token": server._api_token}
        trace["unauthenticated"] = [probe(client, spec, {}) for spec in cell["unauthenticated"]]
        trace["wrong_token"] = [probe(client, spec, {"X-Localflow-Token": "A" * len(server._api_token)})
                                for spec in cell["wrong_token"]]
        trace["foreign_host"] = [probe(client, spec, {"Host": "evil.example"}) for spec in cell["foreign_host"]]
        trace["foreign_host_with_token"] = [probe(client, spec, {"Host": "evil.example", **token})
                                            for spec in cell["foreign_host"]]
        trace["public"] = [probe(client, spec, {}) for spec in cell["public"]]
        trace["authenticated"] = [probe(client, spec, token) for spec in cell["authenticated"]]
        config = client.get("/api/config", headers=token).json()
        status = client.get("/meeting/status", headers=token).json()
        trace["after_unauthenticated"] = {"cleanup_level": config["cleanup"]["level"],
                                          "session": status.get("session")}
    return trace


def cell_config_roundtrip(cell: dict) -> dict:
    from localflow import server

    path = Path(os.environ["LOCALFLOW_CONFIG"])
    http: list = []
    bodies: list = []

    def call(client, method: str, body=None):
        status, data = http_call(client, http, method, "/api/config", **({"json": body} if body is not None else {}))
        bodies.append(json.dumps(data, default=str))
        return [status, data]

    trace: dict = {}
    with test_client(server) as client:
        client.headers["X-Localflow-Token"] = server._api_token
        trace["before"] = call(client, "get")
        trace["put"] = call(client, "put", cell["update"])
        trace["after"] = call(client, "get")
        trace["disk_after"] = tomllib.loads(path.read_text(encoding="utf-8"))
        trace["invalid"] = []
        for item in cell["invalid"]:
            put = call(client, "put", item["body"])
            trace["invalid"].append({"name": item["name"], "status": put[0], "detail": put[1],
                                     "config_after": call(client, "get")[1]})
        trace["restart_put"] = call(client, "put", cell["restart_update"])
        trace["final"] = call(client, "get")
        trace["disk_final"] = tomllib.loads(path.read_text(encoding="utf-8"))
    key = os.environ.get("FIREWORKS_API_KEY", "")
    disk_text = path.read_text(encoding="utf-8")
    trace["secret_in_responses"] = bool(key) and any(key in body for body in bodies)
    trace["secret_on_disk"] = bool(key) and key in disk_text
    trace["http"] = http
    return trace


def cell_migrate(cell: dict) -> dict:
    from localflow import config as config_module
    from localflow.config_migrate import migrate_v1

    env_key_before = os.environ.get("FIREWORKS_API_KEY")
    cases = []
    for case in cell["cases"]:
        migrated, key = migrate_v1(dict(case["v1"]))
        loaded = config_module._config_from_v2(migrated)
        fake = case["v1"].get("fireworks_api_key")
        row: dict = {
            "name": case["name"], "migrated": migrated, "key": key,
            "key_in_migrated": bool(fake) and fake in json.dumps(migrated),
            "config_fields": {name: getattr(loaded, name) for name in case["config_fields"]},
        }
        if case.get("legacy_file"):
            legacy = TMP / f"legacy-{case['name']}.toml"
            legacy.write_text(
                "\n".join(f"{k} = {config_module._toml_value(v)}" for k, v in case["v1"].items()) + "\n",
                encoding="utf-8",
            )
            target = TMP / f"migrated-{case['name']}" / "config.toml"
            previous = os.environ["LOCALFLOW_CONFIG"]
            os.environ["LOCALFLOW_CONFIG"] = str(target)
            try:
                result = config_module.migrate_legacy(legacy)
            finally:
                os.environ["LOCALFLOW_CONFIG"] = previous
            text = target.read_text(encoding="utf-8") if target.exists() else ""
            row["legacy"] = {
                "returned_config": result is not None,
                "target_written": target.exists(),
                "key_in_target": bool(fake) and fake in text,
                "target": tomllib.loads(text) if text else None,
                "legacy_left_in_place": legacy.exists(),
                "env_key_unchanged": os.environ.get("FIREWORKS_API_KEY") == env_key_before,
            }
        cases.append(row)
    return {"cases": cases}


def _agent_parts(cell: dict):
    templates = list(cell["templates"])
    return cell["platform"], (lambda config: templates)


def cell_validator(cell: dict) -> dict:
    from localflow.config import Config
    from localflow.settings_agent import SettingError, Validator

    platform, templates = _agent_parts(cell)
    validator = Validator(platform, templates)
    results = []
    for case in cell["cases"]:
        try:
            results.append({"key": case["key"], "value": case["value"],
                            "ok": validator.value(case["key"], case["value"], Config())})
        except SettingError as exc:
            results.append({"key": case["key"], "value": case["value"], "error": str(exc)})
        except Exception as exc:  # noqa: BLE001 - anything but SettingError is itself a finding
            results.append({"key": case["key"], "value": case["value"],
                            "exception": f"{type(exc).__name__}: {exc}"})
    return {"results": results}


def cell_apply_undo(cell: dict) -> dict:
    from localflow import settings_agent as sa
    from localflow.config import load_config
    from localflow.dictionary import DictionaryStore

    platform, templates = _agent_parts(cell)
    agent_config = TMP / "agent" / "config.toml"
    dictionary = DictionaryStore(TMP / "agent" / "dictionary.json")
    agent = sa.SettingsAgent(config_path=agent_config, dictionary=dictionary,
                             undo_path=TMP / "agent" / "settings-undo.json",
                             platform=platform, templates=templates)

    def state() -> dict:
        config = agent.load()
        return {"cleanup_level": config.cleanup_level, "sounds_enabled": config.sounds_enabled,
                "hotkey": config.hotkey, "command_hotkey": config.command_hotkey,
                "server_port": config.server_port, "words": list(dictionary.words()),
                "replacements": [list(pair) for pair in dictionary.replacements()]}

    steps = []

    def step(name: str, action) -> None:
        row: dict = {"step": name}
        try:
            result = action()
            if isinstance(result, sa.ApplyResult):
                row["applied"] = len(result.applied)
                row["message"] = result.message
            elif isinstance(result, sa.UndoResult):
                row["reverted"] = list(result.reverted)
                row["skipped"] = list(result.skipped)
                row["message"] = result.message
            else:
                row["value"] = result
        except sa.SettingsAgentError as exc:
            row["error"] = f"SettingsAgentError: {exc}"
        except Exception as exc:  # noqa: BLE001 - recorded per step, judged by the gates
            row["exception"] = f"{type(exc).__name__}: {exc}"
        row["state"] = state()
        steps.append(row)

    forged = sa.Change(label="Cleanup", old="a", new="b", kind="setting", key="server_port",
                       before=8756, value=9000)
    step("initial", lambda: None)
    step("set_medium", lambda: agent.set_setting("cleanup_level", "medium"))
    step("undo_medium", agent.undo)
    step("undo_empty", agent.undo)
    step("apply_batch", lambda: agent.apply([
        sa.setting_change("sounds_enabled", True, False),
        sa.word_change("Qdrant"),
        sa.replacement_change("post gres", "PostgreSQL"),
    ]))
    step("pending_undo", agent.pending_undo)
    step("undo_batch", agent.undo)
    step("invalid_value", lambda: agent.set_setting("cleanup_level", "extreme"))
    step("not_allowlisted", lambda: agent.set_setting("server_port", "9000"))
    step("noop", lambda: agent.set_setting("cleanup_level", "light"))
    step("shortcut_conflict", lambda: agent.set_setting("command_hotkey", "alt_l"))
    step("forged_apply", lambda: agent.apply([forged]))
    live = load_config()  # the Fireworks openai-compatible config every other cell runs with
    return {
        "steps": steps,
        "ollama_guard": {
            "live_provider": live.llm_provider,
            "live_url_is_loopback": sa.is_loopback_url(live.llm_base_url),
            "agent_url_for_live_config": sa.local_ollama_url(live),
            "default_ollama_url": sa.DEFAULT_OLLAMA_URL,
        },
    }


def cell_change_warning(cell: dict) -> dict:
    from localflow import settings_agent as sa

    results = []
    for case in cell["cases"]:
        spec = case["change"]
        if spec["kind"] == "setting":
            change = sa.setting_change(spec["key"], spec["before"], spec["value"])
        elif spec["kind"] == "word":
            change = sa.word_change(spec["word"])
        elif spec["kind"] == "replacement":
            change = sa.replacement_change(spec["spoken"], spec["written"])
        elif spec["kind"] == "raw":
            change = sa.Change(label="Forged", old="a", new="b", kind="setting", key=spec["key"],
                               before=spec["before"], value=spec["value"])
        else:
            change = {"kind": "setting", "key": "sounds_enabled", "value": False}
        results.append({"name": case["name"], "request": case["request"],
                        "warning": sa.change_warning(change, case["request"]),
                        "line": sa.change_line(change),
                        "warnings_for": sa.warnings_for([change], case["request"])})
    return {"results": results}


RUNNERS = {
    "cleanup": cell_text, "style": cell_text, "vocab": cell_text, "injection": cell_text,
    "formatting": cell_formatting, "hotphrase": cell_hotphrase, "transcribe": cell_transcribe,
    "meeting": cell_meeting, "workprompt": cell_workprompt,
    "answerable": cell_recall, "unanswerable": cell_recall, "time_filter": cell_recall,
    "source_filter": cell_recall, "multi_turn": cell_recall,
    "mcp": cell_mcp,
    "auth": cell_auth, "config_roundtrip": cell_config_roundtrip, "migrate": cell_migrate,
    "validator": cell_validator, "apply_undo": cell_apply_undo, "change_warning": cell_change_warning,
}


def child(args: argparse.Namespace) -> int:
    cell = next(c for c in suite_cells(args.suite) if c["id"] == args.scenario)
    result: dict = {
        "cell": cell["id"], "suite": args.suite, "kind": cell["kind"],
        "commit": os.environ.get("LFSIM_COMMIT", ""), "sut_dirty": os.environ.get("LFSIM_DIRTY") == "1",
        "started": now_stamp(), "llm_provider": LLM["provider"], "llm_base_url": LLM["base_url"],
        "llm_model": LLM["model"], "spec_version": SPEC["version"],
    }
    try:
        boot()
        import localflow
        from localflow.config import load_config

        loaded = load_config()
        assert (loaded.llm_provider, loaded.llm_base_url, loaded.llm_model) == (
            LLM["provider"], LLM["base_url"], LLM["model"]), "config.toml is not the frozen LLM config"
        result["localflow_file_in_sut"] = Path(localflow.__file__).resolve().is_relative_to(SUT.resolve())
        result["llm_key_present"] = bool(os.environ.get("LOCALFLOW_LLM_API_KEY"))
        result.update(RUNNERS[cell["kind"]](cell))
    except BaseException as exc:  # noqa: BLE001 - a crashed cell is recorded, then gated as not exercised
        result["error"] = f"{type(exc).__name__}: {exc}"
        result["traceback"] = traceback.format_exc()[-4000:]
    result["finished"] = now_stamp()
    out = Path(args.out_dir) / f"{cell['id']}.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(scrub(json.dumps(result, indent=1, default=str)), encoding="utf-8")
    print(json.dumps({"cell": cell["id"], "error": result.get("error"),
                      "llm_calls": len(result.get("llm_calls", []))}))
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--suite", required=True, choices=[*SUITE_ORDER, "all"])
    ap.add_argument("--out-dir")
    ap.add_argument("--scenario", help="run a single cell id (smoke / repeatability checks)")
    ap.add_argument("--list", action="store_true", help="print the suite's cell ids and exit")
    ap.add_argument("--timeout", type=int, default=0, help="seconds per cell (default 900; meetings 1800)")
    ap.add_argument("--keep-tmp", action="store_true", help="keep each cell's temp dir for debugging")
    ap.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    args = ap.parse_args()
    return child(args) if args.child else parent(args)


if __name__ == "__main__":
    sys.exit(main())
