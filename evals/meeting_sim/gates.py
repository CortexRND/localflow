"""Deterministic gates M1-M14 over <run>/rK/results/*.json (thresholds frozen in PROTOCOL.md).

Usage: gates.py RUN_DIR [--sut PATH] [--scenarios FILE] [--output|--json FILE]

``--sut`` (env ``SIM_SUT``, default: this repo) is the checkout whose
``localflow`` helpers (title sanitizer, templates) are imported. Output goes to
``RUN_DIR/gates.json`` unless ``--output``/``--json`` names another file. Besides
``per_repeat`` and ``summary`` the output carries ``timings``: millisecond
samples pooled over every result file that has the driver's ``timings`` dict.
"""
import argparse
import json
import math
import os
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SC = {}
_safe_title = resolve_template = None  # bound by load_sut() from the checkout under test
ABSTAIN = re.compile(r"(doesn'?t|does not|did not|didn'?t|isn'?t|wasn'?t|is not|was not|not)\s+(mention|mentioned|contain|say|said|discuss|discussed|specif|state|include|provide|cover)|no (mention|information|record|details?)|not (in|part of) the transcript|transcript (doesn'?t|does not)", re.IGNORECASE)
WORDS = {w: i for i, w in enumerate(["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen", "twenty"])}
WORDS.update({"thirty": 30, "forty": 40, "fifty": 50, "sixty": 60, "seventy": 70, "eighty": 80, "ninety": 90, "hundred": 100, "thousand": 1000})
THRESH = {"M5": 0.90, "M6": 0.70, "M7": 0.05, "M9": 0.80, "M10": 0.80}
GATES = [f"M{i}" for i in range(1, 15)]
LABEL_RE = re.compile(r"^\*\*\d\d:\d\d\*\* \*\*(Me|Others):\*\* (.*)$", re.MULTILINE)
DIAR_LABEL_RE = re.compile(r"^\*\*\d\d:\d\d\*\* \*\*(Me|Others|Speaker \d+):\*\* (.*)$", re.MULTILINE)


def expected_speaker_labels(s):
    """Line index -> "Speaker k" (first-appearance rank among remote speakers); {} if not diarized."""
    if not s.get("diarization_backend"):
        return {}
    rank, out = {}, {}
    for i, ((src, _), who) in enumerate(zip(s["lines"], s["line_speakers"])):
        if src == "others":
            out[i] = f"Speaker {rank.setdefault(who, len(rank) + 1)}"
    return out

def norm_head(h): return re.sub(r"[^a-z0-9 ]", "", h.replace("**", "").lower()).strip()
def headings(md): return [norm_head(m) for m in re.findall(r"^#{1,6}\s+(.+?)\s*$", md, re.MULTILINE)]
def notes_md(text, title):
    body = text.split("\n---\n", 1)[1] if text.startswith("---\n") else text
    body = body.split("\nRaw transcript: [[", 1)[0]
    return re.sub(rf"^\s*# {re.escape(_safe_title(title))}\s*\n", "", body, count=1)
def nums(text):
    out = set()
    for tok in re.findall(r"\d[\d,]*(?:\.\d+)?", text):
        tok = tok.rstrip(",").replace(",", "")
        out.add(str(float(tok)).rstrip("0").rstrip(".") if "." in tok else str(int(tok)))
    return out
def word_nums(text):
    out, words, cur = set(), re.findall(r"[a-z]+", text.lower()), None
    for w in words:
        if w in WORDS:
            v = WORDS[w]; out.add(str(v))
            if w in ("hundred", "thousand") and cur is not None:
                cur *= v; out.add(str(cur))
            else:
                cur = v
        else:
            cur = None
    return out
def wilson(k, n, z=1.96):
    if n == 0: return (None, None)
    p = k / n; d = 1 + z * z / n; c = p + z * z / (2 * n); r = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n))
    return (round((c - r) / d, 3), round((c + r) / d, 3))

def repeat_gates(files):
    g = {k: {"k": 0, "n": 0, "fail": []} for k in GATES}
    def tally(key, ok, item):
        g[key]["n"] += 1; g[key]["k"] += int(bool(ok))
        if not ok: g[key]["fail"].append(item)
    unsupported = []
    present = {json.loads(f.read_text())["scenario"] for f in files}
    for sid in sorted(set(SC) - present):
        tally("M1", False, f"{sid}:missing")
    for f in files:
        r = json.loads(f.read_text()); s = SC[r["scenario"]]; sid = s["id"]
        stop = r["stop"][1] if isinstance(r["stop"][1], dict) else {}
        md = notes_md(r["notes_text"], s["title"]); low = md.lower(); nf = r["notes_final"].strip()
        tally("M1", all(200 <= h["status"] < 300 for h in r["http"]) and r["notes_text"] and r["log_text"]
              and stop.get("template") == s["template"] and stop.get("enhanced") == bool(nf), sid)
        body = r["log_text"].split("\n## My raw notes\n", 1)[0]
        diarized = bool(s.get("diarization_backend"))
        got = (DIAR_LABEL_RE if diarized else LABEL_RE).findall(body)
        if s["system_audio"] == "ok":
            exp = [("Me" if a == "me" else "Others", t) for a, t in s["lines"]]
            # Diarized: compare channels, so any non-Me label (Speaker k) counts as Others.
            chan = [("Me" if lab == "Me" else "Others", t) for lab, t in got] if diarized else got
            for i, e in enumerate(exp):
                tally("M2", i < len(chan) and chan[i] == e, f"{sid}#{i}")
            for i in range(len(exp), len(chan)):
                tally("M2", False, f"{sid}#extra{i}")
            if diarized:
                labels = expected_speaker_labels(s)
                for i, label in labels.items():
                    tally("M14", i < len(got) and got[i] == (label, s["lines"][i][1]), f"{sid}#{i}")
                logged = sorted(str(x.get("label")) for x in stop.get("speakers") or [] if isinstance(x, dict))
                tally("M14", stop.get("diarization") == "ok" and logged == sorted(set(labels.values())),
                      f"{sid}:speakers")
        if nf:
            tally("M3", f"## My raw notes\n\n{nf}\n" in r["log_text"], sid)
            uh, nh, pos, ok = headings(nf), headings(md), -1, True
            for h in uh:
                idx = [j for j, x in enumerate(nh) if x == h and j > pos]
                if not idx: ok = False; break
                pos = idx[0]
            tally("M4", ok, sid)
        for p in s["user_points"]:
            tally("M5", any(k.lower() in low for k in p["keywords"]), f"{sid}:{p['keywords'][0]}")
        for fact in s["facts"]:
            tally("M6", any(k.lower() in low for k in fact["any"]), f"{sid}:{fact['id']}")
        # First summarize prompt only: it holds transcript, notes and template; a retry prompt embeds the model draft.
        llm_in = r["llm_calls"][0]["prompt"] if r["llm_calls"] else ""
        support = nums(llm_in) | word_nums(llm_in)
        for n in sorted(nums(md)):
            ok = n in support
            tally("M7", ok, f"{sid}:{n}")
            if not ok: unsupported.append(f"{sid}:{n}")
        if not nf:
            tpl = resolve_template(s["template"])
            nh = headings(md)
            tally("M8", all(norm_head(x) in nh for x in tpl.section_names()), sid)
        for c in r["chats"]:
            a = c["answer"]
            if c["kind"] == "answerable":
                tally("M9", c["status"] == 200 and any(k.lower() in a.lower() for k in c["any"]), f"{sid}:{c['q']}")
            elif c["kind"] == "unanswerable":
                tally("M10", c["status"] == 200 and bool(ABSTAIN.search(a)), f"{sid}:{c['q']}")
            else:
                tally("M11", c["status"] == 200 and bool(ABSTAIN.search(a)) and c["token"].lower() not in a.lower(), f"{sid}:{c['q']}")
        if s["system_audio"] == "fail":
            st = r["start"][1] if isinstance(r["start"][1], dict) else {}
            mid = json.dumps(r.get("status_mid", [0, {}])[1])
            me_lines = [t for a, t in s["lines"] if a == "me"]
            tally("M12", st.get("system_audio") == "error" and st.get("system_audio_error") and '"error"' in mid
                  and "**Others:**" not in r["log_text"] and all(t in r["log_text"] for t in me_lines), sid)
        rs = r["restart"]
        tally("M13", rs["status"] == 200 and rs["log_path"] == rs["expected"], sid)
    out = {}
    for key, v in g.items():
        rate = v["k"] / v["n"] if v["n"] else None
        if key == "M7":
            bad = v["n"] - v["k"]; rate = bad / v["n"] if v["n"] else 0.0
            passed = rate <= THRESH["M7"]
        elif key in THRESH:
            passed = rate is not None and rate >= THRESH[key]
        else:
            passed = v["n"] > 0 and v["k"] == v["n"]
        out[key] = {"value": None if rate is None else round(rate, 3), "k": v["k"], "n": v["n"], "pass": passed if v["n"] else None, "fail": v["fail"]}
    return out

def load_sut(sut):
    global _safe_title, resolve_template
    sys.path.insert(0, str(Path(sut).expanduser()))
    from localflow.meeting_templates import resolve_template
    from localflow.meetings import _safe_title


def load_scenarios(path):
    with open(path, encoding="utf-8") as fh:
        return {s["id"]: s for s in json.load(fh)["scenarios"]}


TIMING_KEYS = ("stop_to_notes_ms", "summarize_ms", "chat_ms", "restart_chat_ms", "llm_call_ms")


def pooled_timings(files):
    """Pool the driver's ``timings`` (ms) over result files; older files without them are counted, not used."""
    out = {k: [] for k in TIMING_KEYS}
    out["llm_calls_per_meeting"] = []
    used = skipped = 0
    for f in files:
        t = json.loads(f.read_text()).get("timings")
        if not isinstance(t, dict):
            skipped += 1
            continue
        used += 1
        for k in TIMING_KEYS:
            v = t.get(k)
            out[k].extend(x for x in (v if isinstance(v, list) else [v]) if isinstance(x, (int, float)))
        if isinstance(t.get("llm_calls_count"), int):
            out["llm_calls_per_meeting"].append(t["llm_calls_count"])
    out["meetings"], out["meetings_without_timings"] = used, skipped
    return out


def evaluate(run):
    repeats = sorted(p for p in run.glob("r*/results") if p.is_dir())
    per = {p.parent.name: repeat_gates(sorted(p.glob("*.json"))) for p in repeats}
    summary = {}
    for key in GATES:
        vals = [per[r][key] for r in per]
        passes = [v["pass"] for v in vals if v["pass"] is not None]
        k = sum(v["k"] for v in vals); n = sum(v["n"] for v in vals)
        status = "not exercised" if not passes else "pass" if all(passes) and len(passes) == len(vals) else "fail" if not any(passes) else "flaky"
        xs = [v["value"] for v in vals if v["value"] is not None]
        summary[key] = {"status": status, "pass_count": f"{sum(passes)}/{len(vals)}", "min": min(xs) if xs else None, "max": max(xs) if xs else None,
                        "spread": round(max(xs) - min(xs), 3) if xs else None, "pooled_k": k, "pooled_n": n, "wilson95_pooled": wilson(k, n), "low_power": n < 10}
    timings = pooled_timings(sorted(f for p in repeats for f in p.glob("*.json")))
    return {"per_repeat": per, "summary": summary, "timings": timings}


def main(argv=None):
    global SC
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("run", type=Path, help="run dir holding rK/results/*.json")
    ap.add_argument("--sut", default=os.environ.get("SIM_SUT") or str(REPO_ROOT), help="LocalFlow checkout to import helpers from (env SIM_SUT)")
    ap.add_argument("--scenarios", default=str(Path(__file__).with_name("scenarios.json")), help="scenarios JSON file")
    ap.add_argument("--output", "--json", dest="output", type=Path, help="gates JSON path (default RUN/gates.json)")
    args = ap.parse_args(argv)
    load_sut(args.sut)
    SC = load_scenarios(args.scenarios)
    result = evaluate(args.run)
    out = args.output or args.run / "gates.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(result, indent=1))
    for k, v in result["summary"].items():
        print(k, v["status"], v["pass_count"], v["min"], v["max"], f"n={v['pooled_n']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
