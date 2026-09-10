"""Registry of slash-command names dictation may produce.

Names come from two places: `command_names` in ~/.localflow.toml, and the
subdirectory names of each `skills_dirs` entry (agent skill layouts such as
~/.claude/skills/<name>/SKILL.md). Only these exact spellings are ever
produced by normalization; nothing is inferred from the words themselves.
"""

import re
from pathlib import Path
from typing import Iterable

_VALID_NAME = re.compile(r"^[A-Za-z0-9]+(?:[-_][A-Za-z0-9]+)*$")


def discover_commands(names: Iterable[str] = (), skills_dirs: Iterable[str] = ()) -> list[str]:
    """Return the sorted, de-duplicated set of registered command names."""
    found: set[str] = set()
    for name in names:
        if _VALID_NAME.match(name):
            found.add(name)
    for entry in skills_dirs:
        root = Path(entry).expanduser()
        if not root.is_dir():
            continue
        for child in root.iterdir():
            if child.is_dir() and not child.name.startswith(".") and _VALID_NAME.match(child.name):
                found.add(child.name)
    return sorted(found)
