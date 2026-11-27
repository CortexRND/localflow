"""Hot-command rule CRUD over `HotCommandStore` (server, and CLI when it is down)."""

from __future__ import annotations

from collections.abc import Callable, Iterable

from localflow.agents.hotcommands import (
    MAX_RULES,
    RuleConflict,
    RuleError,
    check_conflicts,
    parse_repos,
    validate_rule,
)
from localflow.agents.models import (
    HotCommandConflict,
    HotCommandRule,
    NotFound,
    ValidationError,
    new_short_id,
    utc_now,
)
from localflow.agents.store import HotCommandStore

DEFAULTS = {"profile": "", "repo": "", "prompt_template": "{prompt}", "min_prompt_words": 3,
            "enabled": True}


def hot_phrase_triggers() -> list[str]:
    """Enabled hot-phrase triggers (rules must never overlap them)."""
    from localflow.hotphrases import HotPhraseStore

    return [trigger for trigger, _text in HotPhraseStore().enabled()]


def _check_refs(rule: dict, config, known_provider: Callable[[str], bool]) -> None:
    if not known_provider(rule["provider"]):
        raise ValidationError(f"unknown provider {rule['provider']!r}")
    if rule["provider"] == "http" and not rule.get("profile"):
        raise ValidationError("an http rule needs a profile")
    if rule.get("repo") and rule["repo"] not in parse_repos(config.agent_repos):
        raise ValidationError(f"unknown repo {rule['repo']!r}")


def add_rule(store: HotCommandStore, body: dict, config, *,
             known_provider: Callable[[str], bool],
             phrases: Iterable[str] | None = None) -> dict:
    try:
        cleaned = validate_rule(body)
    except RuleError as exc:
        raise ValidationError(str(exc)) from None
    rule = {**DEFAULTS, **cleaned}
    _check_refs(rule, config, known_provider)
    phrases = list(phrases if phrases is not None else hot_phrase_triggers())
    with store.mutate() as rules:
        if len(rules) >= MAX_RULES:
            raise ValidationError(f"at most {MAX_RULES} hot commands")
        try:
            check_conflicts(rule["trigger"], rules, phrases)
        except RuleConflict as exc:
            raise HotCommandConflict(str(exc)) from None
        now = utc_now()
        entry = HotCommandRule(id=new_short_id(), created_at=now, updated_at=now,
                               **rule).to_dict()
        rules.append(entry)
    return entry


def update_rule(store: HotCommandStore, rule_id: str, body: dict, config, *,
                known_provider: Callable[[str], bool],
                phrases: Iterable[str] | None = None) -> dict:
    try:
        cleaned = validate_rule(body, partial=True)
    except RuleError as exc:
        raise ValidationError(str(exc)) from None
    phrases = list(phrases if phrases is not None else hot_phrase_triggers())
    with store.mutate() as rules:
        entry = next((r for r in rules if r.get("id") == rule_id), None)
        if entry is None:
            raise NotFound(f"no hot command {rule_id}")
        merged = {**entry, **cleaned}
        _check_refs(merged, config, known_provider)
        if "trigger" in cleaned:
            try:
                check_conflicts(merged["trigger"], rules, phrases, exclude_id=rule_id)
            except RuleConflict as exc:
                raise HotCommandConflict(str(exc)) from None
        entry.update(cleaned)
        entry["updated_at"] = utc_now()
        result = dict(entry)
    return result


def delete_rule(store: HotCommandStore, rule_id: str) -> None:
    with store.mutate() as rules:
        remaining = [r for r in rules if r.get("id") != rule_id]
        if len(remaining) == len(rules):
            raise NotFound(f"no hot command {rule_id}")
        rules[:] = remaining
