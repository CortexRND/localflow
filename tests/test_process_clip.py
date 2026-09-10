from types import SimpleNamespace
from unittest.mock import Mock

import numpy as np
import pytest

pytest.importorskip("sounddevice")

from localflow import app  # noqa: E402

REGISTRY = ["skill_part", "candidate-solutioning"]


def _config(cleanup_enabled: bool) -> SimpleNamespace:
    return SimpleNamespace(cleanup_enabled=cleanup_enabled, spoken_symbols=True)


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
