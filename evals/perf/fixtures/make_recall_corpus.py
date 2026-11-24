"""Deterministic, seeded recall corpus (``docs/plans/PLAN-perf-evals.md`` §5.4).

Stdlib only. Writes a corpus both recall implementations read identically::

    OUT/
      history.sqlite                  # dictations table, schema v1 (localflow/history.py,
                                      # rust/lf-core/src/history.rs)
      vault/Notes/*.md                # meeting notes (frontmatter) + standalone notes
      vault/Transcripts/*.md          # "YYYY-MM-DD HHMM Title (transcript).md"
      vault/Transcripts/Zoom/*.md     # a subfolder, to exercise the recursive walk
      queries.json                    # list of query strings / {"query", "since"} objects
      recall_corpus.json              # manifest: version, entries, seed, counts, digest

Determinism: the same ``(entries, seed)`` always yields the same SQLite rows,
the same markdown bytes and the same queries (``random.Random(seed)`` drawn in
a fixed order). Dictation ``ts`` values are whole epoch seconds counted back
from a fixed UTC anchor (so rows do not depend on the local time zone) and are
all at least a day before ``REFERENCE_NOW`` = ``2026-11-14T10:00:00`` in any
zone. Vault file stamps are naive local times, as the app writes them.

CLI::

    python -m evals.perf.fixtures.make_recall_corpus --out DIR --entries 1000 --seed 42
"""

from __future__ import annotations

import argparse
import calendar
import hashlib
import json
import os
import random
import shutil
import sqlite3
import sys
from datetime import datetime, timedelta
from pathlib import Path

GENERATOR_VERSION = 1
DEFAULT_ENTRIES = 1000
DEFAULT_SEED = 42
REFERENCE_NOW = "2026-11-14T10:00:00"
NOTES_FOLDER = "Notes"
LOGS_FOLDER = "Transcripts"
MANIFEST_NAME = "recall_corpus.json"
DB_NAME = "history.sqlite"
VAULT_NAME = "vault"
QUERIES_NAME = "queries.json"

_NOW = datetime.strptime(REFERENCE_NOW, "%Y-%m-%dT%H:%M:%S")
_ANCHOR_EPOCH = calendar.timegm(_NOW.timetuple())  # REFERENCE_NOW read as UTC
_HISTORY_SPAN_DAYS = 180
_VAULT_SPAN_DAYS = 120

# Same DDL as localflow/history.py HistoryStore._initialize (schema v1).
_SCHEMA_VERSION = 1
_DDL = (
    """
    CREATE TABLE IF NOT EXISTS dictations (
        id INTEGER PRIMARY KEY,
        ts REAL NOT NULL,
        kind TEXT NOT NULL CHECK(kind IN ('dictate', 'command', 'transform')),
        app_name TEXT,
        bundle_id TEXT,
        category TEXT,
        duration_s REAL NOT NULL,
        raw_text TEXT NOT NULL,
        final_text TEXT NOT NULL,
        words INTEGER NOT NULL,
        status TEXT NOT NULL CHECK(status IN ('ok', 'failed', 'silent')),
        audio_path TEXT
    )
    """,
    "CREATE INDEX IF NOT EXISTS dictations_ts ON dictations(ts DESC, id DESC)",
    """
    CREATE TABLE IF NOT EXISTS achievements (
        milestone INTEGER NOT NULL,
        day TEXT NOT NULL,
        PRIMARY KEY (milestone, day)
    )
    """,
)
COLUMNS = (
    "id", "ts", "kind", "app_name", "bundle_id", "category", "duration_s",
    "raw_text", "final_text", "words", "status", "audio_path",
)

# --------------------------------------------------------------------------
# Vocabulary
# --------------------------------------------------------------------------

APPS = (
    ("Slack", "com.tinyspeck.slackmacgap", "chat"),
    ("Mail", "com.apple.mail", "email"),
    ("Notes", "com.apple.Notes", "docs"),
    ("Visual Studio Code", "com.microsoft.VSCode", "code"),
    ("Google Chrome", "com.google.Chrome", "browser"),
    ("Messages", "com.apple.MobileSMS", "chat"),
    ("Notion", "notion.id", "docs"),
    ("Terminal", "com.apple.Terminal", "code"),
)
PEOPLE = ("Priya", "Marcus", "Elena", "Tomás", "Aiko", "Jordan", "Sam", "Fatima", "Noah", "Lena")
TEAMS = ("platform", "mobile", "growth", "design", "infra", "support")
PRODUCTS = ("Pro", "Team", "Starter", "Enterprise")
QUARTERS = ("Q1", "Q2", "Q3", "Q4")
WEEKDAYS = ("Monday", "Tuesday", "Wednesday", "Thursday", "Friday")
FEATURES = ("onboarding flow", "search page", "billing settings", "offline mode", "export dialog",
            "dark theme", "sync engine", "notification center")
CITIES = ("Lisbon", "Berlin", "Toronto", "Osaka", "Austin", "Nairobi")

SENTENCES = (
    "The launch pricing for the {product} tier should stay at {n2} dollars a month.",
    "Can we move the launch date for {product} to {weekday} so pricing is final?",
    "Our {quarter} marketing budget is {n3} thousand and we are already over by {n1} percent.",
    "{person} wants the budget review for the {team} team before {weekday}.",
    "We need to hire {n1} more engineers for the {team} team by the end of {quarter}.",
    "The hiring plan assumes two senior designers and one recruiter.",
    "Reminder to send {person} the {quarter} roadmap draft with the {feature} changes.",
    "The roadmap now puts {feature} ahead of the {product} migration.",
    "Customer feedback says the {feature} is confusing for new users.",
    "Churn went up {n1} percent after we changed the {product} plan.",
    "{person} reported a bug where the {feature} freezes on large files.",
    "The security incident postmortem is scheduled for {weekday} with {person}.",
    "Standup notes: {person} is blocked on the {feature} review, {person2} is unblocked.",
    "Book a dentist appointment for next {weekday} morning.",
    "Pick up groceries: oat milk, eggs, coffee beans and spinach.",
    "Flight to {city} lands at {n1} pm, then dinner at the café near the hotel.",
    "Let's keep the café menu idea for the offsite in {city}.",
    "Draft reply to {person}: thanks for the notes, I will follow up on {weekday}.",
    "Project Zephyr is still under wraps, do not mention it in the all hands.",
    "The quarterly business review deck needs the churn and budget slides.",
    "Ship the {feature} fix behind a flag and watch the error rate.",
    "Refactor the sync engine retry loop so it backs off after {n1} failures.",
    "Ask {person} whether the {team} team can own the {feature} rollout.",
    "The vendor quote came in at {n3} thousand, which blows the budget.",
)
#: Spoken-meeting phrasing that only appears in transcripts, so meeting
#: queries have hits that dictations do not drown out.
MEETING_LINES = (
    "Can everyone see my screen? This is the agenda for {weekday}.",
    "Let's circle back on the {feature} after the demo.",
    "Next slide please, this one shows the {quarter} numbers.",
    "I'll take that action item and send a recap to {person}.",
    "We are running out of time, let's park the {feature} discussion.",
    "Sorry, you were on mute, could you repeat the part about {feature}?",
)
COMMANDS = (
    "make this more formal",
    "summarize the last paragraph",
    "translate to Spanish",
    "turn this into bullet points",
)
MEETING_TITLES = (
    "Pricing sync", "Budget review", "Hiring plan", "Roadmap planning", "Weekly standup",
    "Customer feedback triage", "Security postmortem", "Design critique", "Quarterly business review",
    "Onboarding retro", "Launch go-no-go", "One on one",
)
PLATFORMS = ("zoom", "meet", "teams", "slack")


def _fill(rng: random.Random, template: str) -> str:
    person, person2 = rng.sample(PEOPLE, 2)
    return template.format(
        person=person,
        person2=person2,
        team=rng.choice(TEAMS),
        product=rng.choice(PRODUCTS),
        quarter=rng.choice(QUARTERS),
        weekday=rng.choice(WEEKDAYS),
        feature=rng.choice(FEATURES),
        city=rng.choice(CITIES),
        n1=rng.randint(2, 12),
        n2=rng.randint(9, 99),
        n3=rng.randint(20, 900),
    )


def _paragraph(rng: random.Random, sentences: int) -> str:
    return " ".join(_fill(rng, rng.choice(SENTENCES)) for _ in range(sentences))


def _raw(final_text: str) -> str:
    """A plausible raw STT transcript for ``final_text`` (lowercase, no punctuation)."""
    kept = "".join(ch for ch in final_text.lower() if ch.isalnum() or ch.isspace() or ch == "'")
    return " ".join(kept.split())


# --------------------------------------------------------------------------
# Dictations
# --------------------------------------------------------------------------


def _dictation_rows(rng: random.Random, entries: int) -> list[tuple]:
    # Distinct whole-second offsets in [1 day, span], oldest first -> ids grow with ts.
    low, high = 86_400 + 60, _HISTORY_SPAN_DAYS * 86_400
    offsets = sorted(rng.sample(range(low, high), entries), reverse=True)
    rows = []
    for index, offset in enumerate(offsets, start=1):
        ts = float(_ANCHOR_EPOCH - offset)
        roll = rng.random()
        kind = "dictate" if roll < 0.85 else "command" if roll < 0.95 else "transform"
        roll = rng.random()
        status = "ok" if roll < 0.92 else "failed" if roll < 0.96 else "silent"
        if rng.random() < 0.08:
            app_name = bundle_id = category = None
        else:
            app_name, bundle_id, category = rng.choice(APPS)
        if kind == "command":
            final_text = rng.choice(COMMANDS)
        elif rng.random() < 0.01:
            # Rare long dictation (> CHUNK_MAX_WORDS) so chunk splitting is exercised.
            final_text = _paragraph(rng, rng.randint(22, 30))
        else:
            final_text = _paragraph(rng, rng.randint(1, 4))
        raw_text = _raw(final_text)
        if status == "silent":
            raw_text = final_text = ""
        duration_s = round(max(0.5, len(raw_text.split()) / 2.5), 1)
        rows.append((
            index, ts, kind, app_name, bundle_id, category, duration_s,
            raw_text, final_text, len(final_text.split()), status, None,
        ))
    return rows


def _write_db(path: Path, rows: list[tuple]) -> None:
    tmp = path.with_name(path.name + ".tmp")
    if tmp.exists():
        tmp.unlink()
    connection = sqlite3.connect(tmp)
    try:
        for statement in _DDL:
            connection.execute(statement)
        connection.executemany(
            f"INSERT INTO dictations ({', '.join(COLUMNS)}) VALUES ({', '.join('?' * len(COLUMNS))})",
            rows,
        )
        connection.execute(f"PRAGMA user_version = {_SCHEMA_VERSION}")
        connection.commit()
    finally:
        connection.close()
    os.replace(tmp, path)


def read_rows(db_path: str | Path) -> list[tuple]:
    """All dictation rows in id order (for determinism checks)."""
    connection = sqlite3.connect(str(db_path))
    try:
        return connection.execute(
            f"SELECT {', '.join(COLUMNS)} FROM dictations ORDER BY id"
        ).fetchall()
    finally:
        connection.close()


# --------------------------------------------------------------------------
# Vault
# --------------------------------------------------------------------------


def _stamp(rng: random.Random, used: set[str], title: str) -> tuple[datetime, str]:
    """A naive local start time and a unique ``YYYY-MM-DD HHMM Title`` stem."""
    day = rng.randint(1, _VAULT_SPAN_DAYS)
    start = (_NOW - timedelta(days=day)).replace(
        hour=rng.randint(8, 18), minute=rng.choice((0, 15, 30, 45)), second=0
    )
    base = f"{start:%Y-%m-%d %H%M} {title}"
    stem, k = base, 2
    while stem in used:
        stem, k = f"{base} {k}", k + 1
    used.add(stem)
    return start, stem


def _frontmatter(title: str, start: datetime, platform: str, minutes: int) -> str:
    return (
        "---\n"
        f"title: {json.dumps(title, ensure_ascii=True)}\n"
        f"date: {start:%Y-%m-%dT%H:%M}\n"
        f"platform: {platform}\n"
        f"duration_minutes: {minutes}\n"
        "tags: [meeting, localflow]\n"
        "---\n\n"
    )


def _meeting(rng: random.Random, used: set[str]) -> tuple[str, str, str, str]:
    """(subfolder, stem, notes markdown, transcript markdown) for one meeting."""
    title = rng.choice(MEETING_TITLES)
    start, stem = _stamp(rng, used, title)
    platform = rng.choice(PLATFORMS)
    speakers = rng.sample(PEOPLE, rng.randint(2, 4))
    with_speakers = rng.random() < 0.8
    segments, seconds = [], 0
    for _ in range(rng.randint(12, 36)):
        seconds += rng.randint(4, 40)
        stamp = f"[{seconds // 60:02d}:{seconds % 60:02d}]"
        text = _paragraph(rng, rng.randint(1, 3))
        if rng.random() < 0.5:
            text = _fill(rng, rng.choice(MEETING_LINES)) + " " + text
        if with_speakers:
            segments.append(f"**{stamp}** **{rng.choice(speakers)}:** {text}")
        else:
            segments.append(f"**{stamp}** {text}")
    minutes = seconds // 60
    front = _frontmatter(title, start, platform, minutes)
    raw_notes = ""
    if rng.random() < 0.3:
        raw_notes = f"## My raw notes\n\n{_paragraph(rng, 2)}\n\n"
    transcript = (
        front
        + f"# {title} — transcript\n\n"
        + "\n\n".join(segments) + "\n\n"
        + raw_notes
        + f"Notes: [[{stem}]]\n"
    )
    actions = "\n".join(
        f"- [ ] {rng.choice(speakers)}: {_fill(rng, rng.choice(SENTENCES))}"
        for _ in range(rng.randint(1, 4))
    )
    decisions = "\n".join(f"- {_fill(rng, rng.choice(SENTENCES))}" for _ in range(rng.randint(1, 3)))
    notes = (
        front
        + f"# {title}\n\n"
        + f"## Summary\n\n{_paragraph(rng, rng.randint(2, 5))}\n\n"
        + f"## Decisions\n\n{decisions}\n\n"
        + f"## Action items\n\n{actions}\n\n"
        + f"Raw transcript: [[{stem} (transcript)]]\n"
    )
    subfolder = "Zoom" if platform == "zoom" else ""
    return subfolder, stem, notes, transcript


def _standalone_note(rng: random.Random, used: set[str]) -> tuple[str, str]:
    title = rng.choice(("Ideas", "Reading list", "Trip planning", "Weekly review", "Scratchpad"))
    _, stem = _stamp(rng, used, title)
    lines = [f"# {title}", ""]
    for _ in range(rng.randint(3, 10)):
        lines.append(f"- {_fill(rng, rng.choice(SENTENCES))}")
    return stem, "\n".join(lines) + "\n"


def _vault_files(rng: random.Random, entries: int) -> dict[str, str]:
    """Relative POSIX path (under ``vault/``) -> file text."""
    files: dict[str, str] = {}
    used: set[str] = set()
    for _ in range(max(2, entries // 50)):
        subfolder, stem, notes, transcript = _meeting(rng, used)
        files[f"{NOTES_FOLDER}/{stem}.md"] = notes
        folder = f"{LOGS_FOLDER}/{subfolder}" if subfolder else LOGS_FOLDER
        files[f"{folder}/{stem} (transcript).md"] = transcript
    for _ in range(max(1, entries // 100)):
        stem, text = _standalone_note(rng, used)
        files[f"{NOTES_FOLDER}/{stem}.md"] = text
    return files


# --------------------------------------------------------------------------
# Queries
# --------------------------------------------------------------------------

#: Fixed query set: multi-word, single-word, quoted phrase, people, unicode,
#: date-filtered (``since``), and queries that must return nothing.
QUERIES: tuple = (
    "launch pricing budget",
    "hiring plan for the platform team",
    "budget",
    "zephyr",
    '"quarterly business review"',
    "what did we decide about the onboarding flow",
    "action items for Priya",
    "dentist appointment",
    "customer churn feedback",
    "security incident postmortem",
    "café menu offsite",
    "sync engine retry failures",
    "can everyone see my screen agenda",
    "circle back after the demo",
    {"query": "budget", "since": "7d"},
    {"query": "roadmap", "since": "2026-10-01"},
    {"query": "standup blockers", "since": "30d"},
    {"query": "slide numbers", "since": "yesterday"},
    "xylophone quasar nebula",
    "the and of it",
)


def query_key(entry: str | dict) -> str:
    """Stable result key for one query entry (shared with both benches)."""
    if isinstance(entry, str):
        return entry
    since = entry.get("since")
    return f"{entry['query']} (since {since})" if since else entry["query"]


# --------------------------------------------------------------------------
# Public API
# --------------------------------------------------------------------------


def _digest(rows: list[tuple], files: dict[str, str], queries: list) -> str:
    h = hashlib.sha256()
    h.update(json.dumps(rows, ensure_ascii=False).encode("utf-8"))
    for rel in sorted(files):
        h.update(b"\0" + rel.encode("utf-8") + b"\0" + files[rel].encode("utf-8"))
    h.update(json.dumps(queries, ensure_ascii=False, sort_keys=True).encode("utf-8"))
    return h.hexdigest()


def _prepare_out(out: Path) -> None:
    """Create ``out`` or clear a previous corpus there; never wipe a foreign folder."""
    if out.exists():
        if not out.is_dir():
            raise ValueError(f"{out} exists and is not a directory")
        children = list(out.iterdir())
        if children:
            manifest_path = out / MANIFEST_NAME
            valid_manifest = False
            if manifest_path.is_file():
                try:
                    data = json.loads(manifest_path.read_text(encoding="utf-8"))
                    valid_manifest = isinstance(data, dict) and "generator_version" in data and "digest" in data
                except (OSError, ValueError):
                    valid_manifest = False
            if not valid_manifest:
                raise ValueError(f"{out} is not empty and holds no valid {MANIFEST_NAME}; refusing to overwrite")
        for name in (MANIFEST_NAME, DB_NAME, QUERIES_NAME):
            if (out / name).exists():
                (out / name).unlink()
        if (out / VAULT_NAME).exists():
            shutil.rmtree(out / VAULT_NAME)
    out.mkdir(parents=True, exist_ok=True)


def generate_corpus(out_dir: str | Path, *, entries: int = DEFAULT_ENTRIES, seed: int = DEFAULT_SEED) -> dict:
    """Write the corpus to ``out_dir`` and return its manifest (also saved as ``recall_corpus.json``)."""
    entries, seed = int(entries), int(seed)
    if entries <= 0:
        raise ValueError("entries must be positive")
    out = Path(out_dir)
    rng = random.Random(seed)
    rows = _dictation_rows(rng, entries)
    files = _vault_files(rng, entries)
    queries = list(QUERIES)
    _prepare_out(out)
    _write_db(out / DB_NAME, rows)
    vault = out / VAULT_NAME
    for rel, text in files.items():
        path = vault / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="\n") as fh:
            fh.write(text)
    (vault / NOTES_FOLDER).mkdir(parents=True, exist_ok=True)
    (vault / LOGS_FOLDER).mkdir(parents=True, exist_ok=True)
    (out / QUERIES_NAME).write_text(json.dumps(queries, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    manifest = {
        "generator_version": GENERATOR_VERSION,
        "entries": entries,
        "seed": seed,
        "now": REFERENCE_NOW,
        "notes_folder": NOTES_FOLDER,
        "logs_folder": LOGS_FOLDER,
        "db": DB_NAME,
        "vault": VAULT_NAME,
        "queries": QUERIES_NAME,
        "counts": {
            "dictations": len(rows),
            "dictations_ok": sum(1 for r in rows if r[10] == "ok" and r[8].strip()),
            "notes": sum(1 for rel in files if rel.startswith(NOTES_FOLDER + "/")),
            "transcripts": sum(1 for rel in files if rel.startswith(LOGS_FOLDER + "/")),
            "queries": len(queries),
        },
        "digest": _digest(rows, files, queries),
    }
    (out / MANIFEST_NAME).write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return corpus_paths(out, manifest)


def corpus_paths(out_dir: str | Path, manifest: dict) -> dict:
    """``manifest`` plus absolute ``db_path`` / ``vault_path`` / ``queries_path`` / ``root``."""
    root = Path(out_dir).resolve()
    return {
        **manifest,
        "root": str(root),
        "db_path": str(root / manifest["db"]),
        "vault_path": str(root / manifest["vault"]),
        "queries_path": str(root / manifest["queries"]),
    }


def load_corpus(out_dir: str | Path, *, entries: int, seed: int = DEFAULT_SEED) -> dict | None:
    """The manifest of a complete matching corpus already in ``out_dir``, else ``None``."""
    root = Path(out_dir)
    try:
        manifest = json.loads((root / MANIFEST_NAME).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(manifest, dict) or (
        manifest.get("generator_version"), manifest.get("entries"), manifest.get("seed")
    ) != (GENERATOR_VERSION, int(entries), int(seed)):
        return None
    try:
        paths = corpus_paths(root, manifest)
    except KeyError:
        return None
    if not (Path(paths["db_path"]).is_file() and Path(paths["queries_path"]).is_file()
            and Path(paths["vault_path"]).is_dir()):
        return None
    return paths


def ensure_corpus(out_dir: str | Path, *, entries: int = DEFAULT_ENTRIES, seed: int = DEFAULT_SEED) -> dict:
    """Reuse a matching corpus in ``out_dir`` or generate it."""
    return load_corpus(out_dir, entries=entries, seed=seed) or generate_corpus(out_dir, entries=entries, seed=seed)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Generate the deterministic recall perf corpus.")
    parser.add_argument("--out", type=Path, required=True, help="output folder")
    parser.add_argument("--entries", type=int, default=DEFAULT_ENTRIES, help="dictation rows (default 1000)")
    parser.add_argument("--seed", type=int, default=DEFAULT_SEED, help="random seed (default 42)")
    args = parser.parse_args(argv)
    try:
        manifest = generate_corpus(args.out, entries=args.entries, seed=args.seed)
    except (OSError, ValueError, sqlite3.Error) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    print(json.dumps(manifest, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
