import logging
import re

from localflow.providers.base import LLMProvider

log = logging.getLogger("localflow.transform")

COMMAND_WITH_SELECTION = (
    "You are a writing assistant. Apply the instruction to the selected text. "
    "Return only the resulting text that should replace the selection, with no "
    "commentary, no quotes, and no explanations.\n\nInstruction:\n{instruction}"
    "\n\nSelected text:\n{selection}"
)
COMMAND_NO_SELECTION = (
    "You are a writing assistant. Write the text the instruction asks for. "
    "Return only that text, with no commentary, no quotes, and no explanations."
    "\n\nInstruction:\n{instruction}"
)
POLISH = (
    "You are an editor. Rewrite the text to be clear, concise and well-structured "
    "while keeping the author's meaning, voice and every fact. Fix grammar and "
    "punctuation. Do not add new information. Return only the rewritten text with "
    "no commentary, no quotes, and no explanations.\n\nText:\n{text}"
)
PROMPT_ENGINEER = (
    "You turn rough notes into a clear prompt for an AI assistant. Rewrite the "
    "text as a well-structured prompt: state the goal, the relevant context, the "
    "constraints and the desired output format, using short sections or bullet "
    "points where helpful. Keep every requirement from the original and do not "
    "invent new ones. Return only the prompt with no commentary.\n\nText:\n{text}"
)


class Transformer:
    def __init__(self, llm: LLMProvider):
        self.llm = llm

    def run(self, prompt: str) -> str | None:
        try:
            system, user = prompt.split("\n\n", 1)
            result = self.llm.complete(system, user)
        except Exception:
            log.exception("text transform failed")
            return None
        result = result.strip().strip("\"'").strip()
        result = re.sub(r"^(?:Text|Prompt):\s*", "", result, count=1, flags=re.IGNORECASE)
        result = result.strip().strip("\"'").strip()
        return result or None


def command_prompt(instruction: str, selection: str) -> str:
    if selection:
        return COMMAND_WITH_SELECTION.format(
            instruction=instruction.strip(), selection=selection
        )
    return COMMAND_NO_SELECTION.format(instruction=instruction.strip())


def transform_prompt(preset: str, text: str) -> str:
    templates = {"polish": POLISH, "prompt": PROMPT_ENGINEER}
    try:
        template = templates[preset]
    except KeyError as exc:
        raise ValueError(f"unknown transform preset: {preset}") from exc
    return template.format(text=text)
