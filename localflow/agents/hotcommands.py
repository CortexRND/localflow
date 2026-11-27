"""Hot-command rules: validation, conflict checks and the deterministic matcher.

Pure functions (`re` only), shared by the dictation app and
`POST /api/agents/hot-commands/test`, so both always agree.

    utterance := TRIGGER [ ("in" | "on") REPO_ALIAS ] [ "to" ] PROMPT
"""

from __future__ import annotations

import re
from collections.abc import Callable, Iterable, Mapping
from dataclasses import dataclass

MAX_RULES = 32
MIN_TRIGGER_WORDS = 2
MAX_TRIGGER_WORDS = 6
MAX_TRIGGER_CHARS = 60
MAX_TEMPLATE_CHARS = 20000
MAX_MIN_WORDS = 50
PROVIDER_ID_RE = re.compile(r"^[a-z0-9][a-z0-9_.-]{0,39}$")
PROFILE_ID_RE = re.compile(r"^[0-9a-f]{8}$")
REPO_ALIAS_RE = re.compile(r"^[a-z0-9][a-z0-9 _-]{0,39}$")
_WORD = re.compile(r"[^\W_]+")
_RULE_FIELDS = {"trigger", "provider", "profile", "repo", "prompt_template",
                "min_prompt_words", "enabled"}


class RuleError(ValueError):
    """An invalid hot-command rule."""


class RuleConflict(RuleError):
    """A trigger that duplicates or overlaps another trigger."""


def normalize(text: str) -> str:
    """Same rule as hotphrases.normalize_trigger."""
    return " ".join(_WORD.findall(text.lower()))


def _tokens(text: str) -> tuple[str, ...]:
    return tuple(normalize(text).split())


def validate_rule(fields: Mapping, *, partial: bool = False) -> dict:
    """Type/shape checks; returns the cleaned fields. Raises RuleError."""
    if not isinstance(fields, Mapping):
        raise RuleError("expected a JSON object")
    unknown = sorted(set(fields) - _RULE_FIELDS)
    if unknown:
        raise RuleError(f"unknown field(s): {', '.join(unknown)}")
    if not partial:
        for required in ("trigger", "provider"):
            if required not in fields:
                raise RuleError(f"{required} is required")
    types = {"trigger": str, "provider": str, "profile": str, "repo": str,
             "prompt_template": str, "min_prompt_words": int, "enabled": bool}
    cleaned: dict = {}
    for name, value in fields.items():
        if type(value) is not types[name]:
            kind = {bool: "boolean", int: "integer", str: "string"}[types[name]]
            raise RuleError(f"{name} must be a {kind}")
        cleaned[name] = value
    if "trigger" in cleaned:
        trigger = cleaned["trigger"].strip()
        if len(trigger) > MAX_TRIGGER_CHARS:
            raise RuleError(f"trigger is too long (max {MAX_TRIGGER_CHARS} characters)")
        words = len(_tokens(trigger))
        if not MIN_TRIGGER_WORDS <= words <= MAX_TRIGGER_WORDS:
            raise RuleError(
                f"trigger needs {MIN_TRIGGER_WORDS} to {MAX_TRIGGER_WORDS} words"
            )
        cleaned["trigger"] = trigger
    if "provider" in cleaned and not PROVIDER_ID_RE.match(cleaned["provider"]):
        raise RuleError("provider is not a valid provider id")
    if cleaned.get("profile") and not PROFILE_ID_RE.match(cleaned["profile"]):
        raise RuleError("profile is not a valid HTTP profile id")
    if "prompt_template" in cleaned:
        template = cleaned["prompt_template"]
        if not template.strip():
            raise RuleError("prompt_template is empty")
        if len(template) > MAX_TEMPLATE_CHARS:
            raise RuleError(f"prompt_template is too long (max {MAX_TEMPLATE_CHARS})")
    if "min_prompt_words" in cleaned and not 0 <= cleaned["min_prompt_words"] <= MAX_MIN_WORDS:
        raise RuleError(f"min_prompt_words must be 0 to {MAX_MIN_WORDS}")
    return cleaned


def _overlaps(a: tuple[str, ...], b: tuple[str, ...]) -> bool:
    shorter, longer = (a, b) if len(a) <= len(b) else (b, a)
    return longer[: len(shorter)] == shorter


def check_conflicts(trigger: str, rules: Iterable[Mapping], hot_phrase_triggers: Iterable[str],
                    *, exclude_id: str | None = None) -> None:
    """Refuse duplicates/token-prefix overlaps with other rules or hot phrases."""
    tokens = _tokens(trigger)
    for rule in rules:
        if rule.get("id") == exclude_id:
            continue
        other = _tokens(str(rule.get("trigger", "")))
        if other and _overlaps(tokens, other):
            raise RuleConflict(f"trigger overlaps the hot command {rule.get('trigger')!r}")
    for phrase in hot_phrase_triggers:
        other = _tokens(phrase)
        if other and _overlaps(tokens, other):
            raise RuleConflict(f"trigger overlaps the hot phrase {phrase!r}")


def parse_repos(entries: Iterable[str]) -> dict[str, str]:
    """`alias=/abs/path` entries -> {alias: path}; invalid entries are skipped."""
    repos: dict[str, str] = {}
    for entry in entries:
        if not isinstance(entry, str) or "=" not in entry:
            continue
        alias, path = entry.split("=", 1)
        alias, path = alias.strip(), path.strip()
        if REPO_ALIAS_RE.match(alias) and path:
            repos[alias] = path
    return repos


@dataclass(frozen=True)
class HotCommandMatch:
    rule_id: str
    provider: str
    profile: str
    repo: str
    prompt: str


@dataclass(frozen=True)
class HotCommandResult:
    matched: bool
    reason: str = ""
    rule_id: str = ""
    provider: str = ""
    profile: str = ""
    repo: str = ""
    prompt: str = ""

    def to_dict(self) -> dict:
        return {"matched": self.matched, "rule_id": self.rule_id, "provider": self.provider,
                "profile": self.profile, "repo": self.repo, "prompt": self.prompt,
                "reason": self.reason}

    def as_match(self) -> HotCommandMatch | None:
        if not self.matched:
            return None
        return HotCommandMatch(self.rule_id, self.provider, self.profile, self.repo,
                               self.prompt)


@dataclass(frozen=True)
class CompiledRules:
    rules: tuple[tuple[tuple[str, ...], dict], ...]  # longest trigger first
    aliases: tuple[tuple[tuple[str, ...], str], ...]  # longest alias first
    repos: frozenset[str]
    default_repo: str


def compile_rules(rules: Iterable[Mapping], repos: Mapping[str, str] | Iterable[str],
                  default_repo: str = "") -> CompiledRules:
    if not isinstance(repos, Mapping):
        repos = parse_repos(repos)
    ordered = []
    for rule in rules:
        tokens = _tokens(str(rule.get("trigger", "")))
        if len(tokens) >= MIN_TRIGGER_WORDS:
            ordered.append((tokens, dict(rule)))
    ordered.sort(key=lambda item: (-len(item[0]), -len(" ".join(item[0]))))
    aliases = sorted(((_tokens(alias), alias) for alias in repos if _tokens(alias)),
                     key=lambda item: -len(item[0]))
    return CompiledRules(tuple(ordered), tuple(aliases), frozenset(repos), default_repo)


def evaluate(text: str, compiled: CompiledRules, *,
             transform: Callable[[str], str] | None = None,
             known_provider: Callable[[str], bool] | None = None) -> HotCommandResult:
    """Explain the outcome for `text` (the /hot-commands/test answer)."""
    if not text or not compiled.rules:
        return HotCommandResult(False, "no_trigger")
    tokens = [(m.group(0).lower(), m.start()) for m in _WORD.finditer(text)]
    words = tuple(t for t, _ in tokens)
    for trigger, rule in compiled.rules:
        if words[: len(trigger)] != trigger:
            continue
        if not rule.get("enabled", True):
            return HotCommandResult(False, "disabled", rule_id=str(rule.get("id", "")))
        index = len(trigger)
        repo = ""
        if index < len(words) and words[index] in ("in", "on"):
            for alias_tokens, alias in compiled.aliases:
                if words[index + 1: index + 1 + len(alias_tokens)] == alias_tokens:
                    repo = alias
                    index += 1 + len(alias_tokens)
                    break
        if index < len(words) and words[index] == "to":
            index += 1
        raw = text[tokens[index][1]:] if index < len(tokens) else ""
        prompt = re.sub(r"^[\W_]+", "", raw).strip() if raw else ""
        if prompt and transform is not None:
            prompt = transform(prompt).strip()
        template = str(rule.get("prompt_template") or "{prompt}")
        fixed = "{prompt}" not in template
        min_words = 0 if fixed else int(rule.get("min_prompt_words", 3))
        base = {"rule_id": str(rule.get("id", "")), "provider": str(rule.get("provider", "")),
                "profile": str(rule.get("profile", ""))}
        if not fixed and len(prompt.split()) < max(min_words, 1):
            return HotCommandResult(False, "too_few_words", **base)
        repo = repo or str(rule.get("repo") or "") or compiled.default_repo
        if repo and repo not in compiled.repos:
            return HotCommandResult(False, "unknown_repo", repo=repo, **base)
        if known_provider is not None and not known_provider(base["provider"]):
            return HotCommandResult(False, "unknown_provider", repo=repo, **base)
        final = template if fixed else template.replace("{prompt}", prompt)
        return HotCommandResult(True, "", repo=repo, prompt=final, **base)
    return HotCommandResult(False, "no_trigger")


def match_hot_command(text: str, compiled: CompiledRules, *,
                      transform: Callable[[str], str] | None = None,
                      known_provider: Callable[[str], bool] | None = None
                      ) -> HotCommandMatch | None:
    return evaluate(text, compiled, transform=transform,
                    known_provider=known_provider).as_match()
