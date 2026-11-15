"""Recall: ask questions across dictation history, meeting transcripts and notes.

Everything is local. Passages are pulled from the dictation history database and
the Obsidian vault, ranked with a small pure-Python BM25, and (optionally) handed
to the configured LLM, which answers using only those passages. The index is
rebuilt on every call (O(corpus)); corpora are at most tens of thousands of
passages so this stays well under a second without any new dependency.
"""

from __future__ import annotations

import logging
import math
import re
from collections import Counter
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta
from pathlib import Path

from localflow.history import HistoryStore
from localflow.meetings import read_transcript_log
from localflow.providers.base import LLMProvider, ProviderError

log = logging.getLogger("localflow.recall")

NO_ANSWER = "I couldn't find that in your history."

SOURCE_DICTATION = "dictation"
SOURCE_MEETING = "meeting"
SOURCE_NOTES = "notes"

# BM25 parameters (standard defaults).
BM25_K1 = 1.5
BM25_B = 0.75
# A quoted phrase found verbatim in a passage adds this much to its BM25 score.
PHRASE_BOOST = 3.0
# Recency tiebreak: score *= 1 + RECENCY_WEIGHT * 0.5 ** (age_days / RECENCY_HALF_LIFE_DAYS).
# At most +10% for something said just now, fading with a 30-day half-life, so
# it only reorders passages whose relevance is already close.
RECENCY_WEIGHT = 0.1
RECENCY_HALF_LIFE_DAYS = 30.0

CHUNK_TARGET_WORDS = 150
CHUNK_MAX_WORDS = 200
PROMPT_PASSAGE_MAX_WORDS = 300
SNIPPET_CHARS = 200
HISTORY_SCAN_LIMIT = 200_000

_STOPWORD_TEXT = (
    "a an and are as at be been but by can did do does for from had has have he her his "
    "how i if in is it its me my of on or our she so than that the their them then there "
    "these they this to us was we were what when where which who whom why will with would "
    "you your about into over said say tell told"
)
_STOPWORDS = frozenset(_STOPWORD_TEXT.split())
_WORD_RE = re.compile(r"[a-z0-9]+(?:'[a-z0-9]+)*")
_PHRASE_RE = re.compile(r'"([^"]+)"')
_STAMP_RE = re.compile(r"^\*\*\[?(\d+:\d{2}(?::\d{2})?)\]?\*\*")
_FILE_STAMP_RE = re.compile(r"^(\d{4})-(\d{2})-(\d{2}) (\d{2})(\d{2})(?:\s+(.*))?$")
_CITATION_RE = re.compile(r"\[(\d{1,2}(?:\s*[,;]\s*\d{1,2})*)\]")
_RELATIVE_RE = re.compile(r"^(\d+)\s*d$", re.IGNORECASE)


# ------------------------------------------------------------------ data ---

@dataclass
class Passage:
    text: str
    source: str            # dictation | meeting | notes
    title: str             # meeting/note title, or the app name for dictations
    ts: float | None       # epoch seconds
    ref: str               # dictation id, or transcript path + "#" + stamp
    app: str = ""


@dataclass
class RecallResult:
    answer: str | None
    citations: list[dict] = field(default_factory=list)

    def to_dict(self) -> dict:
        return {"answer": self.answer, "citations": self.citations}


# ------------------------------------------------------------- tokenizing ---

def stem(word: str) -> str:
    """Very small suffix stemmer: plurals, -ing, -ed (enough to match budgets/budget)."""
    word = word.removesuffix("'s")
    word = word.replace("'", "")
    if len(word) > 4 and word.endswith("ies"):
        return word[:-3] + "y"
    if len(word) > 5 and word.endswith("ing"):
        word = word[:-3]
        if len(word) > 2 and word[-1] == word[-2] and word[-1] not in "lsz":
            word = word[:-1]
        return word
    if len(word) > 4 and word.endswith("ed"):
        word = word[:-2]
        if len(word) > 2 and word[-1] == word[-2] and word[-1] not in "lsz":
            word = word[:-1]
        return word
    if len(word) > 3 and word.endswith("sses"):
        return word[:-2]
    if len(word) > 3 and word.endswith("s") and not word.endswith(("ss", "us", "is")):
        return word[:-1]
    return word


def tokenize(text: str, *, keep_stopwords: bool = False) -> list[str]:
    tokens = []
    for raw in _WORD_RE.findall(text.lower()):
        if not keep_stopwords and raw in _STOPWORDS:
            continue
        tokens.append(stem(raw))
    return tokens


def extract_phrases(question: str) -> tuple[str, list[list[str]]]:
    """Split ``"quoted phrases"`` out of a question; returns (plain text, phrase token lists)."""
    phrases = [
        toks for m in _PHRASE_RE.findall(question)
        if (toks := tokenize(m, keep_stopwords=True))
    ]
    return question.replace('"', " "), phrases


def _contains_run(tokens: list[str], run: list[str]) -> bool:
    n = len(run)
    if n == 0 or n > len(tokens):
        return False
    first = run[0]
    return any(
        tokens[i] == first and tokens[i:i + n] == run
        for i in range(len(tokens) - n + 1)
    )


# --------------------------------------------------------------- since ---

def parse_since(value: str | date | datetime | None, now: datetime | None = None) -> datetime | None:
    """``today``, ``yesterday``, ``7d``/``30d`` or ``YYYY-MM-DD`` -> local start datetime."""
    if value is None or value == "":
        return None
    now = now or datetime.now()
    if isinstance(value, datetime):
        return value
    if isinstance(value, date):
        return datetime(value.year, value.month, value.day)
    text = str(value).strip().lower()
    midnight = now.replace(hour=0, minute=0, second=0, microsecond=0)
    if text == "today":
        return midnight
    if text == "yesterday":
        return midnight - timedelta(days=1)
    match = _RELATIVE_RE.match(text)
    if match:
        return now - timedelta(days=int(match.group(1)))
    try:
        return datetime.strptime(text, "%Y-%m-%d")
    except ValueError:
        raise ValueError(
            f"invalid since value {value!r}; use today, yesterday, 7d, 30d or YYYY-MM-DD"
        ) from None


# -------------------------------------------------------------- chunking ---

def parse_file_stamp(path: Path) -> tuple[datetime, str]:
    """(start time, title) from ``YYYY-MM-DD HHMM Title[ (transcript)].md``.

    Falls back to the file's mtime and its bare stem when the name has no stamp.
    """
    stem_ = Path(path).stem.removesuffix(" (transcript)")
    match = _FILE_STAMP_RE.match(stem_)
    if match:
        try:
            when = datetime(*(int(match.group(i)) for i in range(1, 6)))
        except ValueError:
            when = None
        if when is not None:
            return when, (match.group(6) or "").strip() or stem_
    try:
        mtime = datetime.fromtimestamp(Path(path).stat().st_mtime)
    except OSError:
        mtime = datetime.now()
    return mtime, stem_


def chunk_lines(text: str, target: int = CHUNK_TARGET_WORDS,
                maximum: int = CHUNK_MAX_WORDS) -> list[str]:
    """Group lines into ~``target``-word chunks without splitting a line.

    A transcript line starts with its ``**[mm:ss]** Speaker:`` prefix, so keeping
    lines whole keeps the prefix with its text. Only a single over-long line is
    split on word boundaries (the first piece keeps the prefix).
    """
    chunks: list[str] = []
    current: list[str] = []
    count = 0

    def flush() -> None:
        nonlocal current, count
        if current:
            chunks.append("\n".join(current))
        current, count = [], 0

    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        words = line.split()
        if len(words) > maximum:
            flush()
            for i in range(0, len(words), maximum):
                chunks.append(" ".join(words[i:i + maximum]))
            continue
        if current and count + len(words) > maximum:
            flush()
        current.append(line)
        count += len(words)
        if count >= target:
            flush()
    flush()
    return chunks


def _stamp_seconds(stamp: str) -> int:
    parts = [int(p) for p in stamp.split(":")]
    seconds = 0
    for part in parts:
        seconds = seconds * 60 + part
    return seconds


def transcript_passages(path: Path) -> list[Passage]:
    try:
        text = read_transcript_log(path)
    except OSError as exc:
        log.warning("recall: cannot read %s: %s", path, exc)
        return []
    started, title = parse_file_stamp(path)
    # Drop the "# Title — transcript" heading; only spoken lines are passages.
    text = "\n".join(line for line in text.splitlines() if not line.startswith("# "))
    passages = []
    for chunk in chunk_lines(text):
        match = _STAMP_RE.match(chunk)
        stamp = match.group(1) if match else ""
        when = started + timedelta(seconds=_stamp_seconds(stamp)) if stamp else started
        passages.append(Passage(
            text=chunk, source=SOURCE_MEETING, title=title, ts=when.timestamp(),
            ref=f"{path}#{stamp}" if stamp else str(path),
        ))
    return passages


def _strip_frontmatter(text: str) -> str:
    if text.startswith("---\n"):
        end = text.find("\n---\n", 4)
        if end != -1:
            return text[end + 5:]
    return text


def notes_passages(path: Path) -> list[Passage]:
    try:
        text = _strip_frontmatter(Path(path).read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError) as exc:
        log.warning("recall: cannot read %s: %s", path, exc)
        return []
    started, title = parse_file_stamp(path)
    return [
        Passage(text=chunk, source=SOURCE_NOTES, title=title, ts=started.timestamp(),
                ref=str(path))
        for chunk in chunk_lines(text)
    ]


def build_corpus(
    *,
    store: HistoryStore | None = None,
    vault_path: str | Path = "",
    notes_folder: str = "",
    logs_folder: str = "",
) -> list[Passage]:
    passages: list[Passage] = []
    if store is not None and store.enabled:
        for row in store.recent(limit=HISTORY_SCAN_LIMIT):
            text = (row.get("final_text") or "").strip()
            if row.get("status") != "ok" or not text:
                continue
            app = row.get("app_name") or ""
            passages.append(Passage(
                text=text, source=SOURCE_DICTATION, title=app, app=app,
                ts=float(row["ts"]), ref=str(row["id"]),
            ))
    if vault_path:
        vault = Path(vault_path).expanduser()
        logs_root = vault / logs_folder if logs_folder else None
        notes_root = vault / notes_folder if notes_folder else None
        if logs_root is not None and logs_root.is_dir():
            for path in sorted(logs_root.rglob("* (transcript).md")):
                if path.is_file():
                    passages.extend(transcript_passages(path))
        if notes_root is not None and notes_root.is_dir():
            for path in sorted(notes_root.rglob("*.md")):
                if path.is_file() and not path.name.endswith(" (transcript).md"):
                    passages.extend(notes_passages(path))
    return passages


# ------------------------------------------------------------------ BM25 ---

def rank(
    question: str,
    passages: list[Passage],
    *,
    k: int = 8,
    since: str | date | datetime | None = None,
    now: datetime | None = None,
) -> list[Passage]:
    """Top-``k`` passages for ``question`` (BM25 + phrase boost + recency tiebreak)."""
    now = now or datetime.now()
    start = parse_since(since, now)
    if start is not None:
        floor = start.timestamp()
        passages = [p for p in passages if p.ts is not None and p.ts >= floor]
    plain, phrases = extract_phrases(question)
    query_terms = list(dict.fromkeys(tokenize(plain)))
    if not query_terms or not passages or k <= 0:
        return []

    docs = [tokenize(p.text) for p in passages]
    n = len(docs)
    avg_len = (sum(len(d) for d in docs) / n) or 1.0
    df: Counter[str] = Counter()
    for doc in docs:
        df.update(set(doc) & set(query_terms))
    idf = {t: math.log(1 + (n - df[t] + 0.5) / (df[t] + 0.5)) for t in query_terms if df[t]}
    if not idf:
        return []

    now_ts = now.timestamp()
    scored: list[tuple[float, float, int]] = []
    for index, (passage, doc) in enumerate(zip(passages, docs)):
        tf = Counter(doc)
        score = 0.0
        for term, weight in idf.items():
            freq = tf.get(term, 0)
            if freq:
                norm = freq + BM25_K1 * (1 - BM25_B + BM25_B * len(doc) / avg_len)
                score += weight * freq * (BM25_K1 + 1) / norm
        if score <= 0:
            continue
        if phrases:
            full = tokenize(passage.text, keep_stopwords=True)
            score += PHRASE_BOOST * sum(_contains_run(full, ph) for ph in phrases)
        if passage.ts is not None:
            age_days = max(0.0, (now_ts - passage.ts) / 86400)
            score *= 1 + RECENCY_WEIGHT * 0.5 ** (age_days / RECENCY_HALF_LIFE_DAYS)
        scored.append((score, passage.ts or 0.0, index))
    scored.sort(key=lambda item: (-item[0], -item[1], item[2]))
    return [passages[i] for _, _, i in scored[:k]]


# ------------------------------------------------------------- citations ---

def _snippet(text: str, terms: set[str]) -> str:
    flat = " ".join(text.split())
    if len(flat) <= SNIPPET_CHARS:
        return flat
    start = 0
    for match in _WORD_RE.finditer(flat.lower()):
        if stem(match.group(0)) in terms:
            start = max(0, match.start() - 60)
            break
    piece = flat[start:start + SNIPPET_CHARS].strip()
    return ("…" if start else "") + piece + ("…" if start + SNIPPET_CHARS < len(flat) else "")


def _citation(n: int, passage: Passage, terms: set[str]) -> dict:
    when = (
        datetime.fromtimestamp(passage.ts).isoformat(timespec="minutes")
        if passage.ts is not None else ""
    )
    return {
        "n": n,
        "source": passage.source,
        "title": passage.title,
        "app": passage.app,
        "when": when,
        "snippet": _snippet(passage.text, terms),
        "ref": passage.ref,
    }


def _citations(passages: list[Passage], question: str) -> list[dict]:
    plain, _ = extract_phrases(question)
    terms = set(tokenize(plain))
    return [_citation(i, p, terms) for i, p in enumerate(passages, start=1)]


def clean_citations(answer: str, valid: int) -> str:
    """Drop ``[n]`` references to passages that don't exist (keeps ``[12:30]`` timestamps)."""
    def fix(match: re.Match) -> str:
        nums = [int(x) for x in re.split(r"\s*[,;]\s*", match.group(1))]
        keep = [x for x in nums if 1 <= x <= valid]
        if not keep:
            return ""
        return "[" + ", ".join(str(x) for x in keep) + "]"

    cleaned = _CITATION_RE.sub(fix, answer)
    return re.sub(r"[ \t]+([.,;:!?])", r"\1", re.sub(r"[ \t]{2,}", " ", cleaned)).strip()


# ----------------------------------------------------------------- prompt ---

_RECALL_PROMPT = """You answer questions about the user's own past dictations, meetings and notes. Use ONLY the numbered passages below. Be concise (a few sentences). Cite the passages you used like [2]. Never guess or add outside knowledge. If the passages do not contain the answer, reply with exactly: {no_answer}

Passages:
{passages}

Question: {question}
"""


def _format_passage(n: int, passage: Passage) -> str:
    when = (
        datetime.fromtimestamp(passage.ts).strftime("%Y-%m-%d %H:%M")
        if passage.ts is not None else "unknown time"
    )
    label = passage.source + (f" · {passage.title}" if passage.title else "")
    words = passage.text.split()
    text = passage.text
    if len(words) > PROMPT_PASSAGE_MAX_WORDS:
        text = " ".join(words[:PROMPT_PASSAGE_MAX_WORDS]) + " …"
    return f"[{n}] ({when} · {label})\n<<<\n{text}\n>>>"


def build_prompt(question: str, passages: list[Passage]) -> str:
    return _RECALL_PROMPT.format(
        no_answer=NO_ANSWER,
        passages="\n\n".join(_format_passage(i, p) for i, p in enumerate(passages, start=1)),
        question=question,
    )


# ------------------------------------------------------------- public API ---

def search(
    question: str,
    *,
    store: HistoryStore | None = None,
    vault_path: str | Path = "",
    notes_folder: str = "",
    logs_folder: str = "",
    k: int = 8,
    since: str | date | datetime | None = None,
    now: datetime | None = None,
) -> RecallResult:
    """Retrieval only: ranked passages as citations, ``answer=None``."""
    corpus = build_corpus(
        store=store, vault_path=vault_path, notes_folder=notes_folder,
        logs_folder=logs_folder,
    )
    top = rank(question, corpus, k=k, since=since, now=now)
    return RecallResult(answer=None, citations=_citations(top, question))


def recall(
    question: str,
    *,
    llm: LLMProvider,
    store: HistoryStore | None = None,
    vault_path: str | Path = "",
    notes_folder: str = "",
    logs_folder: str = "",
    k: int = 8,
    since: str | date | datetime | None = None,
    now: datetime | None = None,
) -> RecallResult:
    """Answer ``question`` from the user's own history using only retrieved passages."""
    corpus = build_corpus(
        store=store, vault_path=vault_path, notes_folder=notes_folder,
        logs_folder=logs_folder,
    )
    top = rank(question, corpus, k=k, since=since, now=now)
    if not top:
        return RecallResult(answer=NO_ANSWER, citations=[])
    citations = _citations(top, question)
    try:
        reply = llm.complete("", build_prompt(question, top)).strip()
    except (ProviderError, OSError) as exc:
        # Connection problems still leave the matching passages useful.
        log.warning("recall: LLM unavailable, returning passages only: %s", exc)
        return RecallResult(answer=None, citations=citations)
    if not reply or NO_ANSWER.lower().rstrip(".") in reply.lower():
        return RecallResult(answer=NO_ANSWER, citations=[])
    return RecallResult(answer=clean_citations(reply, len(top)), citations=citations)
