from types import SimpleNamespace
from unittest.mock import ANY, Mock

import numpy as np
import pytest

pytest.importorskip("sounddevice")

from localflow import app
from localflow.context import AppInfo

REGISTRY = ["skill_part", "candidate-solutioning"]


def _config(cleanup_enabled: bool, hot_phrases: bool = True) -> SimpleNamespace:
    return SimpleNamespace(
        cleanup_enabled=cleanup_enabled,
        cleanup_level="light",
        styles_enabled=False,
        style_work="formal",
        style_email="formal",
        style_personal="casual",
        style_other="formal",
        app_category_overrides=[],
        spoken_symbols=True,
        spoken_punctuation=True,
        silence_threshold_dbfs=-50.0,
        paste_method="auto",
        hot_phrases=hot_phrases,
    )


@pytest.mark.parametrize("cleanup_enabled", [True, False])
def test_process_clip_normalizes_known_command(monkeypatch, cleanup_enabled):
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda text, method: pasted.append(text))
    transcriber = Mock(transcribe=Mock(return_value="Run slash skill part with these arguments."))
    cleaner = Mock(clean=Mock(side_effect=lambda text, **_kwargs: text.replace("Run", "Please run")))

    app._process_clip(
        np.full(16000, 0.1, np.float32), _config(cleanup_enabled), transcriber, cleaner, REGISTRY
    )

    expected = "Please run" if cleanup_enabled else "Run"
    assert pasted == [f"{expected} /skill_part with these arguments."]
    assert cleaner.clean.called is cleanup_enabled


def test_process_clip_short_command_skips_cleanup(monkeypatch):
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda text, method: pasted.append(text))
    transcriber = Mock(transcribe=Mock(return_value="Slash candidate solutioning."))
    cleaner = Mock(clean=Mock(side_effect=AssertionError("cleanup must not run")))

    app._process_clip(np.full(16000, 0.1, np.float32), _config(True), transcriber, cleaner, REGISTRY)

    assert pasted == ["/candidate-solutioning"]


def test_process_clip_can_return_text_without_pasting(monkeypatch):
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda *args: pasted.append(args))
    transcriber = Mock(transcribe=Mock(return_value="A complete sentence for the test."))
    cleaner = Mock(clean=Mock(side_effect=lambda text, **_kwargs: text))

    result = app._process_clip(
        np.full(16000, 0.1, np.float32),
        _config(False),
        transcriber,
        cleaner,
        paste=False,
    )

    assert result == "A complete sentence for the test."
    assert pasted == []


# --------------------------------------------------------------- hot phrases ---

from localflow.hotphrases import HotPhraseStore


def _store(tmp_path, entries):
    store = HotPhraseStore(tmp_path / "hot_phrases.json")
    for trigger, text in entries:
        store.add(trigger, text)
    return store


def test_process_clip_whole_match_skips_cleanup_and_symbols(monkeypatch, tmp_path):
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda text, _method="auto": pasted.append(text))
    transcriber = Mock(transcribe=Mock(return_value="Review checklist."))
    cleaner = Mock(clean=Mock(side_effect=AssertionError("cleanup must not run")))
    store = _store(tmp_path, [("review checklist", "check slash dash back\nsecond line")])

    app._process_clip(
        np.full(16000, 0.1, np.float32), _config(True), transcriber, cleaner, REGISTRY,
        hot_phrases=store,
    )

    # Verbatim expansion: "slash dash" would have become "- -" under symbols.
    assert pasted == ["check slash dash back\nsecond line"]


def test_process_clip_inline_expansion_after_cleanup(monkeypatch, tmp_path):
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda text, _method="auto": pasted.append(text))
    transcriber = Mock(
        transcribe=Mock(return_value="please review checklist then commit now")
    )
    cleaner = Mock(clean=Mock(side_effect=lambda text, **_kwargs: text.upper()))
    store = _store(tmp_path, [("review checklist", "CHECK IT")])

    app._process_clip(
        np.full(16000, 0.1, np.float32), _config(True), transcriber, cleaner, REGISTRY,
        hot_phrases=store,
    )

    assert pasted == ["PLEASE CHECK IT THEN COMMIT NOW"]


def test_process_clip_trigger_with_spoken_symbol_word_expands(monkeypatch, tmp_path):
    # Triggers are parked before symbols run, so "review dash checklist"
    # expands instead of becoming "review-checklist".
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda text, _method="auto": pasted.append(text))
    transcriber = Mock(
        transcribe=Mock(return_value="please review dash checklist now")
    )
    cleaner = Mock(clean=Mock(side_effect=lambda text, **_kwargs: text))
    store = _store(tmp_path, [("review dash checklist", "CHECK IT")])

    app._process_clip(
        np.full(16000, 0.1, np.float32), _config(False), transcriber, cleaner,
        REGISTRY, hot_phrases=store,
    )

    assert pasted == ["please CHECK IT now"]


def test_process_clip_expansion_is_not_symbol_converted(monkeypatch, tmp_path):
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda text, _method="auto": pasted.append(text))
    transcriber = Mock(transcribe=Mock(return_value="a dash b review checklist"))
    cleaner = Mock(clean=Mock(side_effect=lambda text, **_kwargs: text))
    store = _store(tmp_path, [("review checklist", "use slash here")])

    app._process_clip(
        np.full(16000, 0.1, np.float32), _config(False), transcriber, cleaner,
        REGISTRY, hot_phrases=store,
    )

    # Symbols still apply around the trigger, but the expansion's "slash"
    # stays literal.
    assert pasted == ["a-b use slash here"]


def test_process_clip_hot_phrases_disabled_in_config(monkeypatch, tmp_path):
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda text, _method="auto": pasted.append(text))
    transcriber = Mock(transcribe=Mock(return_value="review checklist"))
    cleaner = Mock(clean=Mock(side_effect=lambda text, **_kwargs: text))
    store = _store(tmp_path, [("review checklist", "CHECK IT")])

    app._process_clip(
        np.full(16000, 0.1, np.float32), _config(False, hot_phrases=False),
        transcriber, cleaner, REGISTRY, hot_phrases=store,
    )

    assert pasted == ["review checklist"]


def test_process_clip_silence_skips_transcriber_and_returns_empty(monkeypatch):
    transcriber = Mock()
    cleaner = Mock()
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda text, _method="auto": pasted.append(text))

    result = app._process_clip(
        np.zeros(16000, np.float32),
        _config(True),
        transcriber,
        cleaner,
        REGISTRY,
    )

    assert result == ""
    transcriber.transcribe.assert_not_called()
    assert pasted == []


def test_process_clip_uses_dictionary_prompt_and_app_style(monkeypatch, tmp_path):
    from localflow.dictionary import DictionaryStore

    config = _config(True)
    config.styles_enabled = True
    config.style_personal = "casual"
    config.app_category_overrides = ["com.example.editor=personal"]
    transcript = "teh LocalFlow with many useful words today."
    transcriber = Mock(transcribe=Mock(return_value=transcript))
    cleaner = Mock(clean=Mock(side_effect=lambda text, **_kwargs: text))
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda text, _method="auto": pasted.append(text))
    store = DictionaryStore(tmp_path / "dictionary.json")
    store.add_word("LocalFlow")
    store.add_replacement("teh", "the")

    result = app._process_clip(
        np.full(16000, 0.1, np.float32),
        config,
        transcriber,
        cleaner,
        app_info=AppInfo("Editor", "com.example.editor"),
        dictionary_store=store,
    )

    transcriber.transcribe.assert_called_once_with(
        ANY, prompt="Vocabulary: LocalFlow."
    )
    cleaner.clean.assert_called_once()
    assert cleaner.clean.call_args.kwargs["style"] == "casual"
    assert result.startswith("the LocalFlow")
    assert pasted == [result]
