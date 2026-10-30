"""Convert spoken formatting commands and apply deterministic writing styles."""

from __future__ import annotations

import re
from collections.abc import Sequence

_BREAK = re.compile(
    r"[ \t]*[,;]*[ \t]*(?<!\w)(?P<command>new paragraph|new line)\b[.,!?;:]*[ \t]*",
    re.IGNORECASE,
)
_DETERMINERS = {
    "a", "an", "the", "my", "your", "our", "his", "her", "their", "its",
    "this", "that", "any", "one", "every", "each", "no", "another",
}
_PUNCTUATION = {
    "full stop": ".",
    "period": ".",
    "question mark": "?",
    "exclamation point": "!",
    "exclamation mark": "!",
    "comma": ",",
    "semicolon": ";",
    "colon": ":",
}
_SPOKEN_PUNCTUATION = re.compile(
    r"[ \t]*,?[ \t]*(?<!\w)(?P<command>"
    + "|".join(
        re.escape(item)
        for item in sorted(_PUNCTUATION, key=len, reverse=True)
    )
    + r")\b(?=(?:[ \t]*[.,!?])|[\r\n]|[ \t]*$)[ \t]*[.,!?]*",
    re.IGNORECASE,
)
_SCRATCH = re.compile(r"\bscratch\s+that\b[.,!?;:]*", re.IGNORECASE)
_CAPITALIZE = re.compile(r"([.?!]|\n+)([ \t]*)([a-z])")
_STYLE_NAMES = {"formal", "casual", "very_casual", "excited"}


def apply_spoken_formatting(text: str) -> str:
    def replace_break(match: re.Match) -> str:
        return "\n\n" if match.group("command").casefold() == "new paragraph" else "\n"

    def replace_break_if_command(match: re.Match) -> str:
        prefix = text[: match.start("command")].rstrip(" \t,;")
        previous = re.search(r"(\w+)$", prefix)
        following = re.match(r"\s*(\w+)", text[match.end() :])
        if previous and previous.group(1).casefold() in _DETERMINERS:
            return match.group()
        if following and following.group(1).casefold() == "of":
            return match.group()
        return replace_break(match)

    text = _BREAK.sub(replace_break_if_command, text)
    text = _SPOKEN_PUNCTUATION.sub(
        lambda match: _PUNCTUATION[match.group("command").casefold()],
        text,
    )
    for match in reversed(list(_SCRATCH.finditer(text))):
        prefix = text[: match.start()].rstrip(" \t,.!?;:")
        suffix = text[match.end() :]
        before = prefix.rstrip(" \t")
        after = suffix
        prior_boundary = not before or before[-1] in ".,!?;:\n"
        next_boundary = (
            not after
            or match.group().endswith(tuple(".,!?;:"))
            or after[0] in ".,!?;:\n"
        )
        if not prior_boundary and not next_boundary:
            continue
        boundary = max(prefix.rfind(mark) for mark in ".?!\n")
        if suffix.startswith("\n") and (boundary < 0 or text[boundary] == "\n"):
            suffix = suffix.lstrip(" \t")
            suffix = suffix.removeprefix("\n")
        text = text[: boundary + 1] + suffix
    text = text.strip(" \t")
    return _CAPITALIZE.sub(
        lambda match: match.group(1) + match.group(2) + match.group(3).upper(),
        text,
    )


def apply_style(
    text: str,
    style: str,
    vocabulary: Sequence[str] = (),
) -> str:
    if style not in _STYLE_NAMES:
        return text
    if style in {"casual", "very_casual"}:
        trailing_period = re.search(r"\.(\s*)$", text)
        if trailing_period and (
            style == "very_casual"
            or (
                "\n" not in text
                and not re.search(r"[.!?]", text[: trailing_period.start()])
            )
        ):
            text = text[: trailing_period.start()] + trailing_period.group(1)
    if style == "very_casual":
        first = re.search(r"\w+", text)
        protected = {item.strip().casefold() for item in vocabulary if item.strip()}
        if (
            first
            and first.group().casefold() != "i"
            and not first.group().isupper()
            and first.group().casefold() not in protected
        ):
            text = (
                text[: first.start()]
                + first.group()[0].lower()
                + text[first.start() + 1 :]
            )
    return text
