import ast
import os
import stat
import time
from pathlib import Path
from urllib.parse import urlparse
from urllib.request import url2pathname

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


def _fake_server(monkeypatch, tmp_path, code="one-time-code"):
    calls = []
    monkeypatch.setattr(weblaunch, "load", lambda: "secret-token")
    monkeypatch.setattr(weblaunch, "config_dir", lambda: tmp_path)

    def fake_post(url, headers, timeout):
        calls.append((url, headers))
        return FakeResponse({"code": code})

    monkeypatch.setattr(weblaunch.requests, "post", fake_post)
    return calls


def test_opener_gets_a_private_launch_file_not_the_code(monkeypatch, tmp_path):
    calls = _fake_server(monkeypatch, tmp_path)
    opened = []
    weblaunch.open_web_ui(BASE, "/settings", opener=opened.append)

    assert calls == [(f"{BASE}/api/launch-code", {"X-Localflow-Token": "secret-token"})]
    assert len(opened) == 1
    assert "one-time-code" not in opened[0]
    assert "secret-token" not in opened[0]
    assert "launch=" not in opened[0]
    launch_file = Path(url2pathname(urlparse(opened[0]).path))
    assert launch_file.parent == tmp_path / "launch"
    page = launch_file.read_text(encoding="utf-8")
    assert 'method="post"' in page
    assert f'action="{BASE}/settings"' in page
    assert 'value="one-time-code"' in page
    assert "secret-token" not in page
    if os.name == "posix":
        assert stat.S_IMODE(launch_file.stat().st_mode) == 0o600
        assert stat.S_IMODE(launch_file.parent.stat().st_mode) == 0o700


def test_launch_file_escapes_the_code(monkeypatch, tmp_path):
    _fake_server(monkeypatch, tmp_path, code='"><script>x</script>')
    opened = []
    weblaunch.open_web_ui(BASE, "/", opener=opened.append)
    page = Path(url2pathname(urlparse(opened[0]).path)).read_text(encoding="utf-8")
    assert "<script>x" not in page


def test_stale_launch_files_are_removed(monkeypatch, tmp_path):
    _fake_server(monkeypatch, tmp_path)
    stale = tmp_path / "launch" / "open-old.html"
    stale.parent.mkdir()
    stale.write_text("old", encoding="utf-8")
    old = time.time() - weblaunch.LAUNCH_FILE_TTL_SECONDS - 5
    os.utime(stale, (old, old))
    weblaunch.open_web_ui(BASE, "/", opener=lambda target: None)
    assert not stale.exists()


def test_launch_never_falls_back_to_the_token(monkeypatch, tmp_path):
    monkeypatch.setattr(weblaunch, "load", lambda: "secret-token")
    monkeypatch.setattr(weblaunch, "config_dir", lambda: tmp_path)

    def down(*args, **kwargs):
        raise requests.ConnectionError("server not running")

    monkeypatch.setattr(weblaunch.requests, "post", down)
    assert weblaunch.launch_target(BASE, "/") == f"{BASE}/"

    monkeypatch.setattr(weblaunch.requests, "post", lambda *a, **k: FakeResponse({}, 401))
    assert weblaunch.launch_target(BASE) == f"{BASE}/settings"


def test_launch_without_token_skips_the_server(monkeypatch):
    monkeypatch.setattr(weblaunch, "load", lambda: "")
    monkeypatch.setattr(
        weblaunch.requests, "post", lambda *a, **k: (_ for _ in ()).throw(AssertionError)
    )
    assert weblaunch.launch_target(BASE) == f"{BASE}/settings"


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
    monkeypatch.setattr(weblaunch, "launch_target", lambda base, path: "file:///launch.html")
    result = CliRunner().invoke(cli_module.cli, ["ui"])

    assert result.exit_code == 0, result.output
    assert opened == ["file:///launch.html"]


def test_web_pages_strip_token_and_launch_from_the_address_bar():
    for name in ("index.html", "settings.html"):
        page = (ROOT / "static" / name).read_text(encoding="utf-8")
        assert "history.replaceState" in page, name
        assert "params.delete('token')" in page, name
        assert "params.delete('launch')" in page, name
