"""Deterministic gates M1-M13 over ~/sim-runs/<run>/rK/results/*.json (thresholds frozen in PLAN.md)."""
import json, math, re, sys
from pathlib import Path
sys.path.insert(0, str(Path.home() / "work/sim-sut"))
from localflow.meetings import read_transcript_log  # noqa: E402
from localflow.meeting_templates import resolve_template  # noqa: E402

RUN = Path(sys.argv[1])
SC = {s["id"]: s for s in json.load(open(Path(__file__).with_name("scenarios.json")))["scenarios"]}
ABSTAIN = re.compile(r"(doesn'?t|does not|did not|didn'?t|isn'?t|wasn'?t|is not|was not|not)\s+(mention|mentioned|contain|say|said|discuss|discussed|specif|state|include|provide|cover)|no (mention|information|record|details?)|not (in|part of) the transcript|transcript (doesn'?t|does not)", re.I)
WORDS = {w: i for i, w in enumerate("zero one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty".split())}
WORDS.update({"thirty": 30, "forty": 40, "fifty": 50, "sixty": 60, "seventy": 70, "eighty": 80, "ninety": 90, "hundred": 100, "thousand": 1000})
THRESH = {"M5": 0.90, "M6": 0.70, "M7": 0.05, "M9": 0.80, "M10": 0.80}

def norm_head(h): return re.sub(r"[^a-z0-9 ]", "", h.replace("**", "").lower()).strip()
def headings(md): return [norm_head(m) for m in re.findall(r"^#{1,6}\s+(.+?)\s*$", md, re.M)]
def notes_md(text, title):
    body = text.split("\n---\n", 1)[1] if text.startswith("---\n") else text
    body = body.split("\nRaw transcript: [[", 1)[0]
    return re.sub(rf"^\s*# {re.escape(title)}\s*\n", "", body, count=1)
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
    g = {k: {"k": 0, "n": 0, "fail": []} for k in [f"M{i}" for i in range(1, 14)]}
    def tally(key, ok, item):
        g[key]["n"] += 1; g[key]["k"] += int(bool(ok))
        if not ok: g[key]["fail"].append(item)
    unsupported = []
    for f in files:
        r = json.loads(f.read_text()); s = SC[r["scenario"]]; sid = s["id"]
        stop = r["stop"][1] if isinstance(r["stop"][1], dict) else {}
        md = notes_md(r["notes_text"], s["title"]); low = md.lower(); nf = r["notes_final"].strip()
        tally("M1", all(200 <= h["status"] < 300 for h in r["http"]) and r["notes_text"] and r["log_text"]
              and stop.get("template") == s["template"] and stop.get("enhanced") == bool(nf), sid)
        body = r["log_text"].split("\n## My raw notes\n", 1)[0]
        got = re.findall(r"^\*\*\d\d:\d\d\*\* \*\*(Me|Others):\*\* (.*)$", body, re.M)
        if s["system_audio"] == "ok":
            exp = [("Me" if a == "me" else "Others", t) for a, t in s["lines"]]
            for i, e in enumerate(exp):
                tally("M2", i < len(got) and got[i] == e, f"{sid}#{i}")
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
        llm_in = "\n".join(c["prompt"] for c in r["llm_calls"][: r["summarize_calls"]])
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

repeats = sorted(p for p in RUN.glob("r*/results") if p.is_dir())
per = {p.parent.name: repeat_gates(sorted(p.glob("*.json"))) for p in repeats}
summary = {}
for key in [f"M{i}" for i in range(1, 14)]:
    vals = [per[r][key] for r in per]
    passes = [v["pass"] for v in vals if v["pass"] is not None]
    k = sum(v["k"] for v in vals); n = sum(v["n"] for v in vals)
    status = "not exercised" if not passes else "pass" if all(passes) and len(passes) == len(vals) else "fail" if not any(passes) else "flaky"
    xs = [v["value"] for v in vals if v["value"] is not None]
    summary[key] = {"status": status, "pass_count": f"{sum(passes)}/{len(vals)}", "min": min(xs) if xs else None, "max": max(xs) if xs else None,
                    "spread": round(max(xs) - min(xs), 3) if xs else None, "pooled_k": k, "pooled_n": n, "wilson95_pooled": wilson(k, n), "low_power": n < 10}
(RUN / "gates.json").write_text(json.dumps({"per_repeat": per, "summary": summary}, indent=1))
for k, v in summary.items():
    print(k, v["status"], v["pass_count"], v["min"], v["max"], f"n={v['pooled_n']}")
