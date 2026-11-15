"""Meeting note templates: Markdown files that define the notes' sections.

A template is a ``.md`` file with optional ``key: value`` frontmatter
(``name``, ``description``), optional free-text guidance, then one
``## Header`` per section followed by guidance for that section::

    ---
    name: Standup
    description: Yesterday / today / blockers.
    ---
    Keep it short.

    ## Blockers
    Bulleted list of blockers. Write "None raised." if none.

Built-in templates ship in ``localflow/templates/meetings``. Files in the
user's templates directory (``[meetings] templates_dir``, default
``~/.localflow/meeting_templates``) add templates or replace a built-in with
the same file name. The template id is the lower-cased file stem.
"""

import logging
from dataclasses import dataclass, field
from pathlib import Path

log = logging.getLogger("localflow.meeting_templates")

BUILTIN_DIR = Path(__file__).parent / "templates" / "meetings"
DEFAULT_TEMPLATE_ID = "general"
_MAX_TEMPLATE_BYTES = 64 * 1024


@dataclass(frozen=True)
class Section:
    header: str
    guidance: str = ""


@dataclass(frozen=True)
class MeetingTemplate:
    id: str
    name: str
    description: str = ""
    instructions: str = ""
    sections: tuple[Section, ...] = field(default_factory=tuple)

    def section_names(self) -> list[str]:
        return [s.header for s in self.sections]

    def sections_markdown(self) -> str:
        """The section list as the prompt shows it: header, then guidance."""
        blocks = []
        for section in self.sections:
            block = f"## {section.header}"
            if section.guidance:
                block += f"\n{section.guidance}"
            blocks.append(block)
        return "\n\n".join(blocks)


def parse_template(template_id: str, text: str) -> MeetingTemplate:
    meta: dict[str, str] = {}
    lines = text.replace("\r\n", "\n").split("\n")
    if lines and lines[0].strip() == "---":
        for index in range(1, len(lines)):
            line = lines[index]
            if line.strip() == "---":
                lines = lines[index + 1:]
                break
            key, sep, value = line.partition(":")
            if sep:
                meta[key.strip().lower()] = value.strip()
        else:
            meta = {}  # unterminated frontmatter: treat the whole file as body

    intro: list[str] = []
    sections: list[tuple[str, list[str]]] = []
    for line in lines:
        if line.startswith("## "):
            sections.append((line[3:].strip(), []))
        elif sections:
            sections[-1][1].append(line)
        else:
            intro.append(line)
    return MeetingTemplate(
        id=template_id,
        name=meta.get("name") or template_id,
        description=meta.get("description", ""),
        instructions="\n".join(intro).strip(),
        sections=tuple(
            Section(header, "\n".join(body).strip())
            for header, body in sections
            if header
        ),
    )


def _read_dir(directory: Path) -> dict[str, MeetingTemplate]:
    found: dict[str, MeetingTemplate] = {}
    if not directory.is_dir():
        return found
    for path in sorted(directory.glob("*.md")):
        try:
            if path.stat().st_size > _MAX_TEMPLATE_BYTES:
                log.warning("skipping oversized meeting template %s", path)
                continue
            template = parse_template(path.stem.lower(), path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError) as exc:
            log.warning("skipping unreadable meeting template %s: %s", path, exc)
            continue
        if not template.sections:
            log.warning("skipping meeting template %s: no '## ' sections", path)
            continue
        found[template.id] = template
    return found


def load_templates(user_dir: str | Path | None = None) -> dict[str, MeetingTemplate]:
    """Built-ins overlaid with the user's templates, keyed by id."""
    templates = _read_dir(BUILTIN_DIR)
    if user_dir:
        templates.update(_read_dir(Path(user_dir).expanduser()))
    return templates


def resolve_template(
    template_id: str | None, user_dir: str | Path | None = None
) -> MeetingTemplate:
    """The named template, else the general one; never raises for a bad id."""
    templates = load_templates(user_dir)
    if template_id and template_id in templates:
        return templates[template_id]
    if template_id:
        log.warning("unknown meeting template %r; using %s", template_id, DEFAULT_TEMPLATE_ID)
    return templates.get(DEFAULT_TEMPLATE_ID) or next(iter(templates.values()))
