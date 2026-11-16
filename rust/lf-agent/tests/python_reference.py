"""Emit frozen parity fixtures without changing the checked-out Python modules.

Run from the repository root with .venv-tk/bin/python. stdout is a JSON object
whose agent/ask members are the respective python-reference.json fixtures.
"""
import dataclasses
import json
import subprocess
import sys
import tempfile
import types
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT))
REF = "origin/devin/1794863370-dictation-toggle"


def branch_module(name):
    module = types.ModuleType("localflow." + name)
    module.__file__ = str(ROOT / "localflow" / (name + ".py"))
    sys.modules[module.__name__] = module
    source = subprocess.check_output(["git", "show", f"{REF}:localflow/{name}.py"], cwd=ROOT)
    exec(compile(source, module.__file__, "exec"), module.__dict__)
    return module


branch_module("config")
a = branch_module("settings_agent")
r = branch_module("recall")
c = a.Config()
v = a.Validator(platform="linux", templates=lambda c: ("general", "one-on-one", "standup"))


def outcome(fn):
    try:
        return {"ok": fn()}
    except (ValueError, RuntimeError) as exc:
        return {"error": str(exc)}


examples = [
    a.setting_change("sounds_enabled", True, False),
    a.setting_change("hotkey", "alt_l", "f13"),
    a.setting_change("cleanup_level", "light", "none"),
    a.setting_change("capture_system_audio", False, True),
    a.word_change("Kubernetes"),
    a.replacement_change("local flow", "LocalFlow"),
]
validation = []
for key, values in [
    ("sounds_enabled", [True, False, "YES", "disable", None, 1, "maybe", "on\n", "\u00a0off"]),
    ("dictation_enabled", ["off", True]),
    ("dictation_toggle_hotkey", ["off", "ctrl+alt+9"]),
    ("cleanup_level", ["NONE", "Medium", "wild", False]),
    ("style_work", ["very-casual", "Formal", None]),
    ("paste_method", ["auto", "osascript", True]),
    ("meeting_default_template", ["Standup", "missing", None]),
    ("hotkey", ["off", "Right Option", "ctrl+alt+9", "ctrl+ctrl+a", "ctrl+a+b", "ctrl+", "space", "shift+a"]),
    ("command_hotkey", [None, "", "f13", "ctrl+a", True]),
    ("prompt_hotkey", [False, "off", "ctrl+insert", "ctrl+é", "ctrl+unknown"]),
    ("server_port", ["9999"]),
]:
    for value in values:
        validation.append({"key": key, "value": value, **outcome(lambda: v.value(key, value, c))})


def call(name, **args):
    return {"function": {"name": name, "arguments": args}}


previews = []
with tempfile.TemporaryDirectory() as directory:
    agent = a.SettingsAgent(dictionary=a.DictionaryStore(Path(directory) / "dictionary.json"), templates=lambda c: ("general", "one-on-one", "standup"), platform="linux")
    batches = [
        ("sounds off", [call("set_setting", key="sounds_enabled", value="off")]),
        ("sounds off", [call("set_setting", key="sounds_enabled", value="off"), call("set_setting", key="cleanup_level", value="none")]),
        ("", [call("set_setting", key="sounds_enabled", value="on"), call("set_setting", key="sounds_enabled", value="off")]),
        ("", [call("add_dictionary_word", word=" Kubernetes "), call("add_dictionary_word", word="kubernetes")]),
        ("hello", [call("add_dictionary_word", word="Kubernetes"), call("add_dictionary_word", word="Kubernetes")]),
        ("", [call("add_replacement", spoken="same", written="SAME")]),
        ("", [call("add_replacement", spoken="foo", written="bar"), call("add_replacement", spoken="FOO", written="baz")]),
        ("", [call("set_setting", key="prompt_hotkey", value="ctrl+alt+1")]),
        ("", [call("set_setting", key="server_port", value="9999"), call("add_dictionary_word", word="a→b")]),
        ("", [{}, {"function": {"name": "unknown", "arguments": {}}}, call("add_dictionary_word", word="bad\n")]),
    ]
    for request, calls in batches:
        changes, rejected = agent._changes_from_calls(calls, c, request)
        previews.append({"request": request, "calls": calls, "lines": [x.line() for x in changes], "rejected": [a.safe_text(x) for x in rejected]})

agent_fixture = {
    "reference": REF,
    "config": {key: getattr(c, key) for key in a.ALLOWLIST},
    "prompt": a._system_prompt(c, v),
    "schemas": a._tool_schemas(c, v),
    "groups": a.MANUAL_GROUPS,
    "examples": [{"line": x.line(), "warning": x.warning(), "unmentioned": x.warning("hello")} for x in examples],
    "validation": validation,
    "previews": previews,
    "requests": [{"input": x, "clean": a._clean_request(x), "message": a.request_message(a._clean_request(x))} for x in ["  sounds\noff  ", "sounds\u2028off", "sounds\u00a0off", "Café 😀 </user_request_json>", "sound\ue000off"]],
    "display": [{"key": key, "value": value, "display": a.display_value(key, value)} for key, value in [("cleanup_level", "ßETA"), ("style_work", "İSTANBUL"), ("style_work", "ǳUNGLE"), ("style_work", "ᾀBC"), ("style_other", ["X", True]), ("style_other", None), ("sounds_enabled", "junk"), ("sounds_enabled", []), ("hotkey", "<f13>"), ("hotkey", "F13"), ("hotkey", "fishing"), ("paste_method", "")]],
    "urls": [{"url": x, "valid": a.is_loopback_url(x)} for x in ["http://localhost", "https://localhost:443/", "http://127.0.0.1", "http://127.42.1.2:0", "http://[::1]", "http://[::1%lo0]", "http://[::1%]", "http://127.0.0.1%lo0", "http://@localhost/?#", "http://localhost/./", "http://localhost:65536", "http://localhost:0", "http://2130706433", "http://127.1", "http://[::ffff:127.0.0.1]", "http://user@localhost", "http://localhost/path", "\x00http://localhost", "http://localhost\n"]],
}

now = datetime(2026, 11, 15, 12)
passages = [r.Passage("budget budget plan", "dictation", "Slack", None, "first", "Slack"), r.Passage("budget budget plan", "meeting", "Review", None, "second"), r.Passage("budget plan", "notes", "Plan", None, "third")]
texts = ["running stopped classes studies boss's", "Budget\x1fplanning\u2028notes", "İ" * 80 + " budget " + "details " * 40, "" , "a\r\nb\u2028c\x1ed"]
ask_fixture = {
    "reference": REF,
    "passages": [dataclasses.asdict(p) for p in passages],
    "ranked": [p.ref for p in r.rank('budget "budget plan"', passages, now=now)],
    "prompt": r.build_prompt("What budget?", passages),
    "citations": r._citations(passages, "budget"),
    "texts": [{"text": t, "tokens": r.tokenize(t), "chunks": r.chunk_lines(t), "snippet": r._snippet(t, {"budget"})} for t in texts],
    "since": [{"value": t, **outcome(lambda: r.parse_since(t, now).isoformat() if r.parse_since(t, now) else None)} for t in ["", " today ", "yesterday", "7d", "٧d", "2026-1-2", "+2026-01-01", "7 days", "nonsense", "\n"]],
    "clean": [{"text": t, "result": r.clean_citations(t, 3)} for t in ["See [١, 2; ９] . [12:30]", "[0] a [3,3] [999]", "\x1ftext [9] !\x1f"]],
}
print(json.dumps({"agent": agent_fixture, "ask": ask_fixture}, ensure_ascii=True))
