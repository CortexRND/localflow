"""Convert spoken symbol names in dictated text to the symbols themselves.

"config slash settings dash prod underscore v2" -> "config/settings-prod_v2"
Dictation-only: meeting transcripts keep natural speech untouched.

With a registry of known command names, the words after a dictated slash are
also snapped to a registered spelling: "slash skill part" -> "/skill_part"
when `skill_part` is registered. Only exact word-sequence matches count; the
registered separator (- or _) is used verbatim and unknown words are left alone.
"""

import re
from typing import Iterable, Sequence

# Glue both sides: "warm dash cache" -> "warm-cache"
_JOINING = {
    "dash": "-",
    "hyphen": "-",
    "underscore": "_",
}

# Word possibly wrapped in whisper punctuation, e.g. " dash," or "Dash."
_JOIN_PATTERN = re.compile(
    r"\s*\b(" + "|".join(_JOINING) + r")\b[.,]?\s*",
    re.IGNORECASE,
)

# Slash binds rightward only, so dictated slash-commands keep the space
# before them: "run slash skill" -> "run /skill". A slash directly after
# another symbol or start of text gets no leading space: "slash home" -> "/home".
_SLASH_PATTERN = re.compile(r"(^|\s+)slash\b[.,]?\s*", re.IGNORECASE)

_NAME_SPLIT = re.compile(r"[-_]")


def _command_pattern(names: Iterable[str]) -> re.Pattern | None:
    """Match "/" + a registered name whose parts may be separated by spaces,
    "-" or "_" (STT output or an already-correct command). Longest names are
    tried first so "skill_part_two" wins over "skill_part"."""
    ordered = sorted({n for n in names if n}, key=len, reverse=True)
    if not ordered:
        return None
    alternatives = [
        r"[\s_-]+".join(re.escape(part) for part in _NAME_SPLIT.split(name))
        for name in ordered
    ]
    # Only a command-position slash (start of text or after whitespace) is
    # considered, so path segments like /usr/skill part are left alone.
    # Trailing [.,!?] is only consumed when nothing but whitespace follows,
    # i.e. STT sentence punctuation on a standalone command.
    return re.compile(
        r"(?<!\S)/(" + "|".join(alternatives) + r")(?![\w-])([.,!?]+(?=\s*$))?",
        re.IGNORECASE,
    )


def _canonical_lookup(names: Iterable[str]) -> dict[str, str]:
    return {" ".join(_NAME_SPLIT.split(n)).lower(): n for n in names if n}


def normalize_commands(text: str, names: Sequence[str]) -> str:
    pattern = _command_pattern(names)
    if pattern is None:
        return text
    lookup = _canonical_lookup(names)

    def repl(m: re.Match) -> str:
        key = re.sub(r"[\s_-]+", " ", m.group(1)).lower()
        return "/" + lookup.get(key, m.group(1))

    return pattern.sub(repl, text)


def apply_spoken_symbols(text: str, commands: Sequence[str] = ()) -> str:
    text = _JOIN_PATTERN.sub(lambda m: _JOINING[m.group(1).lower()], text)
    text = _SLASH_PATTERN.sub(lambda m: ("/" if m.group(1) == "" else " /"), text)
    if commands:
        text = normalize_commands(text, commands)
    return text
