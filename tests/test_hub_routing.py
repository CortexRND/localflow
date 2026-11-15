"""Hub page routing: request file, dispatchers, menu bar, CLI, legacy bundle.

Everything is mocked at the IO/subprocess/AppKit boundary; no macOS needed.
"""

import ast
import json
import os
import stat
import sys
import time
import types
from pathlib import Path

import pytest

from localflow import hub_route, hub_window, phrases_window
from localflow.appbundle import install_app

ROOT = Path(__file__).parent.parent


@pytest.fixture(autouse=True)
def home(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    return tmp_path


def _leftovers(tmp_path):
    folder = tmp_path / ".localflow"
    return sorted(p.name for p in folder.iterdir()) if folder.exists() else []


# ------------------------------------------------------------ request file ---


@pytest.mark.parametrize("page", hub_route.HUB_PAGES)
def test_every_page_round_trips(page):
    assert hub_route.write_request(page) is True

    assert hub_route.consume_request() == page


def test_request_is_atomic_private_and_consumed_once(home):
    hub_route.write_request("Hot Phrases")

    path = hub_route.request_path()
    assert path == home / ".localflow" / "page-request.json"
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    assert _leftovers(home) == ["page-request.json"]  # no tmp file left behind
    assert hub_route.consume_request() == "Hot Phrases"
    assert _leftovers(home) == []  # cleaned up on consume
    assert hub_route.consume_request() is None


def test_last_writer_wins_and_only_one_page_is_pending():
    hub_route.write_request("History")
    hub_route.write_request("Hot Phrases")

    assert hub_route.consume_request() == "Hot Phrases"
    assert hub_route.consume_request() is None


def test_racing_consumers_get_the_request_exactly_once(monkeypatch):
    hub_route.write_request("Models")
    path = hub_route.request_path()
    real_replace = os.replace
    second = []
    racing = []

    def racing_replace(src, dst):
        # A second consumer claims the file between our check and our claim.
        if Path(src) == path and not racing:
            racing.append(True)
            second.append(hub_route.consume_request())
        return real_replace(src, dst)

    monkeypatch.setattr(hub_route.os, "replace", racing_replace)

    first = hub_route.consume_request()

    assert second == ["Models"] and first is None


def test_invalid_pages_are_never_written(home):
    for page in ("Nope", "", None, "hot phrases", 3, "Home; rm -rf /"):
        assert hub_route.write_request(page) is False
    assert _leftovers(home) == []


@pytest.mark.parametrize(
    "payload",
    [
        "not json {",
        "[]",
        '"Hot Phrases"',
        "{}",
        '{"page": "Hot Phrases"}',  # no timestamp
        '{"page": "Hot Phrases", "ts": "now"}',
        '{"page": "Hot Phrases", "ts": true}',
        '{"page": "Evil", "ts": 0}',
        '{"page": ["Home"], "ts": 0}',
        '{"page": "__import__(\'os\').system(\'x\')", "ts": 0}',
    ],
)
def test_malformed_or_unknown_requests_fail_safely_and_are_removed(payload, home):
    path = hub_route.request_path()
    path.parent.mkdir(parents=True)
    ts = json.dumps(time.time())
    path.write_text(payload.replace('"ts": 0', f'"ts": {ts}'), encoding="utf-8")

    assert hub_route.consume_request() is None
    assert _leftovers(home) == []


def test_oversized_request_is_ignored(home):
    path = hub_route.request_path()
    path.parent.mkdir(parents=True)
    path.write_text(
        json.dumps({"page": "Home", "ts": time.time(), "pad": "x" * 100000}),
        encoding="utf-8",
    )

    assert hub_route.consume_request() is None
    assert _leftovers(home) == []


def test_stale_and_future_requests_are_dropped():
    now = 10_000.0
    hub_route.write_request("Hot Phrases", now=lambda: now - hub_route.STALE_SECONDS - 1)
    assert hub_route.consume_request(now=lambda: now) is None

    hub_route.write_request("Hot Phrases", now=lambda: now + 600)
    assert hub_route.consume_request(now=lambda: now) is None

    hub_route.write_request("Hot Phrases", now=lambda: now - 1)
    assert hub_route.consume_request(now=lambda: now) == "Hot Phrases"


def test_write_failure_is_reported_and_leaves_no_tmp_file(monkeypatch, home):
    def broken(src, dst):
        raise OSError("disk full")

    monkeypatch.setattr(hub_route.os, "replace", broken)

    assert hub_route.write_request("Hot Phrases") is False
    assert _leftovers(home) == []


def test_unwritable_directory_is_reported(monkeypatch):
    monkeypatch.setattr(
        hub_route.Path, "mkdir", lambda *a, **k: (_ for _ in ()).throw(PermissionError("x"))
    )

    assert hub_route.write_request("Hot Phrases") is False


def test_clear_request_is_idempotent(home):
    hub_route.write_request("Home")
    hub_route.clear_request()
    hub_route.clear_request()

    assert _leftovers(home) == []


def test_validate_page_is_a_strict_whitelist():
    assert hub_route.validate_page(" Hot Phrases ") == "Hot Phrases"
    assert hub_route.validate_page("hot phrases") is None
    assert hub_route.validate_page(None) is None
    assert hub_route.HUB_PAGES[0] == hub_route.DEFAULT_PAGE == "Home"


# ------------------------------------------------------------- dispatchers ---


def _no_standalone_mac_editor(monkeypatch):
    """Importing the standalone AppKit editor would raise ImportError."""
    monkeypatch.setitem(sys.modules, "localflow.phrases_mac", None)


def test_phrases_window_on_macos_routes_to_the_main_app_hub_page(monkeypatch):
    monkeypatch.setattr(sys, "platform", "darwin")
    _no_standalone_mac_editor(monkeypatch)
    routed = []
    app = types.ModuleType("localflow.main_app")
    app.main = lambda argv=None: routed.append(argv)
    monkeypatch.setitem(sys.modules, "localflow.main_app", app)

    phrases_window.main()

    assert routed == [["--page", "Hot Phrases"]]


def test_phrases_window_with_an_explicit_store_path_stays_standalone(monkeypatch):
    monkeypatch.setattr(sys, "platform", "darwin")
    opened = []
    tk = types.ModuleType("localflow.phrases_tk")
    tk.main = lambda path=None: opened.append(path)
    monkeypatch.setitem(sys.modules, "localflow.phrases_tk", tk)

    phrases_window.main("/tmp/x.json")

    assert opened == ["/tmp/x.json"]


def test_phrases_window_off_macos_keeps_the_tk_fallback(monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    opened = []
    tk = types.ModuleType("localflow.phrases_tk")
    tk.main = lambda path=None: opened.append(path)
    monkeypatch.setitem(sys.modules, "localflow.phrases_tk", tk)

    phrases_window.main()

    assert opened == [None]


@pytest.mark.parametrize("page", ["Home", "Hot Phrases", "Meetings"])
def test_hub_window_on_macos_routes_every_request_to_the_main_app(monkeypatch, page):
    monkeypatch.setattr(sys, "platform", "darwin")
    routed = []
    app = types.ModuleType("localflow.main_app")
    app.main = lambda argv=None: routed.append(argv)
    monkeypatch.setitem(sys.modules, "localflow.main_app", app)
    # An independent hub_mac must never be built by the dispatcher.
    monkeypatch.setitem(sys.modules, "localflow.hub_mac", None)

    hub_window.main(["--page", page])

    assert routed == [["--page", page]]


def test_hub_window_defaults_to_home_and_rejects_unknown_pages(monkeypatch, capsys):
    monkeypatch.setattr(sys, "platform", "darwin")
    routed = []
    app = types.ModuleType("localflow.main_app")
    app.main = lambda argv=None: routed.append(argv)
    monkeypatch.setitem(sys.modules, "localflow.main_app", app)

    hub_window.main([])
    with pytest.raises(SystemExit) as exit_info:
        hub_window.main(["--page", "Nope"])

    assert routed == [["--page", "Home"]] and exit_info.value.code == 2
    assert "invalid choice" in capsys.readouterr().err


def test_hub_window_off_macos_runs_the_backend_directly(monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    opened = []
    tk = types.ModuleType("localflow.hub_tk")
    tk.PAGES = ("Home", "History")
    tk.main = lambda initial_page="Home": opened.append(initial_page)
    monkeypatch.setitem(sys.modules, "localflow.hub_tk", tk)
    app = types.ModuleType("localflow.main_app")
    app.main = lambda argv=None: opened.append("main_app")
    monkeypatch.setitem(sys.modules, "localflow.main_app", app)

    hub_window.main(["--page", "History"])
    hub_window.main(["--page", "Models"])  # no such Tk page -> Home

    assert opened == ["History", "Home"]


def test_backend_hot_phrases_falls_back_to_the_tk_editor(monkeypatch):
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setitem(sys.modules, "localflow.hub_mac", None)  # no PyObjC
    opened = []
    tk = types.ModuleType("localflow.phrases_tk")
    tk.main = lambda path=None: opened.append("phrases_tk")
    monkeypatch.setitem(sys.modules, "localflow.phrases_tk", tk)

    hub_window.run_backend("Hot Phrases")

    assert opened == ["phrases_tk"]


def test_backend_uses_the_native_hub_on_macos(monkeypatch):
    monkeypatch.setattr(sys, "platform", "darwin")
    opened = []
    mac = types.ModuleType("localflow.hub_mac")
    mac.main = lambda initial_page="Home": opened.append(initial_page)
    monkeypatch.setitem(sys.modules, "localflow.hub_mac", mac)

    hub_window.run_backend("Hot Phrases")

    assert opened == ["Hot Phrases"]


def test_dispatchers_never_reach_the_standalone_native_editor():
    for name in ("phrases_window.py", "hub_window.py", "main_app.py", "main_app_mac.py"):
        tree = ast.parse((ROOT / "localflow" / name).read_text(encoding="utf-8"))
        imported = {
            node.module for node in ast.walk(tree) if isinstance(node, ast.ImportFrom)
        } | {
            alias.name for node in ast.walk(tree) if isinstance(node, ast.Import)
            for alias in node.names
        }
        assert "localflow.phrases_mac" not in imported, name
    app = (ROOT / "localflow" / "main_app_mac.py").read_text(encoding="utf-8")
    assert "phrases_mac" not in app


# ------------------------------------------------------ menu bar / CLI / app ---


def test_menu_bar_hot_phrases_item_opens_the_hub_page():
    source = (ROOT / "localflow" / "menubar.py").read_text(encoding="utf-8")
    tree = ast.parse(source)
    method = next(
        node for node in ast.walk(tree)
        if isinstance(node, ast.FunctionDef) and node.name == "open_phrases"
    )
    body = ast.get_source_segment(source, method)
    assert '"localflow.hub_window", "--page", "Hot Phrases"' in body
    assert "localflow.phrases_window" not in body
    assert 'rumps.MenuItem("Hot Phrases…", callback=self.open_phrases)' in source


def test_lf_phrases_uses_the_dispatcher(monkeypatch):
    from click.testing import CliRunner

    from localflow import cli

    opened = []
    monkeypatch.setattr(phrases_window, "main", lambda path=None: opened.append(path))

    result = CliRunner().invoke(cli.cli, ["phrases"])

    assert result.exit_code == 0, result.output
    assert opened == [None]


def test_lf_hub_page_option_is_validated_and_forwarded(monkeypatch):
    from click.testing import CliRunner

    from localflow import cli

    launched = []
    monkeypatch.setattr(cli.subprocess, "Popen", lambda argv, **kw: launched.append(argv))

    ok = CliRunner().invoke(cli.cli, ["hub", "--page", "Hot Phrases"])
    default = CliRunner().invoke(cli.cli, ["hub"])
    bad = CliRunner().invoke(cli.cli, ["hub", "--page", "Nope"])

    assert ok.exit_code == 0 and default.exit_code == 0 and bad.exit_code == 2
    assert launched == [
        [sys.executable, "-m", "localflow.hub_window", "--page", "Hot Phrases"],
        [sys.executable, "-m", "localflow.hub_window", "--page", "Home"],
    ]


def test_legacy_hot_phrases_app_is_an_alias_for_the_hub_page(tmp_path, monkeypatch):
    bundle = install_app(dest_dir=tmp_path, python="/usr/bin/python3")
    launcher = (bundle / "Contents" / "MacOS" / "launcher").read_text()
    assert launcher == (
        '#!/bin/bash\nexec /usr/bin/python3 -m localflow.phrases_window "$@"\n'
    )

    # What that launcher runs: the dispatcher, which hands over to main_app.
    monkeypatch.setattr(sys, "platform", "darwin")
    routed = []
    app = types.ModuleType("localflow.main_app")
    app.main = lambda argv=None: routed.append(argv)
    monkeypatch.setitem(sys.modules, "localflow.main_app", app)
    phrases_window.main()

    assert routed == [["--page", "Hot Phrases"]]


def test_readme_points_at_the_hub_and_the_optional_alias():
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    assert "Hot Phrases page" in readme
    assert "compatibility alias" in readme
