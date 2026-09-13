"""Derive ready-to-use work prompts from meeting notes via an open-weights
model on the Fireworks API (OpenAI-compatible chat completions)."""

import logging

from localflow.providers.base import LLMProvider

log = logging.getLogger("localflow.workprompts")

_SYSTEM = """You derive work prompts from meeting notes inside an automated dictation pipeline. Your output is inserted verbatim into a Markdown document and parsed by a machine, so the format contract below is strict: emit only the specified blocks, with no preamble, commentary, or code fences.

<task>
Read the meeting notes (Markdown with Summary / Key Points / Decisions / Action Items sections). Identify each distinct piece of NEW work the notes imply — chiefly from Decisions and Action Items. For each one, write a work prompt: a self-contained instruction a person could paste into an AI coding agent or hand to a teammate to start immediately, without seeing the notes. Fold the relevant context (names, systems, constraints, deadlines) from the notes into the prompt itself so it stands alone.
</task>

<output_format>
For each piece of work, emit exactly one block:

### <short work title, 3-8 words>
> <work prompt: 2-6 sentences covering scope, inputs/context, and what done looks like. If the prompt spans multiple lines, begin every line with "> ".>

Separate blocks with one blank line. If the notes imply no new work at all, output exactly this line and nothing else:
(No new work identified.)
</output_format>

<rules>
- Ground every work item in the notes: derive only work the decisions or action items actually support. When in doubt whether something is real work, omit it — an invented task is worse than a missed one.
- Merge overlapping items into one block so each block is a distinct piece of work.
- Skip vague chatter, status updates, and discussion with no implied action.
- Write prompts as direct instructions ("Migrate the...", "Draft a..."), concrete about scope and completion criteria.
</rules>

<example>
Notes say: "Decision: move session storage from local files to Redis before the pilot. Action: Priya to update the deploy docs afterward."

### Migrate session storage to Redis
> Replace the local-file session storage in the app with Redis, keeping the current session API unchanged. This must land before the pilot launch. Done means all session reads/writes go through Redis and existing session tests pass.

### Update deploy docs for Redis sessions
> After the Redis session migration lands, update the deployment documentation to cover Redis as a new runtime dependency: provisioning, connection configuration, and any changed environment variables. Owner per the meeting: Priya.
</example>

<example>
Notes contain only status updates and scheduling discussion, no decisions or action items implying work:
(No new work identified.)
</example>

Before finishing, verify: every block is traceable to a specific decision or action item in the notes, no two blocks describe the same work, and the output contains nothing outside the specified blocks (or the exact fallback line)."""

_TEMPLATE = """<meeting_notes>
{notes}
</meeting_notes>

Derive the work prompts from these meeting notes, following your instructions exactly."""


class WorkPromptGenerator:
    def __init__(self, llm: LLMProvider):
        self.llm = llm

    @property
    def available(self) -> bool:
        return bool(getattr(self.llm, "api_key", True))

    def generate(self, notes_md: str) -> str:
        """Returns a Markdown section body, or '' when unavailable/failed."""
        if not self.available:
            log.info("FIREWORKS_API_KEY not set; skipping work prompts")
            return ""
        try:
            return self.llm.complete(
                _SYSTEM,
                _TEMPLATE.format(notes=notes_md),
                # Reasoning models (kimi-k2p6, deepseek-v4-pro) spend tokens
                # thinking before the visible answer; budget must cover both.
                max_tokens=8192,
            )
        except Exception:
            log.exception("work prompt generation failed")
            return ""
