import pytest

from localflow.commands import discover_commands
from localflow.symbols import apply_spoken_symbols, normalize_commands

REGISTRY = ["skill_part", "candidate-solutioning"]


@pytest.mark.parametrize(
    "spoken, expected",
    [
        ("slash skill part", "/skill_part"),
        ("Slash skill part.", "/skill_part"),
        ("slash candidate solutioning", "/candidate-solutioning"),
        ("Slash candidate solutioning.", "/candidate-solutioning"),
        ("/candidate-solutioning", "/candidate-solutioning"),
        ("/skill_part", "/skill_part"),
        ("run slash skill part with these arguments", "run /skill_part with these arguments"),
        ("slash unknown words", "/unknown words"),
        ("we discussed candidate solutioning today", "we discussed candidate solutioning today"),
        ("slash skill underscore part", "/skill_part"),
        ("slash candidate dash solutioning", "/candidate-solutioning"),
        ("slash Skill Part, then commit", "/skill_part, then commit"),
        ("slash skill partial", "/skill partial"),
        ("slash skill part two", "/skill_part two"),
        ("config slash settings dash prod", "config /settings-prod"),
    ],
)
def test_known_commands(spoken, expected):
    assert apply_spoken_symbols(spoken, REGISTRY) == expected


def test_no_registry_keeps_legacy_behaviour():
    assert apply_spoken_symbols("slash skill part.") == "/skill part."


def test_overlapping_names_prefer_longest():
    names = ["skill_part", "skill_part_two"]
    assert normalize_commands("/skill part two", names) == "/skill_part_two"
    assert normalize_commands("/skill part", names) == "/skill_part"


def test_punctuation_only_stripped_at_end():
    assert normalize_commands("/skill part. Then more", REGISTRY) == "/skill_part. Then more"
    assert normalize_commands("say /skill part.", REGISTRY) == "say /skill_part"


def test_path_like_text_untouched():
    assert normalize_commands("/usr/skill part", REGISTRY) == "/usr/skill part"
    assert normalize_commands("/skill_part/child", REGISTRY) == "/skill_part/child"


def test_discover_commands(tmp_path):
    (tmp_path / "candidate-solutioning").mkdir()
    (tmp_path / "skill_part").mkdir()
    (tmp_path / ".hidden").mkdir()
    (tmp_path / "bad name").mkdir()
    (tmp_path / "README.md").write_text("x")
    names = discover_commands(["explicit_one", "not valid"], [str(tmp_path), str(tmp_path / "missing")])
    assert names == ["candidate-solutioning", "explicit_one", "skill_part"]
