"""Conversation and fresh retrieval for the native Ask page (no AppKit)."""

from dataclasses import dataclass, field

from localflow import recall
from localflow.config import load_config
from localflow.providers.base import ProviderError
from localflow.providers.factory import build_llm

EXAMPLES = (
    "What did we decide in our last meeting?",
    "What are my next steps for the project?",
    "What have I dictated about the launch?",
)
PRIVACY = (
    "Answers use the model selected on the Models page. "
    "If you select a cloud provider, it will receive the matched snippets "
    "and conversation context."
)
TIME_FILTERS = {"Any time": None, "Past week": "7d", "Past month": "30d"}
SOURCES = {"dictation": "Dictation", "meeting": "Meeting", "notes": "Note"}


@dataclass
class Turn:
    question: str
    answer: str = ""
    citations: list[dict] = field(default_factory=list)
    error: str = ""


def citation_target(citation):
    """Return a history id or a source file, without a transcript timestamp."""
    if citation["source"] == "dictation":
        return "history", int(citation["ref"])
    if citation["source"] == "meeting":
        return "file", citation["ref"].rsplit("#", 1)[0]
    return "file", citation["ref"]


def citation_label(citation):
    source = SOURCES[citation["source"]]
    date = citation["when"][:10] or "Unknown date"
    return f'[{citation["n"]}] {source} · {date} · {citation["title"]}'


class AskModel:
    def __init__(self, *, llm=None, config=None, store=None):
        self.llm = llm
        self.config = config
        self.store = store
        self.turns = []

    def clear(self):
        self.turns.clear()

    def ask(self, question, *, time_filter="Any time", sources=None, now=None):
        """Called on a worker; failed turns stay visible but aren't model context."""
        question = question.strip()
        turn = Turn(question)
        if not question:
            turn.error = "Type a question to get started."
            return turn
        try:
            since = TIME_FILTERS[time_filter]
            selected = set(SOURCES) if sources is None else set(sources)
            if not selected:
                turn.error = "Choose at least one source to search."
            else:
                config = self.config or load_config()
                corpus = recall.build_corpus(
                    store=self.store if config.history_enabled else None,
                    vault_path=config.vault_path, notes_folder=config.notes_folder,
                    logs_folder=config.logs_folder,
                )
                top = recall.rank(
                    question, [p for p in corpus if p.source in selected],
                    k=8, since=since, now=now,
                )
                if not top:
                    turn.answer = recall.NO_ANSWER
                else:
                    turn.citations = recall._citations(top, question)
                    context = "\n\n".join(
                        f"User: {t.question}\nAssistant: {t.answer}"
                        for t in self.turns if not t.error
                    )
                    prompt = (
                        "Conversation so far (context only, not source evidence; "
                        "old citation numbers do not apply to this question):\n"
                        + context + "\n\n" + recall.build_prompt(question, top)
                    )
                    llm = self.llm or build_llm(config, timeout=300, num_ctx=8192)
                    answer = llm.complete("", prompt).strip()
                    if not answer or recall.NO_ANSWER.lower().rstrip(".") in answer.lower():
                        turn.answer = recall.NO_ANSWER
                        turn.citations = []
                    else:
                        turn.answer = recall.clean_citations(answer, len(top))
        except (ProviderError, OSError):
            turn.error = "Couldn't reach the model or local sources. Check the Models page and try again."
        except Exception:  # noqa: BLE001 - keep provider failures out of the UI thread
            turn.error = "Couldn't answer that question. Please try again."
        self.turns.append(turn)
        return turn
