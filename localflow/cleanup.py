from localflow.providers.base import LLMProvider

_SYSTEM_PROMPT = (
    "You are a strict text cleanup tool. Fix punctuation and casing, and remove "
    "filler words (um, uh). Do NOT change wording or meaning, do NOT add or remove "
    "any information, do NOT rephrase. Return only the cleaned text with no "
    "commentary, no quotes, and no explanations."
)


class Cleaner:
    def __init__(self, llm: LLMProvider):
        self.llm = llm

    def clean(self, text: str) -> str:
        try:
            cleaned = self.llm.complete(_SYSTEM_PROMPT, f"Text:\n{text}")
            if not cleaned:
                return text
            return cleaned
        except Exception:  # noqa: BLE001
            return text
