import json
from pathlib import Path

import pytest

from localflow.config import Config
from localflow.context import AppInfo, category_for
from localflow.dictionary import apply_replacements
from localflow.formatting import apply_spoken_formatting, apply_style
from localflow.hotphrases import render_dictation

CORPUS = Path(__file__).parent / "fixtures" / "dictation_corpus.jsonl"


def _cases():
    return [
        json.loads(line)
        for line in CORPUS.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


@pytest.mark.parametrize("case", _cases(), ids=lambda case: case["name"])
def test_dictation_corpus(case):
    overrides = case["config_overrides"]
    category = overrides.get("category", "other")
    style = overrides.get("style")
    config = Config(
        cleanup_enabled=False,
        spoken_punctuation=overrides.get("spoken_formatting", False),
        styles_enabled=style is not None,
        app_category_overrides=[f"CorpusApp={category}"],
        **({f"style_{category}": style} if style is not None else {}),
    )
    selected_category = category_for(
        AppInfo("CorpusApp"), config.app_category_overrides
    )
    style_name = (
        getattr(config, f"style_{selected_category}")
        if config.styles_enabled
        else None
    )
    replacements = case.get("replacements", [])
    phrases = [tuple(phrase) for phrase in case.get("phrases", [])]
    actual = render_dictation(
        case["input"],
        phrases,
        replace=(
            (lambda text: apply_replacements(text, replacements))
            if replacements
            else None
        ),
        fmt=apply_spoken_formatting if config.spoken_punctuation else None,
        clean=None,
        symbols=None,
        style=(
            (lambda text: apply_style(text, style_name))
            if style_name is not None
            else None
        ),
    )
    expected = case["expected"]
    if case.get("xfail_reason") and actual != expected:
        pytest.xfail(
            f'{case["xfail_reason"]}: actual={actual!r}; expected={expected!r}'
        )
    assert actual == expected
