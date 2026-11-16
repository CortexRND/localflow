"""PR29 review finding 4: shortcut clashes must compare chords, not strings.

`settings_agent.Validator.conflicts` compares config strings exactly, so
`<alt>+<ctrl>+2` is accepted next to the existing `<ctrl>+<alt>+2`.
KeyboardHub would still fire both: pynput's `HotKey` matches the *set* of
pressed keys after `Listener.canonical`, which folds letter case and maps
left/right modifier variants to the generic modifier
(`pynput.keyboard._MODIFIER_KEYS`: alt/alt_l/alt_r -> alt, cmd*, ctrl*,
shift*), and `HotKey.parse` looks `<name>` up case-insensitively. On darwin
`Key.alt_l`/`ctrl_l`/`cmd_l`/`shift_l` are Enum aliases of the generic key
(same vk) and `Key.alt_gr` is `Key.alt_r` (vk 0x3D). The identity used here
must follow those rules and must stay pure: no pynput (and so no
Quartz/AppKit) import.

Hold keys are different: KeyboardHub compares a held key with the raw key
(`key == self._hold_key`), so `alt_l` and `alt_r` holds stay distinct, and a
hold key is never the same shortcut as a combo that merely uses it.
"""

import sys
from dataclasses import replace

import pytest

from localflow import settings_agent as sa
from localflow.config import Config

DEFAULTS = replace(
    Config(),
    hotkey="alt_l",
    command_hotkey="alt_r",
    polish_hotkey="<ctrl>+<alt>+1",
    prompt_hotkey="<ctrl>+<alt>+2",
    paste_last_hotkey="<ctrl>+<shift>+v",
)


@pytest.fixture
def validator(monkeypatch):
    # Importing pynput here would fail the test: the check must stay pure.
    monkeypatch.setitem(sys.modules, "pynput", None)
    monkeypatch.setitem(sys.modules, "pynput.keyboard", None)
    return sa.Validator("darwin", templates=lambda _config: ())


def clash_keys(validator, config, changed):
    return {key for key, _reason in validator.conflicts(config, changed)}


def test_defaults_do_not_clash(validator):
    assert validator.conflicts(DEFAULTS, sa._SHORTCUT_KEYS) == []


def test_swapped_modifier_order_clashes_with_an_existing_shortcut(validator):
    value = validator.value("polish_hotkey", "alt+ctrl+2", DEFAULTS)
    assert value == "<alt>+<ctrl>+2"  # normalisation keeps the typed order
    config = replace(DEFAULTS, polish_hotkey=value)

    problems = validator.conflicts(config, ["polish_hotkey"])

    assert [key for key, _ in problems] == ["polish_hotkey"]
    assert "already the" in problems[0][1] and "Prompt" in problems[0][1]


def test_swapped_order_clashes_within_one_batch(validator):
    config = replace(
        DEFAULTS, polish_hotkey="<ctrl>+<alt>+7", prompt_hotkey="<alt>+<ctrl>+7",
    )
    assert clash_keys(validator, config, ["polish_hotkey", "prompt_hotkey"]) == {
        "polish_hotkey", "prompt_hotkey",
    }


def test_batch_through_the_agent_path_refuses_the_reordered_clash(validator):
    """The same batch as the agent builds it: validated values, then merged."""
    raw = {"polish_hotkey": "ctrl+alt+7", "prompt_hotkey": "alt+ctrl+7"}
    checked = {k: validator.value(k, v, DEFAULTS) for k, v in raw.items()}
    merged = replace(DEFAULTS, **checked)
    assert clash_keys(validator, merged, list(checked)) == set(raw)


@pytest.mark.parametrize("existing", [
    "<CTRL>+<ALT>+2",    # HotKey.parse looks names up with p.lower()
    "<ctrl>+<ALT>+2",
    "<alt>+<ctrl>+2",
    "<ctrl_l>+<alt>+2",  # darwin: Key.ctrl_l is Key.ctrl (same vk 0x3B)
    "<ctrl>+<alt_l>+2",  # darwin: Key.alt_l is Key.alt (same vk 0x3A)
])
def test_preexisting_equivalent_spellings_clash(validator, existing):
    """A hand-edited config may hold another spelling pynput still accepts
    for the same chord. (Spellings pynput rejects, e.g. `<option>` or extra
    spaces, are disabled by KeyboardHub and so are not live clashes.)"""
    config = replace(DEFAULTS, prompt_hotkey=existing, polish_hotkey="<ctrl>+<alt>+2")
    assert clash_keys(validator, config, ["polish_hotkey"]) == {"polish_hotkey"}


def test_letter_case_is_the_same_key(validator):
    config = replace(DEFAULTS, polish_hotkey="<ctrl>+<alt>+K", prompt_hotkey="<ctrl>+<alt>+k")
    assert clash_keys(validator, config, ["polish_hotkey"]) == {"polish_hotkey"}


def test_dictation_toggle_combo_clashes_in_any_order(validator):
    config = replace(DEFAULTS, hotkey="<cmd>+<shift>+d", polish_hotkey="<shift>+<cmd>+d")
    assert clash_keys(validator, config, ["polish_hotkey"]) == {"polish_hotkey"}


@pytest.mark.parametrize("left,right", [
    ("<ctrl>+<alt>+1", "<ctrl>+<alt>+<shift>+1"),   # extra modifier
    ("<ctrl>+<alt>+1", "<cmd>+<alt>+1"),            # different modifier
    ("<ctrl>+<alt>+1", "<ctrl>+<alt>+<f1>"),         # '1' is not F1
    ("<ctrl>+<alt>+1", "<ctrl>+<alt>+2"),
    ("<ctrl>+<alt>+<up>", "<ctrl>+<alt>+<down>"),
])
def test_distinct_chords_are_still_allowed(validator, left, right):
    config = replace(DEFAULTS, polish_hotkey=left, prompt_hotkey=right)
    assert validator.conflicts(config, ["polish_hotkey", "prompt_hotkey"]) == []


def test_identical_hold_keys_still_clash(validator):
    config = replace(DEFAULTS, hotkey="alt_r", command_hotkey="alt_r")
    assert clash_keys(validator, config, ["command_hotkey"]) == {"command_hotkey"}


@pytest.mark.parametrize("platform_name", ["darwin", "linux", "win32"])
@pytest.mark.parametrize("left,right", [("alt_l", "alt_r"), ("ctrl_l", "ctrl_r"),
                                        ("cmd_l", "cmd_r"), ("shift_l", "shift_r")])
def test_left_and_right_hold_keys_stay_distinct(monkeypatch, platform_name, left, right):
    """Holds compare raw keys in KeyboardHub; sides are different vks everywhere."""
    monkeypatch.setitem(sys.modules, "pynput", None)
    validator = sa.Validator(platform_name, templates=lambda _config: ())
    config = replace(DEFAULTS, hotkey=left, command_hotkey=right)
    assert validator.conflicts(config, ["hotkey", "command_hotkey"]) == []


@pytest.mark.parametrize("hold,same", [
    ("alt", "alt_l"),    # _darwin.Key: both KeyCode.from_vk(0x3A)
    ("ctrl", "ctrl_l"),  # both 0x3B
    ("cmd", "cmd_l"),    # both 0x37
    ("alt_r", "alt_gr"),  # both 0x3D: one physical Right Option key
])
def test_darwin_hold_names_for_one_physical_key_clash(validator, hold, same):
    config = replace(DEFAULTS, hotkey=hold, command_hotkey=same)
    assert clash_keys(validator, config, ["command_hotkey"]) == {"command_hotkey"}


@pytest.mark.parametrize("hold,combo", [
    ("ctrl_r", "<ctrl>+<alt>+1"),
    ("alt_r", "<alt>+<ctrl>+1"),
    ("f13", "<ctrl>+<f13>"),
])
def test_a_hold_key_is_not_the_same_shortcut_as_a_combo_using_it(validator, hold, combo):
    config = replace(DEFAULTS, command_hotkey=hold, polish_hotkey=combo)
    assert validator.conflicts(config, ["command_hotkey", "polish_hotkey"]) == []


def test_off_shortcuts_never_clash(validator):
    config = replace(DEFAULTS, command_hotkey="", polish_hotkey="", prompt_hotkey="")
    assert validator.conflicts(config, sa._SHORTCUT_KEYS) == []


def test_unparseable_preexisting_value_does_not_crash(validator):
    config = replace(DEFAULTS, prompt_hotkey="<ctrl>++", polish_hotkey="<ctrl>+<alt>+1")
    assert validator.conflicts(config, ["polish_hotkey"]) == []
