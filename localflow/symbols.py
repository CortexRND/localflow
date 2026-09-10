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


_TOKEN_SPLIT = re.compile(r"(\s*[_-]\s*|\s+)")


def _shape(token: str, spoken: bool) -> tuple[str, tuple[str | None, ...]]:
    """(key, separators) of a token. The key collapses separators and case
    ("Skill_Part", "skill-part" -> "skill part"); separators record the
    evidence between parts: "-", "_", or None for plain whitespace. With
    spoken=True a separator word between parts is read as that separator
    ("skill underscore part" -> "skill part", ("_",))."""
    pieces = _TOKEN_SPLIT.split(token.lower())
    words = [w.rstrip(".,") for w in pieces[0::2]]
    seps = [s.strip() or None for s in pieces[1::2]]
    if spoken:
        i = 1
        while i < len(words) - 1:
            if words[i] in _JOINING:
                sep = _JOINING[words[i]]
                del words[i]
                seps[i - 1 : i + 1] = [sep]
            else:
                i += 1
    return " ".join(words), tuple(seps)


def _candidates(names: Iterable[str]) -> dict[str, list[str]]:
    """spoken key -> registered names sharing it (several when names differ
    only by separator, e.g. skill-part and skill_part)."""
    lookup: dict[str, list[str]] = {}
    for name in names:
        if name:
            lookup.setdefault(_shape(name, spoken=False)[0], []).append(name)
    return lookup


def _resolve(token: str, lookup: dict[str, list[str]]) -> str | None:
    """The one registered name matching the token, reading separator words
    literally first ("foo dash bar" -> foo_dash_bar) and as separators second
    ("skill underscore part" -> skill_part). Among names that differ only by
    separator, the token's typed/spoken separators decide; None if ambiguous."""
    for spoken in (False, True):
        key, seps = _shape(token, spoken)
        candidates = lookup.get(key, [])
        if len(candidates) == 1:
            return candidates[0]
        matches = [
            name
            for name in candidates
            if all(s is None or s == n for s, n in zip(seps, _shape(name, False)[1]))
        ]
        if len(matches) == 1:
            return matches[0]
        if candidates:
            return None
    return None


def normalize_commands(text: str, names: Sequence[str]) -> str:
    return _normalize(text, names, lambda name: "/" + name)


def _normalize(text: str, names: Sequence[str], render) -> str:
    pattern = _command_pattern(names)
    if pattern is None:
        return text
    exact = set(names)
    lookup = _candidates(names)

    def repl(m: re.Match) -> str:
        token = m.group(1)
        canonical = token if token in exact else _resolve(token, lookup)
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
