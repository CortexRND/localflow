from pathlib import Path
from types import SimpleNamespace
from unittest.mock import ANY, Mock

import numpy as np
import pytest

pytest.importorskip("sounddevice")

from localflow import app
from localflow.context import AppInfo
from localflow.dictionary import DictionaryStore
from localflow.history import HistoryStore
from localflow.jobs import Job

REGISTRY = ["skill_part", "candidate-solutioning"]


def _config(cleanup_enabled: bool = True, hot_phrases: bool = True) -> SimpleNamespace:
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
        sample_rate=16000,
        keep_audio_clips=5,
        history_enabled=True,
        command_names=(),
        skills_dirs=(),
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


@pytest.mark.parametrize(
    ("audio", "transcription", "expected_status", "expected_error"),
    [
        (np.full(16000, 0.1, dtype=np.float32), "hello there", "ok", False),
        (np.zeros(16000, dtype=np.float32), "unused", "silent", False),
        (
            np.full(16000, 0.1, dtype=np.float32),
            RuntimeError("STT failed"),
            "failed",
            True,
        ),
    ],
)
def test_process_job_records_dictation_outcomes(
    tmp_path, audio, transcription, expected_status, expected_error
):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    transcriber = Mock()
    if isinstance(transcription, Exception):
        transcriber.transcribe.side_effect = transcription
    else:
        transcriber.transcribe.return_value = transcription

    text, error = app._process_job(
        Job(kind="dictate", audio=audio, app_info=AppInfo("Editor", "editor")),
        _config(),
        transcriber,
        Mock(clean=Mock(side_effect=lambda value, **_kwargs: value)),
        Mock(),
        (),
        HotPhraseStore(tmp_path / "phrases.json"),
        DictionaryStore(tmp_path / "dictionary.json"),
        store,
    )

    row = store.recent()[0]
    assert row["status"] == expected_status
    assert row["kind"] == "dictate"
    assert row["app_name"] == "Editor"
    assert error is expected_error
    assert bool(row["audio_path"]) is (expected_status != "silent")
    assert row["raw_text"] == (
        transcription if isinstance(transcription, str) and expected_status == "ok" else ""
    )
    assert text == (
        "hello there"
        if expected_status == "ok"
        else "" if expected_status == "silent" else None
    )


def test_failed_dictation_keeps_retry_clip_when_successful_retention_is_zero(
    tmp_path,
):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    config = _config()
    config.keep_audio_clips = 0
    audio = np.full(16000, 0.1, dtype=np.float32)

    app._process_job(
        Job(kind="dictate", audio=audio),
        config,
        Mock(transcribe=Mock(side_effect=RuntimeError("STT failed"))),
        Mock(),
        Mock(),
        (),
        HotPhraseStore(tmp_path / "phrases.json"),
        DictionaryStore(tmp_path / "dictionary.json"),
        store,
    )

    entry = store.recent()[0]
    assert entry["status"] == "failed"
    assert Path(entry["audio_path"]).is_file()


def test_disabled_history_does_not_store_a_clip(tmp_path):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips", enabled=False)
    config = _config()

    app._process_job(
        Job(kind="dictate", audio=np.full(16000, 0.1, dtype=np.float32)),
        config,
        Mock(transcribe=Mock(side_effect=RuntimeError("STT failed"))),
        Mock(),
        Mock(),
        (),
        HotPhraseStore(tmp_path / "phrases.json"),
        DictionaryStore(tmp_path / "dictionary.json"),
        store,
    )

    assert not (tmp_path / "clips").exists()


def test_retry_history_entry_reprocesses_clip_without_pasting(tmp_path, monkeypatch):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    clip = store.write_clip(np.full(16000, 0.1, dtype=np.float32), 16000)
    entry_id = store.add(
        kind="dictate",
        status="failed",
        audio_path=clip,
        raw_text="previous raw",
        final_text="",
    )
    monkeypatch.setattr(app, "write_last_text", lambda _text: None)
    monkeypatch.setattr(
        app, "paste_text", lambda *_args: pytest.fail("retry must not paste")
    )

    text = app.retry_history_entry(
        entry_id,
        store,
        config=_config(),
        transcriber=Mock(transcribe=Mock(return_value="retried words")),
        cleaner=Mock(clean=Mock(side_effect=lambda value, **_kwargs: value)),
        dictionary_store=DictionaryStore(tmp_path / "dictionary.json"),
        hot_phrases=HotPhraseStore(tmp_path / "phrases.json"),
    )

    updated = store.get(entry_id)
    assert text == "retried words"
    assert updated["status"] == "ok"
    assert updated["raw_text"] == "retried words"
    assert updated["final_text"] == "retried words"
