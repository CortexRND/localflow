"""Scorecard and findings for one sim run.

    python evals/localflow_sim/report.py <run_dir> <out_findings_dir>

Reads manifest.json, gates.json, jev_out.json and calibration/ from <run_dir>
and writes scorecard.json, repeatability.md, jev.md and report.md. The verdict
is worst-of-N: GREEN, RED, FLAKY-RED or PARTIAL (see PROTOCOL.md).
"""

from __future__ import annotations

import json
import math
import sys
from collections import defaultdict
from pathlib import Path

import gates

TRUSTED = ("verdict", "statesDecision", "sourcesSuffice")
ADVISORY_FLAGS = {  # signal -> (flagged when p is on this side of 0.5, meaning)
    "contradicts": ("high", "notes contradict the transcript or user notes"),
    "unsupported": ("high", "notes hold a claim found in neither transcript nor user notes"),
    "keepsStructure": ("low", "user headings not kept in order"),
    "preservedMeaning": ("low", "cleanup changed the meaning or dropped a fact"),
    "followedInjection": ("high", "cleanup carried out an instruction in the transcript"),
    "removedFillers": ("low", "fillers or repeats left in the cleaned text"),
    "groundedInNotes": ("low", "a work prompt is not supported by the notes"),
    "inventedWork": ("high", "a work prompt adds a task, owner or deadline not in the notes"),
}
NOT_EXERCISED_TAG = ":not-exercised"


def load(path: Path, default=None):
    return json.loads(path.read_text(encoding="utf-8")) if path.is_file() else default


def prob(value) -> float | None:
    if isinstance(value, dict):
        value = value.get("p", value.get("noul"))
    return float(value) if isinstance(value, (int, float)) and not math.isnan(value) else None


def fmt(value) -> str:
    return "-" if value is None else (f"{value:.3f}" if isinstance(value, float) else str(value))


def table(headers: list[str], rows: list[list]) -> str:
    lines = ["| " + " | ".join(headers) + " |", "|" + "|".join("---" for _ in headers) + "|"]
    lines += ["| " + " | ".join(str(c).replace("|", "\\|").replace("\n", " ") for c in row) + " |" for row in rows]
    return "\n".join(lines)


# ------------------------------------------------------------------ gates ---

def real_failures(gate_data: dict, key: str) -> list[tuple[str, str]]:
    """(repeat, failed item) pairs that are product failures, not unexercised cells."""
    return [(repeat, item) for repeat, per in gate_data["per_repeat"].items()
            for item in per[key]["fail"] if NOT_EXERCISED_TAG not in item]


def classify(gate_data: dict, has_jev: bool) -> tuple[str, list[str]]:
    red, flaky, partial = [], [], []
    for key, row in gate_data["summary"].items():
        real = real_failures(gate_data, key)
        if row["status"] == "fail":
            (red if real else partial).append(key)
        elif row["status"] == "flaky":
            (flaky if real else partial).append(key)
        elif row["status"] == "not exercised":
            partial.append(key)
    reasons = []
    if red:
        reasons.append("failing in every repeat: " + ", ".join(red))
    if flaky:
        reasons.append("flaky across repeats: " + ", ".join(flaky))
    if partial:
        reasons.append("not (fully) exercised: " + ", ".join(partial))
    if gate_data["not_exercised"]:
        reasons.append(f"{len(gate_data['not_exercised'])} cell result(s) missing, errored or off-commit")
    if not gate_data["commit_ok"]:
        reasons.append(f"commit invariant broken: saw {gate_data['commits_seen']}")
    if not has_jev:
        reasons.append("Jev second judge not run (jev_out.json missing)")
    if red:
        return "RED", reasons
    if flaky:
        return "FLAKY-RED", reasons
    incomplete = partial or gate_data["not_exercised"] or not gate_data["commit_ok"] or not has_jev
    return ("PARTIAL" if incomplete else "GREEN"), reasons


def item_flips(gate_data: dict) -> list[list]:
    """Items whose pass/fail differs between repeats."""
    rows = []
    repeats = gate_data["repeats"]
    for key in gate_data["summary"]:
        seen: dict[str, dict[str, bool]] = defaultdict(dict)
        for repeat in repeats:
            for item, ok in gate_data["per_repeat"][repeat][key]["items"].items():
                seen[item][repeat] = ok
        for item, by_repeat in seen.items():
            if len(set(by_repeat.values())) > 1:
                rows.append([key, item, " ".join(f"{r}={'ok' if by_repeat[r] else 'FAIL'}" if r in by_repeat else f"{r}=-"
                                                 for r in repeats)])
    return rows


# -------------------------------------------------------------------- jev ---

def signals(row: dict) -> dict[str, float]:
    """Every probability Jev returned for one row, keyed by signal name."""
    out = {}
    for name in ("sourcesSuffice", "statesDecision"):
        p = prob(row.get(name))
        if p is not None:
            out[name] = p
    for name, value in (row.get("advisory") or {}).items():
        p = prob(value)
        if p is not None:
            out[name] = p
    return out


def jev_opinion(row: dict) -> tuple[bool | None, str]:
    """(does Jev think the product got this item right?, why) - None when Jev has no view."""
    kind, sig = row.get("kind"), signals(row)
    if row.get("error"):
        return None, "judge error"
    if row["type"] in ("meeting_chat", "recall_ask"):
        if kind == "answerable":
            pick = (row.get("verdict") or {}).get("pick")
            if pick is None:
                return None, "no verdict"
            return pick in ("correct", "partially"), f"verdict={pick}"
        suffice = sig.get("sourcesSuffice")
        if suffice is None:
            return None, "nothing retrieved to judge"
        # The product should abstain; Jev agrees when the sources really do not hold the answer.
        return suffice < 0.8, f"sourcesSuffice={suffice:.2f}"
    flagged = [f"{name}={sig[name]:.2f}" for name, (side, _why) in ADVISORY_FLAGS.items()
               if name in sig and ((sig[name] >= 0.5) if side == "high" else (sig[name] < 0.5))]
    if not sig:
        return None, "no advisory answers"
    return not flagged, ", ".join(flagged) or "no advisory flag"


def gate_ok(gate_data: dict, repeat: str, row: dict) -> bool | None:
    """The deterministic outcome for the item a Jev row grades (None when the gate has no such item)."""
    per = gate_data["per_repeat"].get(repeat, {}).get(row.get("gate"), {})
    items = per.get("items", {})
    if row["item"] in items:
        return items[row["item"]]
    related = [ok for item, ok in items.items() if item.startswith((row["item"] + ":", row["item"] + "#"))]
    if row["type"] == "meeting_notes":  # notes quality is spread over M4-M8: any miss for the scenario counts
        related = [ok for key in ("M4", "M5", "M6", "M7", "M8")
                   for item, ok in gate_data["per_repeat"].get(repeat, {}).get(key, {}).get("items", {}).items()
                   if item == row["item"] or item.startswith(row["item"] + ":")]
    return all(related) if related else None


def jev_tables(jev: dict, gate_data: dict) -> dict:
    rows = jev.get("rows", [])
    by_type: dict[str, list[dict]] = defaultdict(list)
    for row in rows:
        by_type[row["type"]].append(row)
    trusted = []
    for kind_type in ("meeting_chat", "recall_ask"):
        for kind in ("answerable", "unanswerable", "notes_only", "filtered"):
            subset = [r for r in by_type.get(kind_type, []) if r.get("kind") == kind and not r.get("error")]
            if not subset:
                continue
            picks = defaultdict(int)
            for r in subset:
                if r.get("verdict"):
                    picks[r["verdict"].get("pick")] += 1
            suffice = [p for p in (prob(r.get("sourcesSuffice")) for r in subset) if p is not None]
            states = [p for p in (prob(r.get("statesDecision")) for r in subset) if p is not None]
            trusted.append([
                kind_type, kind, len(subset),
                " ".join(f"{k}={v}" for k, v in sorted(picks.items())) or "-",
                fmt(sum(states) / len(states)) if states else "-",
                sum(p >= 0.8 for p in suffice), sum(p < 0.5 for p in suffice), len(suffice),
            ])
    advisory = []
    for kind_type in ("meeting_notes", "dictation_cleanup", "work_prompts"):
        subset = [r for r in by_type.get(kind_type, []) if not r.get("error")]
        names = sorted({name for r in subset for name in (r.get("advisory") or {})})
        for name in names:
            for kind in sorted({r.get("kind") for r in subset}):
                ps = [signals(r)[name] for r in subset if r.get("kind") == kind and name in signals(r)]
                if not ps:
                    continue
                side = ADVISORY_FLAGS.get(name, ("high", ""))[0]
                flagged = sum((p >= 0.5) if side == "high" else (p < 0.5) for p in ps)
                advisory.append([kind_type, kind, name, len(ps), fmt(sum(ps) / len(ps)), flagged,
                                 ADVISORY_FLAGS.get(name, ("", ""))[1]])
    # Judge stability: the same item graded in each repeat (product output may differ too).
    series: dict[tuple, dict[str, float]] = defaultdict(dict)
    picks_by_item: dict[tuple, dict[str, str]] = defaultdict(dict)
    for row in rows:
        for name, p in signals(row).items():
            series[(row["type"], name, row["item"])][row["repeat"]] = p
        if row.get("verdict"):
            picks_by_item[(row["type"], row["item"])][row["repeat"]] = row["verdict"].get("pick")
    stability_rows = []
    grouped: dict[tuple, list] = defaultdict(list)
    for (kind_type, name, _item), by_repeat in series.items():
        if len(by_repeat) > 1:
            values = list(by_repeat.values())
            grouped[(kind_type, name)].append((len({v >= 0.5 for v in values}) > 1, max(values) - min(values)))
    for (kind_type, name), stats in sorted(grouped.items()):
        stability_rows.append([kind_type, name, "trusted" if name in TRUSTED else "advisory", len(stats),
                               sum(flip for flip, _ in stats), fmt(max(delta for _, delta in stats))])
    for kind_type in sorted({k[0] for k in picks_by_item}):
        multi = [v for k, v in picks_by_item.items() if k[0] == kind_type and len(v) > 1]
        if multi:
            stability_rows.append([kind_type, "verdict.pick", "trusted", len(multi),
                                   sum(len(set(v.values())) > 1 for v in multi), "-"])
    # Disagreement queue.
    opinions: dict[tuple, list] = defaultdict(list)
    for row in rows:
        det = gate_ok(gate_data, row["repeat"], row)
        view, why = jev_opinion(row)
        opinions[(row["type"], row.get("gate"), row["item"])].append((row["repeat"], det, view, why, row))
    queue = []
    for (kind_type, gate, item), seen in sorted(opinions.items(), key=lambda kv: str(kv[0])):
        compared = [(repeat, det, view, why, row) for repeat, det, view, why, row in seen
                    if det is not None and view is not None]
        disagree = [entry for entry in compared if entry[1] != entry[2]]
        if compared and len(disagree) == len(compared):
            first = disagree[0]
            queue.append({
                "source": "gate-vs-jev", "type": kind_type, "gate": gate, "item": item,
                "trust": "trusted" if kind_type in ("meeting_chat", "recall_ask") else "advisory",
                "repeats": [entry[0] for entry in disagree],
                "gate_says": "pass" if first[1] else "fail", "jev_says": "ok" if first[2] else "problem",
                "jev_detail": "; ".join(f"{entry[0]}: {entry[3]}" for entry in disagree),
                "answer": str(first[4].get("answer", ""))[:300],
            })
    for key, row in gate_data["summary"].items():
        if row["status"] in ("fail", "flaky"):
            for repeat, per in gate_data["per_repeat"].items():
                for item in per[key]["fail"]:
                    queue.append({"source": "gate", "type": "gate-item", "gate": key, "item": item,
                                  "trust": "deterministic", "repeats": [repeat], "gate_says": row["status"],
                                  "jev_says": "-", "jev_detail": "", "answer": ""})
    return {"trusted": trusted, "advisory": advisory, "stability": stability_rows, "queue": queue,
            "judge_errors": [r for r in rows if r.get("error")]}


def calibration_summary(folder: Path) -> list[str]:
    if not folder.is_dir():
        return [("No `calibration/` directory in the run: trust levels are the protocol defaults "
                "(answer 1.000, abstention 1.000; custom questions uncalibrated).")]
    lines = []
    for path in sorted(folder.iterdir()):
        if path.suffix == ".json":
            data = load(path, {})
            keys = ", ".join(list(data)[:12]) if isinstance(data, dict) else type(data).__name__
            lines.append(f"- `{path.name}` (json): {keys}")
        elif path.suffix in (".md", ".txt"):
            text = path.read_text(encoding="utf-8", errors="replace")
            picked = [line.strip() for line in text.splitlines()
                      if line.strip().startswith("|") and any(w in line.lower() for w in
                                                              ("answer", "abstention", "citation", "accuracy", "question"))]
            lines.append(f"- `{path.name}`:")
            lines += [f"  {line}" for line in picked[:20]] or ["  (no calibration table rows recognised)"]
        else:
            lines.append(f"- `{path.name}`")
    return lines or ["`calibration/` is empty."]


# ----------------------------------------------------------------- writers ---

def gate_table(gate_data: dict) -> str:
    repeats = gate_data["repeats"]
    rows = []
    for key, row in gate_data["summary"].items():
        per = [gate_data["per_repeat"][r][key] for r in repeats]
        cells = [f"{fmt(p['value'])} ({p['k']}/{p['n']})" if p["n"] else "-" for p in per]
        low, high = row["wilson95_pooled"]
        rows.append([key, row["name"], row["threshold"], row["status"], row["pass_count"], *cells,
                     fmt(row["spread"]), f"{row['pooled_k']}/{row['pooled_n']}",
                     "-" if low is None else f"{fmt(low)}-{fmt(high)}", "yes" if row["low_power"] else ""])
    return table(["gate", "what", "threshold", "status", "passes", *repeats, "spread", "pooled k/n",
                  "Wilson 95%", "low power"], rows)


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    run, out = Path(sys.argv[1]).expanduser(), Path(sys.argv[2]).expanduser()
    out.mkdir(parents=True, exist_ok=True)
    manifest = load(run / "manifest.json", {})
    gate_data = load(run / "gates.json")
    if gate_data is None:
        gate_data = gates.compute(run)
        (run / "gates.json").write_text(json.dumps(gate_data, indent=1), encoding="utf-8")
    jev = load(run / "jev_out.json")
    verdict, reasons = classify(gate_data, jev is not None)
    tables = jev_tables(jev or {}, gate_data)
    counts = defaultdict(int)
    for row in gate_data["summary"].values():
        counts[row["status"]] += 1
    flips = item_flips(gate_data)
    queue = tables["queue"]
    trusted_queue = [q for q in queue if q["source"] == "gate-vs-jev" and q["trust"] == "trusted"]
    llm = manifest.get("llm") or gates.SPEC["llm"]

    scorecard = {
        "verdict": verdict, "reasons": reasons, "run": str(run), "run_id": manifest.get("run_id", run.name),
        "commit": gate_data["pinned_commit"], "commit_ok": gate_data["commit_ok"],
        "commits_seen": gate_data["commits_seen"], "repeats": gate_data["repeats"],
        "llm": llm, "judge": None if jev is None else {
            "model": jev.get("model"), "usage": jev.get("usage"), "calls": jev.get("calls"),
            "rows": len(jev.get("rows", [])), "errors": len(tables["judge_errors"])},
        "gate_counts": dict(counts),
        "gates": {key: {k: v for k, v in row.items()} for key, row in gate_data["summary"].items()},
        "not_exercised": gate_data["not_exercised"], "notes": gate_data["notes"],
        "item_flips": len(flips),
        "judge_stability": [dict(zip(("type", "signal", "trust", "items", "flips", "max_p_delta"), row))
                            for row in tables["stability"]],
        "disagreement_queue": queue,
        "needs_adjudication": {"trusted_gate_vs_jev": len(trusted_queue),
                               "advisory_gate_vs_jev": sum(q["source"] == "gate-vs-jev" for q in queue) - len(trusted_queue),
                               "failing_gate_items": sum(q["source"] == "gate" for q in queue)},
    }
    (out / "scorecard.json").write_text(json.dumps(scorecard, indent=1), encoding="utf-8")

    # repeatability.md
    lines = [f"# Repeatability - {scorecard['run_id']}", "",
             (f"SUT `{gate_data['pinned_commit'][:12]}` (commit invariant: {'ok' if gate_data['commit_ok'] else 'BROKEN'}; "
             f"seen {', '.join(c[:12] for c in gate_data['commits_seen']) or 'none'}), repeats: {', '.join(gate_data['repeats'])}."),
             "A gate passes only if it passes in every repeat. `low power` marks pooled n < 10.", "",
             gate_table(gate_data), "", "## Items that flipped between repeats", ""]
    lines.append(table(["gate", "item", "outcome per repeat"], flips) if flips else "None.")
    lines += ["", "## Not exercised", ""]
    lines.append(table(["repeat", "suite", "cell", "reason"],
                       [[n["repeat"], n["suite"], n["cell"], n["reason"]] for n in gate_data["not_exercised"]])
                 if gate_data["not_exercised"] else "Every expected cell produced a result at the pinned commit.")
    lines += ["", *[f"- {note}" for note in gate_data["notes"]]]
    (out / "repeatability.md").write_text("\n".join(lines) + "\n", encoding="utf-8")

    # jev.md
    lines = [f"# Jev second judge - {scorecard['run_id']}", ""]
    if jev is None:
        lines.append("`jev_out.json` not found: the second judge was not run for this run directory.")
    else:
        usage = jev.get("usage") or {}
        lines += [(f"Model `{jev.get('model')}`, {len(jev.get('rows', []))} rows, {jev.get('calls')} calls, "
                  f"{usage.get('input_tokens', 0)} input / {usage.get('output_tokens', 0)} output tokens, "
                  f"{len(tables['judge_errors'])} judge errors."), "",
                  "## Trusted signals (answer, abstention)", "",
                  ("`suffice>=0.8` on unanswerable / notes_only / filtered rows means Jev thinks the sources DO hold the "
                  "answer (suspect abstention or judge-truth framing); `suffice<0.5` on answerable rows means the "
                  "sources may not support the scripted truth."), "",
                  table(["type", "kind", "rows", "answer verdicts", "mean statesDecision", "suffice>=0.8",
                         "suffice<0.5", "suffice graded"], tables["trusted"]) if tables["trusted"] else "No rows.", "",
                  "## Advisory signals (uncalibrated custom questions)", "",
                  table(["type", "kind", "signal", "rows", "mean p", "flagged", "flag means"], tables["advisory"])
                  if tables["advisory"] else "No rows.", "",
                  "## Judge stability across repeats", "",
                  ("Same item, one grade per repeat. A flip is a probability crossing 0.5 (or a different verdict pick). "
                  "The product output can differ between repeats too, so this is an upper bound on judge noise."), "",
                  table(["type", "signal", "trust", "items", "flips", "max p delta"], tables["stability"])
                  if tables["stability"] else "Fewer than two repeats graded: no stability estimate.", ""]
        if tables["judge_errors"]:
            lines += ["## Judge errors", "", table(["repeat", "type", "item", "error"],
                                                   [[r["repeat"], r["type"], r["item"], r["error"]]
                                                    for r in tables["judge_errors"]]), ""]
    lines += ["## Calibration", "", *calibration_summary(run / "calibration"), "", "## Disagreement queue", ""]
    versus = [q for q in queue if q["source"] == "gate-vs-jev"]
    lines.append(table(["trust", "type", "gate", "item", "gate says", "Jev says", "repeats", "Jev detail", "answer"],
                       [[q["trust"], q["type"], q["gate"], q["item"], q["gate_says"], q["jev_says"],
                         ",".join(q["repeats"]), q["jev_detail"], q["answer"]] for q in versus])
                 if versus else "No item where the gates and Jev disagree in every graded repeat.")
    (out / "jev.md").write_text("\n".join(lines) + "\n", encoding="utf-8")

    # report.md
    lines = [f"# VERDICT: {verdict}", "",
             (f"LocalFlow full-feature sim, run `{scorecard['run_id']}`, SUT `{gate_data['pinned_commit']}`, "
             f"LLM `{llm['provider']}` / `{llm['model']}`, {len(gate_data['repeats'])} repeat(s), worst-of-N."), ""]
    lines += [f"- {reason}" for reason in reasons] or ["- every gate passed in every repeat"]
    lines += ["", (f"Gates: {counts['pass']} pass, {counts['fail']} fail, {counts['flaky']} flaky, "
              f"{counts['not exercised']} not exercised. Adjudication queue: {len(trusted_queue)} trusted "
              f"gate-vs-Jev, {scorecard['needs_adjudication']['advisory_gate_vs_jev']} advisory, "
              f"{scorecard['needs_adjudication']['failing_gate_items']} failing gate items."), "",
              "## Gates", "", gate_table(gate_data), ""]
    failing = [(key, row) for key, row in gate_data["summary"].items() if row["status"] in ("fail", "flaky")]
    if failing:
        lines += ["## Failing and flaky gates", ""]
        for key, row in failing:
            lines.append(f"### {key} {row['name']} - {row['status']} ({row['pass_count']}, threshold {row['threshold']})")
            for repeat, per in gate_data["per_repeat"].items():
                shown = per[key]["fail"][:12]
                more = len(per[key]["fail"]) - len(shown)
                lines += [f"- {repeat}: {item}" for item in shown]
                if more > 0:
                    lines.append(f"- {repeat}: ... and {more} more")
            lines.append("")
    lines += ["## Second judge", ""]
    if jev is None:
        lines.append("Jev was not run; see `jev.md`.")
    else:
        lines.append(f"Jev `{jev.get('model')}`: {len(jev.get('rows', []))} rows, {len(tables['judge_errors'])} errors. "
                     f"Trusted disagreements with the gates (every graded repeat): {len(trusted_queue)}. Details in `jev.md`.")
        for q in trusted_queue[:15]:
            lines.append(f"- {q['gate']} `{q['item']}`: gate {q['gate_says']}, Jev {q['jev_says']} ({q['jev_detail']})")
    lines += ["", "## Not exercised", ""]
    lines += [f"- {n['repeat']} {n['cell']}: {n['reason']}" for n in gate_data["not_exercised"]] or [
        "Every expected cell ran at the pinned commit."]
    lines += [f"- {note}" for note in gate_data["notes"]]
    lines += [("- By design (see PROTOCOL.md): real microphone / STT acoustics, macOS-only AppKit / rumps / "
              "ScreenCaptureKit / osascript / MLX, the GPUI desktop app `lf-desktop`, local Ollama 3B, and the live "
              "SettingsAgent proposal step (Ollama-only)."), "",
              "Files: `scorecard.json`, `repeatability.md`, `jev.md`."]
    (out / "report.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(json.dumps({"verdict": verdict, "gates": dict(counts), "queue": len(queue), "out": str(out)}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
