"""Selector hygiene for phrases_mac.py — runs on Linux via ast, no AppKit.

PyObjC exports every method on an NSObject subclass to Objective-C; a
method name whose underscores don't match its argument count produces a
BadPrototypeError at import. Anything that isn't meant to be a Cocoa
selector must be @objc.python_method.
"""

import ast
from pathlib import Path

SRC = Path(__file__).parent.parent / "localflow" / "phrases_mac.py"


def _methods():
    tree = ast.parse(SRC.read_text())
    cls = next(
        n for n in tree.body
        if isinstance(n, ast.ClassDef) and n.name == "PhrasesWindowController"
    )
    return [n for n in cls.body if isinstance(n, ast.FunctionDef)]


def _is_python_method(fn):
    for dec in fn.decorator_list:
        if (
            isinstance(dec, ast.Attribute)
            and isinstance(dec.value, ast.Name)
            and dec.value.id == "objc"
            and dec.attr == "python_method"
        ):
            return True
    return False


def test_non_python_methods_are_valid_selectors():
    bad = []
    for fn in _methods():
        if _is_python_method(fn):
            continue
        name = fn.name
        args = len(fn.args.args) + len(fn.args.kwonlyargs) - 1  # minus self
        if name.startswith("_"):
            bad.append(f"{name}: private method exported as a selector")
        elif name.count("_") != args:
            bad.append(f"{name}: {name.count('_')} underscores but {args} args")
    assert not bad, "malformed selectors:\n" + "\n".join(bad)


def test_every_python_method_is_marked():
    """Sanity: the helpers the tests call must be python_method so they're
    plain callables on the class."""
    names = {fn.name for fn in _methods() if _is_python_method(fn)}
    for expected in [
        "new_phrase", "set_fields", "flush_autosave", "select_entry",
        "toggle_enabled", "delete_current", "undo_delete", "set_search",
        "set_test_input", "poll_store", "visible_triggers",
        "trigger_hint_text", "status_text", "preview_output",
        "empty_state_visible", "undo_bar_visible",
    ]:
        assert expected in names, f"{expected} missing @objc.python_method"
