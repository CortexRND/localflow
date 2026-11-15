import ast
from pathlib import Path

import requests

from localflow import weblaunch

BASE = "http://127.0.0.1:8756"
ROOT = Path(__file__).resolve().parents[1] / "localflow"


class FakeResponse:
    def __init__(self, payload, status=200):
        self.payload = payload
        self.status = status

    def raise_for_status(self):
        if self.status >= 400:
            raise requests.HTTPError(str(self.status))

    def json(self):
        return self.payload


def test_launch_url_uses_a_launch_code_not_the_token(monkeypatch):
    calls = []
    monkeypatch.setattr(weblaunch, "load", lambda: "secret-token")

    def fake_post(url, headers, timeout):
        calls.append((url, headers))
        return FakeResponse({"code": "one-time"})

    monkeypatch.setattr(weblaunch.requests, "post", fake_post)
    opened = []
    weblaunch.open_web_ui(BASE, "/settings", opener=opened.append)

    assert opened == [f"{BASE}/settings?launch=one-time"]
    assert calls == [(f"{BASE}/api/launch-code", {"X-Localflow-Token": "secret-token"})]


def test_launch_url_never_falls_back_to_the_token(monkeypatch):
    monkeypatch.setattr(weblaunch, "load", lambda: "secret-token")

    def down(*args, **kwargs):
        raise requests.ConnectionError("server not running")

    monkeypatch.setattr(weblaunch.requests, "post", down)
    assert weblaunch.launch_url(BASE, "/") == f"{BASE}/"

    monkeypatch.setattr(weblaunch.requests, "post", lambda *a, **k: FakeResponse({}, 401))
    assert weblaunch.launch_url(BASE) == f"{BASE}/settings"


def test_launch_url_without_token_skips_the_server(monkeypatch):
    monkeypatch.setattr(weblaunch, "load", lambda: "")
    monkeypatch.setattr(
        weblaunch.requests, "post", lambda *a, **k: (_ for _ in ()).throw(AssertionError)
    )
    assert weblaunch.launch_url(BASE) == f"{BASE}/settings"


def test_launchers_never_put_the_token_in_a_url():
    for name in ("cli.py", "setup_tk.py", "setup_mac.py", "menubar.py"):
        source = (ROOT / name).read_text(encoding="utf-8")
        assert "token=" not in "".join(
            node.value
            for node in ast.walk(ast.parse(source))
            if isinstance(node, ast.Constant) and isinstance(node.value, str)
        ), name
        assert "open_web_ui(" in source, name


def test_cli_ui_opens_a_launch_url(monkeypatch):
    from click.testing import CliRunner

    from localflow import cli as cli_module

    opened = []

    class FakePlatform:
        def open_path(self, target):
            opened.append(target)

    monkeypatch.setattr(cli_module, "current", lambda: FakePlatform())
    monkeypatch.setattr(weblaunch, "launch_url", lambda base, path: f"{base}{path}?launch=c")
    result = CliRunner().invoke(cli_module.cli, ["ui"])

    assert result.exit_code == 0, result.output
    assert opened == [f"{cli_module.BASE}/settings?launch=c"]


def test_web_pages_strip_token_and_launch_from_the_address_bar():
    for name in ("index.html", "settings.html"):
        page = (ROOT / "static" / name).read_text(encoding="utf-8")
        assert "history.replaceState" in page, name
        assert "params.delete('token')" in page, name
        assert "params.delete('launch')" in page, name
