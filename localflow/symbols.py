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
# Between two parts of a registered name: STT whitespace, an already-typed
# separator, or a spoken one ("skill underscore part").
_PART_SEPARATOR = r"(?:\s*[_-]\s*|\s+(?:" + "|".join(_JOINING) + r")\b[.,]?\s+|\s+)"
_PLACEHOLDER = re.compile("\x00(\\d+)\x00")


def _command_pattern(names: Iterable[str]) -> re.Pattern | None:
    """Match "/" + a registered name (STT output or an already-correct command).
    Longest names are tried first so "skill_part_two" wins over "skill_part"."""
    ordered = sorted({n for n in names if n}, key=len, reverse=True)
    if not ordered:
        return None
    alternatives = [
        _PART_SEPARATOR.join(re.escape(part) for part in _NAME_SPLIT.split(name))
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


def _spoken_key(token: str) -> str:
    """Collapse separators (typed or spoken) and case: "Skill underscore Part"
    and "skill-part" both become "skill part"."""
    words = [w for w in re.split(r"[\s_-]+", token.lower()) if w]
    stripped = [w for w in words if w not in _JOINING]
    return " ".join(stripped or words)


def _canonical_lookup(names: Iterable[str]) -> dict[str, str | None]:
    """spoken key -> registered name, or None when several registered names
    share a key (e.g. skill-part and skill_part): ambiguous speech is left as is."""
    lookup: dict[str, str | None] = {}
    for name in names:
        if not name:
            continue
        key = _spoken_key(name)
        lookup[key] = None if key in lookup and lookup[key] != name else name
    return lookup


def normalize_commands(text: str, names: Sequence[str]) -> str:
    return _normalize(text, names, lambda name: "/" + name)


def _normalize(text: str, names: Sequence[str], render) -> str:
    pattern = _command_pattern(names)
    if pattern is None:
        return text
    exact = set(names)
    lookup = _canonical_lookup(names)

    def repl(m: re.Match) -> str:
        token = m.group(1)
        if token in exact:
            return render(token)
        canonical = lookup.get(_spoken_key(token))
        if canonical is None:
            return m.group(0)
        return render(canonical)

    return pattern.sub(repl, text)


def apply_spoken_symbols(text: str, commands: Sequence[str] = ()) -> str:
    text = _SLASH_PATTERN.sub(lambda m: ("/" if m.group(1) == "" else " /"), text)
    # Snap registered commands before the generic symbol-word pass so a name
    # whose parts are literally "dash"/"underscore" survives it; the canonical
    # form is parked behind a placeholder until that pass is done.
    resolved: list[str] = []

    def park(name: str) -> str:
        resolved.append("/" + name)
        return f"\x00{len(resolved) - 1}\x00"

    if commands:
        text = _normalize(text, commands, park)
    text = _JOIN_PATTERN.sub(lambda m: _JOINING[m.group(1).lower()], text)
    if resolved:
        text = _PLACEHOLDER.sub(lambda m: resolved[int(m.group(1))], text)
    return text
