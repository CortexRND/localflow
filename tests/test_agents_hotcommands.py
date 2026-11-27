import time

import pytest

from localflow.agents import hotcommands as hc
from localflow.agents import rules as rule_ops
from localflow.agents.models import HotCommandConflict, ValidationError
from localflow.agents.store import HotCommandStore

REPOS = ["localflow=/r/localflow", "web app=/r/web", "infra=/r/infra"]
RULES = [
    {"id": "r1", "trigger": "launch claude", "provider": "claude", "repo": "",
     "prompt_template": "{prompt}", "min_prompt_words": 3, "enabled": True},
    {"id": "r2", "trigger": "ask devin please", "provider": "devin", "repo": "infra",
     "prompt_template": "Task: {prompt}", "min_prompt_words": 2, "enabled": True},
    {"id": "r3", "trigger": "ship the release", "provider": "claude", "repo": "localflow",
     "prompt_template": "Run the release checklist.", "min_prompt_words": 3, "enabled": True},
    {"id": "r4", "trigger": "wake the robot", "provider": "claude", "repo": "",
     "prompt_template": "{prompt}", "min_prompt_words": 3, "enabled": False},
    {"id": "r5", "trigger": "start ghost agent", "provider": "ghost", "repo": "",
     "prompt_template": "{prompt}", "min_prompt_words": 1, "enabled": True},
    {"id": "r6", "trigger": "run the gone", "provider": "claude", "repo": "deleted",
     "prompt_template": "{prompt}", "min_prompt_words": 1, "enabled": True},
]
KNOWN = {"claude", "devin", "http", "mock"}.__contains__
COMPILED = hc.compile_rules(RULES, REPOS, default_repo="localflow")

POSITIVES = [
    ("Launch Claude in localflow to fix the flaky hot phrase test",
     ("r1", "claude", "localflow", "fix the flaky hot phrase test")),
    ("launch claude on web app to update the landing page copy",
     ("r1", "claude", "web app", "update the landing page copy")),
    ("Launch, Claude: refactor the store module please",
     ("r1", "claude", "localflow", "refactor the store module please")),
    ("launch claude to write more unit tests",
     ("r1", "claude", "localflow", "write more unit tests")),
    ("launch claude in the garden plant some tomatoes",
     ("r1", "claude", "localflow", "in the garden plant some tomatoes")),
    ("launch claude in infra rotate the staging certificates",
     ("r1", "claude", "infra", "rotate the staging certificates")),
    ("Ask Devin please check the nightly build",
     ("r2", "devin", "infra", "Task: check the nightly build")),
    ("ask devin please in localflow bump the version",
     ("r2", "devin", "localflow", "Task: bump the version")),
    ("Ship the release.", ("r3", "claude", "localflow", "Run the release checklist.")),
    ("ship the release now with extra words", ("r3", "claude", "localflow",
                                               "Run the release checklist.")),
    ("launch claude to to double to", ("r1", "claude", "localflow", "to double to")),
    ("LAUNCH CLAUDE IN LOCALFLOW TO FIX IT NOW", ("r1", "claude", "localflow", "FIX IT NOW")),
]

NEGATIVES = [
    ("please launch claude to fix the tests", "no_trigger"),       # mid-sentence
    ("I said launch claude in localflow to fix it", "no_trigger"),
    ("launching claude to fix the tests", "no_trigger"),           # partial word
    ("launch claudette to fix the tests", "no_trigger"),
    ("launch claude", "too_few_words"),
    ("launch claude to fix it", "too_few_words"),
    ("launch claude in localflow to go", "too_few_words"),
    ("wake the robot and do the dishes now", "disabled"),
    ("start ghost agent do something", "unknown_provider"),
    ("run the gone do it", "unknown_repo"),
    ("", "no_trigger"),
    ("launch", "no_trigger"),
]


@pytest.mark.parametrize("text,expected", POSITIVES)
def test_positive_utterances(text, expected):
    match = hc.match_hot_command(text, COMPILED, known_provider=KNOWN)
    assert match is not None, text
    assert (match.rule_id, match.provider, match.repo, match.prompt) == expected


@pytest.mark.parametrize("text,reason", NEGATIVES)
def test_negative_utterances_fall_through(text, reason):
    result = hc.evaluate(text, COMPILED, known_provider=KNOWN)
    assert not result.matched and result.reason == reason
    assert hc.match_hot_command(text, COMPILED, known_provider=KNOWN) is None


def test_transform_runs_on_the_prompt_slice_only():
    seen = []

    def transform(text):
        seen.append(text)
        return text.replace("comma", ",")

    match = hc.match_hot_command("launch claude to fix a comma b and c", COMPILED,
                                 transform=transform)
    assert seen == ["fix a comma b and c"] and match.prompt == "fix a , b and c"


def test_rules_without_any_rule_never_match():
    empty = hc.compile_rules([], REPOS)
    assert hc.evaluate("launch claude to do the thing", empty).reason == "no_trigger"


@pytest.mark.parametrize("body,message", [
    ({"trigger": "launch", "provider": "claude"}, "2 to 6 words"),
    ({"trigger": "a b c d e f g", "provider": "claude"}, "2 to 6 words"),
    ({"trigger": "x" * 30 + " " + "y" * 31, "provider": "claude"}, "too long"),
    ({"trigger": "launch claude", "provider": "Claude Code!"}, "provider"),
    ({"trigger": "launch claude"}, "provider is required"),
    ({"trigger": "launch claude", "provider": "claude", "min_prompt_words": 99}, "0 to 50"),
    ({"trigger": "launch claude", "provider": "claude", "enabled": "yes"}, "boolean"),
    ({"trigger": "launch claude", "provider": "claude", "extra": 1}, "unknown field"),
    ({"trigger": "launch claude", "provider": "claude", "prompt_template": "  "}, "empty"),
])
def test_validate_rule_rejects(body, message):
    with pytest.raises(hc.RuleError, match=message):
        hc.validate_rule(body)


def test_conflicts_with_rules_and_hot_phrases():
    rules = [{"id": "a", "trigger": "launch claude"}]
    with pytest.raises(hc.RuleConflict):
        hc.check_conflicts("Launch, Claude!", rules, [])
    with pytest.raises(hc.RuleConflict):  # token prefix either way
        hc.check_conflicts("launch claude now", rules, [])
    with pytest.raises(hc.RuleConflict):
        hc.check_conflicts("review checklist", [], ["review checklist please"])
    with pytest.raises(hc.RuleConflict):
        hc.check_conflicts("review checklist full", [], ["review checklist"])
    hc.check_conflicts("launch devin", rules, ["review checklist"])
    hc.check_conflicts("launch claude", rules, [], exclude_id="a")


def test_rule_crud_through_the_store(tmp_path):
    from localflow.config import Config

    config = Config(agent_repos=["proj=/tmp/proj"])
    store = HotCommandStore(tmp_path / "hot_commands.json")
    rule = rule_ops.add_rule(store, {"trigger": "launch claude", "provider": "claude",
                                     "repo": "proj"}, config, known_provider=KNOWN, phrases=[])
    assert rule["min_prompt_words"] == 3 and rule["prompt_template"] == "{prompt}"
    with pytest.raises(HotCommandConflict):
        rule_ops.add_rule(store, {"trigger": "launch  claude", "provider": "devin"}, config,
                          known_provider=KNOWN, phrases=[])
    with pytest.raises(HotCommandConflict):
        rule_ops.add_rule(store, {"trigger": "review list", "provider": "devin"}, config,
                          known_provider=KNOWN, phrases=["review list"])
    with pytest.raises(ValidationError, match="unknown repo"):
        rule_ops.add_rule(store, {"trigger": "ask devin", "provider": "devin", "repo": "nope"},
                          config, known_provider=KNOWN, phrases=[])
    with pytest.raises(ValidationError, match="unknown provider"):
        rule_ops.add_rule(store, {"trigger": "ask ghost", "provider": "ghost"}, config,
                          known_provider=KNOWN, phrases=[])
    updated = rule_ops.update_rule(store, rule["id"], {"enabled": False}, config,
                                   known_provider=KNOWN, phrases=[])
    assert updated["enabled"] is False
    rule_ops.delete_rule(store, rule["id"])
    assert store.list() == []


def test_at_most_32_rules(tmp_path):
    from localflow.config import Config

    store = HotCommandStore(tmp_path / "hot_commands.json")
    words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]
    for i in range(hc.MAX_RULES):
        rule_ops.add_rule(store, {"trigger": f"{words[i % 8]} {words[i // 8]} go",
                                  "provider": "claude"}, Config(), known_provider=KNOWN,
                          phrases=[])
    with pytest.raises(ValidationError, match="at most 32"):
        rule_ops.add_rule(store, {"trigger": "zulu zulu", "provider": "claude"}, Config(),
                          known_provider=KNOWN, phrases=[])


def test_matching_is_fast_with_32_rules():
    words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]
    rules = [{"id": str(i), "trigger": f"{words[i % 8]} {words[i // 8]} go",
              "provider": "claude", "min_prompt_words": 3, "enabled": True}
             for i in range(32)]
    compiled = hc.compile_rules(rules, REPOS)
    utterances = [f"{words[i % 8]} {words[(i // 8) % 4]} go in localflow to do task {i} now"
                  if i % 2 else f"this is a normal sentence number {i} with words" for i in
                  range(1000)]
    timings = []
    for text in utterances:
        started = time.perf_counter()
        hc.evaluate(text, compiled)
        timings.append(time.perf_counter() - started)
    timings.sort()
    assert timings[int(len(timings) * 0.95)] < 0.005
