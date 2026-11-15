from datetime import datetime, timedelta

import pytest

from localflow import ask_model as ask
from localflow.config import Config
from localflow.providers.base import ProviderError
from localflow.recall import NO_ANSWER, Passage


class FakeLLM:
    def __init__(self, reply="The launch is Friday [1].", error=None):
        self.reply = reply
        self.error = error
        self.prompts = []

    def complete(self, system, user):
        self.prompts.append((system, user))
        if self.error:
            raise self.error
        return self.reply


@pytest.fixture
def corpus(monkeypatch):
    now = datetime(2026, 4, 15, 12)
    passages = [
        Passage("Launch Friday", "meeting", "Planning", now.timestamp(), "/vault/meeting.md#01:20"),
        Passage("Launch checklist", "notes", "Checklist", (now - timedelta(days=10)).timestamp(), "/vault/note.md"),
        Passage("Launch idea", "dictation", "Editor", (now - timedelta(days=40)).timestamp(), "42"),
    ]
    calls = []

    def build(**kwargs):
        calls.append(kwargs)
        return passages

    monkeypatch.setattr(ask.recall, "build_corpus", build)
    return now, passages, calls


def model(llm=None):
    return ask.AskModel(llm=llm or FakeLLM(), config=Config(), store=object())


def test_conversation_and_fresh_retrieval(corpus):
    llm = FakeLLM()
    chat = model(llm)
    first = chat.ask("When is launch?")
    chat.ask("What is on the launch checklist?")
    assert first.answer == "The launch is Friday [1]."
    assert len(corpus[2]) == 2
    assert "User: When is launch?\nAssistant: The launch is Friday [1]." in llm.prompts[1][1]
    assert "old citation numbers do not apply" in llm.prompts[1][1]
    assert "Question: What is on the launch checklist?" in llm.prompts[1][1]
    chat.clear()
    chat.ask("Launch?")
    assert "User: When" not in llm.prompts[-1][1]
    assert len(chat.turns) == 1


def test_citations_map_to_original_sources(corpus):
    turn = model(FakeLLM("Friday [1], checklist [2], idea [3], invented [99].")).ask("Launch")
    assert "[99]" not in turn.answer
    assert [c["n"] for c in turn.citations] == [1, 2, 3]
    assert [ask.citation_target(c) for c in turn.citations] == [
        ("file", "/vault/meeting.md"), ("file", "/vault/note.md"), ("history", 42),
    ]
    assert ask.citation_label(turn.citations[0]) == "[1] Meeting · 2026-04-15 · Planning"
    assert "Note" in ask.citation_label(turn.citations[1])
    assert "Dictation" in ask.citation_label(turn.citations[2])


@pytest.mark.parametrize("time_filter,count", [("Any time", 3), ("Past week", 1), ("Past month", 2)])
def test_time_filters(corpus, time_filter, count):
    turn = model().ask("launch", time_filter=time_filter, now=corpus[0])
    assert len(turn.citations) == count


def test_source_filters_and_empty_selection(corpus):
    chat = model()
    turn = chat.ask("launch", sources={"notes"})
    assert [c["source"] for c in turn.citations] == ["notes"]
    turn = chat.ask("launch", sources=set())
    assert turn.error == "Choose at least one source to search."
    assert len(corpus[2]) == 1


@pytest.mark.parametrize("reply", ["", "   ", NO_ANSWER])
def test_empty_model_answers(corpus, reply):
    turn = model(FakeLLM(reply)).ask("launch")
    assert turn.answer == NO_ANSWER
    assert turn.citations == []


def test_no_matches_and_empty_question(corpus):
    llm = FakeLLM()
    chat = model(llm)
    assert chat.ask("unrelated zebras").answer == NO_ANSWER
    assert chat.ask("  ").error == "Type a question to get started."
    assert len(chat.turns) == 1
    assert not llm.prompts


@pytest.mark.parametrize("error", [ProviderError("offline"), OSError("offline")])
def test_unreachable_model_and_failed_turn_context(corpus, error):
    llm = FakeLLM(error=error)
    chat = model(llm)
    turn = chat.ask("launch")
    assert "Models page" in turn.error
    assert turn.citations
    llm.error = None
    chat.ask("launch checklist")
    assert "User: launch\n" not in llm.prompts[-1][1]


def test_unexpected_failure_is_friendly(corpus):
    turn = model(FakeLLM(error=RuntimeError("private details"))).ask("launch")
    assert turn.error == "Couldn't answer that question. Please try again."


def test_builds_configured_llm_like_server(corpus, monkeypatch):
    config = Config(history_enabled=False)
    llm = FakeLLM()
    calls = []
    monkeypatch.setattr(ask, "load_config", lambda: config)
    monkeypatch.setattr(ask, "build_llm", lambda cfg, **kw: calls.append((cfg, kw)) or llm)
    chat = ask.AskModel(store=object())
    chat.ask("launch")
    assert calls == [(config, {"timeout": 300, "num_ctx": 8192})]
    assert corpus[2][0]["store"] is None
    assert corpus[2][0]["vault_path"] == config.vault_path
