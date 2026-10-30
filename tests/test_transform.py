from unittest.mock import Mock

import pytest

from localflow import inject
from localflow.transform import (
    COMMAND_NO_SELECTION,
    COMMAND_WITH_SELECTION,
    POLISH,
    PROMPT_ENGINEER,
    Transformer,
    command_prompt,
    transform_prompt,
)


def test_prompt_constants_match_the_specification():
    assert COMMAND_WITH_SELECTION == (
        "You are a writing assistant. Apply the instruction to the selected text. "
        "Return only the resulting text that should replace the selection, with no "
        "commentary, no quotes, and no explanations.\n\nInstruction:\n{instruction}"
        "\n\nSelected text:\n{selection}"
    )
    assert COMMAND_NO_SELECTION == (
        "You are a writing assistant. Write the text the instruction asks for. "
        "Return only that text, with no commentary, no quotes, and no explanations."
        "\n\nInstruction:\n{instruction}"
    )
    assert POLISH == (
        "You are an editor. Rewrite the text to be clear, concise and well-structured "
        "while keeping the author's meaning, voice and every fact. Fix grammar and "
        "punctuation. Do not add new information. Return only the rewritten text with "
        "no commentary, no quotes, and no explanations.\n\nText:\n{text}"
    )
    assert PROMPT_ENGINEER == (
        "You turn rough notes into a clear prompt for an AI assistant. Rewrite the "
        "text as a well-structured prompt: state the goal, the relevant context, the "
        "constraints and the desired output format, using short sections or bullet "
        "points where helpful. Keep every requirement from the original and do not "
        "invent new ones. Return only the prompt with no commentary.\n\nText:\n{text}"
    )


@pytest.mark.parametrize(
    ("prompt", "expected"),
    [
        (
            command_prompt("make it shorter", "selected words"),
            COMMAND_WITH_SELECTION.format(
                instruction="make it shorter", selection="selected words"
            ),
        ),
        (
            command_prompt("write a greeting", ""),
            COMMAND_NO_SELECTION.format(instruction="write a greeting"),
        ),
        (transform_prompt("polish", "rough text"), POLISH.format(text="rough text")),
        (
            transform_prompt("prompt", "rough notes"),
            PROMPT_ENGINEER.format(text="rough notes"),
        ),
    ],
)
def test_transform_prompts_match_exact_templates(prompt, expected):
    assert prompt == expected


def test_transformer_splits_system_user_and_strips_echoed_label():
    llm = Mock()
    llm.complete.return_value = '  "Text:\nRewritten text."\n'

    result = Transformer(llm).run(POLISH.format(text="rough"))

    system, user = llm.complete.call_args.args
    assert system == POLISH.split("\n\n", 1)[0]
    assert user == POLISH.split("\n\n", 1)[1].format(text="rough")
    assert result == "Rewritten text."


def test_transformer_strips_prompt_label_too():
    llm = Mock()
    llm.complete.return_value = "Prompt: \nA prompt."

    assert Transformer(llm).run(PROMPT_ENGINEER.format(text="notes")) == "A prompt."


@pytest.mark.parametrize("output", ["", "Text: \n", "  ''  "])
def test_transformer_returns_none_for_empty_output(output):
    llm = Mock()
    llm.complete.return_value = output
    assert Transformer(llm).run(POLISH.format(text="rough")) is None


def test_transformer_returns_none_when_provider_fails():
    llm = Mock()
    llm.complete.side_effect = RuntimeError("offline")
    assert Transformer(llm).run(POLISH.format(text="rough")) is None


def test_capture_selection_returns_changed_clipboard_and_original(monkeypatch):
    clipboard = {"text": "old"}

    def copy_selection():
        clipboard["text"] = "selected"

    monkeypatch.setattr(inject, "current", lambda: Mock(copy_selection=copy_selection))
    monkeypatch.setattr(inject.pyperclip, "paste", lambda: clipboard["text"])
    monkeypatch.setattr(inject.pyperclip, "copy", lambda text: clipboard.update(text=text))
    monkeypatch.setattr(inject.time, "sleep", lambda _seconds: None)

    capture = inject.capture_selection()

    assert capture.text == "selected"
    assert capture.clipboard_before == "old"
    assert clipboard["text"] == "selected"


def test_capture_selection_returns_empty_when_clipboard_did_not_change(monkeypatch):
    clipboard = {"text": "same"}
    monkeypatch.setattr(inject, "current", lambda: Mock(copy_selection=Mock()))
    monkeypatch.setattr(inject.pyperclip, "paste", lambda: clipboard["text"])
    monkeypatch.setattr(inject.pyperclip, "copy", lambda text: clipboard.update(text=text))
    monkeypatch.setattr(inject.time, "sleep", lambda _seconds: None)

    capture = inject.capture_selection()

    assert capture.text == ""
    assert capture.clipboard_before == "same"
    assert clipboard["text"] == "same"


def test_capture_selection_detects_selection_equal_to_previous_clipboard(monkeypatch):
    clipboard = {"text": "selected"}

    monkeypatch.setattr(
        inject, "current", lambda: Mock(copy_selection=lambda: clipboard.update(text="selected"))
    )
    monkeypatch.setattr(inject.pyperclip, "paste", lambda: clipboard["text"])
    monkeypatch.setattr(inject.pyperclip, "copy", lambda text: clipboard.update(text=text))
    monkeypatch.setattr(inject.time, "sleep", lambda _seconds: None)

    capture = inject.capture_selection()

    assert capture.text == "selected"
    assert capture.clipboard_before == "selected"


@pytest.mark.parametrize(
    ("current_text", "expected"),
    [("selection", "restored"), ("other", "other")],
)
def test_restore_clipboard_only_when_selection_is_still_present(
    monkeypatch, current_text, expected
):
    clipboard = {"text": current_text}
    monkeypatch.setattr(inject.pyperclip, "paste", lambda: clipboard["text"])
    monkeypatch.setattr(inject.pyperclip, "copy", lambda text: clipboard.update(text=text))

    inject.restore_clipboard_if_matches("selection", "restored")

    assert clipboard["text"] == expected
