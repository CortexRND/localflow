from types import SimpleNamespace
from unittest.mock import Mock

import numpy as np
import pytest

pytest.importorskip("sounddevice")

from localflow import app  # noqa: E402

REGISTRY = ["skill_part", "candidate-solutioning"]


def _config(cleanup_enabled: bool, hot_phrases: bool = True) -> SimpleNamespace:
    return SimpleNamespace(
        cleanup_enabled=cleanup_enabled,
        spoken_symbols=True,
        hot_phrases=hot_phrases,
    )


@pytest.mark.parametrize("cleanup_enabled", [True, False])
def test_process_clip_normalizes_known_command(monkeypatch, cleanup_enabled):
    pasted = []
    monkeypatch.setattr(app, "paste_text", pasted.append)
    transcriber = Mock(transcribe=Mock(return_value="Run slash skill part with these arguments."))
    cleaner = Mock(clean=Mock(side_effect=lambda text: text.replace("Run", "Please run")))

    app._process_clip(
        np.zeros(16000, np.float32), _config(cleanup_enabled), transcriber, cleaner, REGISTRY
    )

    expected = "Please run" if cleanup_enabled else "Run"
    assert pasted == [f"{expected} /skill_part with these arguments."]
    assert cleaner.clean.called is cleanup_enabled


def test_process_clip_short_command_skips_cleanup(monkeypatch):
    pasted = []
    monkeypatch.setattr(app, "paste_text", pasted.append)
    transcriber = Mock(transcribe=Mock(return_value="Slash candidate solutioning."))
    cleaner = Mock(clean=Mock(side_effect=AssertionError("cleanup must not run")))

    app._process_clip(np.zeros(16000, np.float32), _config(True), transcriber, cleaner, REGISTRY)

    assert pasted == ["/candidate-solutioning"]


# --------------------------------------------------------------- hot phrases ---

from localflow.hotphrases import HotPhraseStore  # noqa: E402


def _store(tmp_path, entries):
    store = HotPhraseStore(tmp_path / "hot_phrases.json")
    for trigger, text in entries:
        store.add(trigger, text)
    return store


def test_process_clip_whole_match_skips_cleanup_and_symbols(monkeypatch, tmp_path):
    pasted = []
    monkeypatch.setattr(app, "paste_text", pasted.append)
    transcriber = Mock(transcribe=Mock(return_value="Review checklist."))
    cleaner = Mock(clean=Mock(side_effect=AssertionError("cleanup must not run")))
    store = _store(tmp_path, [("review checklist", "check slash dash back\nsecond line")])

    app._process_clip(
        np.zeros(16000, np.float32), _config(True), transcriber, cleaner, REGISTRY,
        hot_phrases=store,
    )

    # Verbatim expansion: "slash dash" would have become "- -" under symbols.
    assert pasted == ["check slash dash back\nsecond line"]


def test_process_clip_inline_expansion_after_cleanup(monkeypatch, tmp_path):
    pasted = []
    monkeypatch.setattr(app, "paste_text", pasted.append)
    transcriber = Mock(
        transcribe=Mock(return_value="please review checklist then commit now")
    )
    cleaner = Mock(clean=Mock(side_effect=lambda text: text.upper()))
    store = _store(tmp_path, [("review checklist", "CHECK IT")])

    app._process_clip(
        np.zeros(16000, np.float32), _config(True), transcriber, cleaner, REGISTRY,
        hot_phrases=store,
    )

    assert pasted == ["PLEASE CHECK IT THEN COMMIT NOW"]


def test_process_clip_trigger_with_spoken_symbol_word_expands(monkeypatch, tmp_path):
    # Triggers are parked before symbols run, so "review dash checklist"
    # expands instead of becoming "review-checklist".
    pasted = []
    monkeypatch.setattr(app, "paste_text", pasted.append)
    transcriber = Mock(
        transcribe=Mock(return_value="please review dash checklist now")
    )
    cleaner = Mock(clean=Mock(side_effect=lambda text: text))
    store = _store(tmp_path, [("review dash checklist", "CHECK IT")])

    app._process_clip(
        np.zeros(16000, np.float32), _config(False), transcriber, cleaner,
        REGISTRY, hot_phrases=store,
    )

    assert pasted == ["please CHECK IT now"]


def test_process_clip_expansion_is_not_symbol_converted(monkeypatch, tmp_path):
    pasted = []
    monkeypatch.setattr(app, "paste_text", pasted.append)
    transcriber = Mock(transcribe=Mock(return_value="a dash b review checklist"))
    cleaner = Mock(clean=Mock(side_effect=lambda text: text))
    store = _store(tmp_path, [("review checklist", "use slash here")])

    app._process_clip(
        np.zeros(16000, np.float32), _config(False), transcriber, cleaner,
        REGISTRY, hot_phrases=store,
    )

    # Symbols still apply around the trigger, but the expansion's "slash"
    # stays literal.
    assert pasted == ["a-b use slash here"]


def test_process_clip_hot_phrases_disabled_in_config(monkeypatch, tmp_path):
    pasted = []
    monkeypatch.setattr(app, "paste_text", pasted.append)
    transcriber = Mock(transcribe=Mock(return_value="review checklist"))
    cleaner = Mock(clean=Mock(side_effect=lambda text: text))
    store = _store(tmp_path, [("review checklist", "CHECK IT")])

    app._process_clip(
        np.zeros(16000, np.float32), _config(False, hot_phrases=False),
        transcriber, cleaner, REGISTRY, hot_phrases=store,
    )

    assert pasted == ["review checklist"]
