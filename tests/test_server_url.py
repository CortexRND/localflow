from pathlib import Path

import pytest

from localflow import cli as cli_module
from localflow.config import Config
from localflow.server_url import base_url, client_host, configured_base_url

ROOT = Path(__file__).parent.parent


@pytest.mark.parametrize(
    ("host", "expected"),
    [
        ("127.0.0.1", "127.0.0.1"),
        ("localhost", "localhost"),
        ("192.168.1.20", "192.168.1.20"),
        ("0.0.0.0", "127.0.0.1"),
        ("*", "127.0.0.1"),
        ("", "127.0.0.1"),
        ("  ", "127.0.0.1"),
        (None, "127.0.0.1"),
        ("::", "::1"),
        ("[::]", "::1"),
        ("::1", "::1"),
        ("[::1]", "::1"),
        ("fd00::5", "fd00::5"),
    ],
)
def test_client_host_maps_wildcards_to_loopback(host, expected):
    assert client_host(host) == expected


@pytest.mark.parametrize(
    ("host", "port", "expected"),
    [
        ("127.0.0.1", 8756, "http://127.0.0.1:8756"),
        ("0.0.0.0", 9000, "http://127.0.0.1:9000"),
        ("::", 9000, "http://[::1]:9000"),
        ("::1", 9000, "http://[::1]:9000"),
        ("[::1]", 9000, "http://[::1]:9000"),
        ("fe80::1%en0", 9000, "http://[fe80::1%25en0]:9000"),
        ("localhost", 9000, "http://localhost:9000"),
        ("10.0.0.5", 9000, "http://10.0.0.5:9000"),
    ],
)
def test_base_url_brackets_ipv6_and_keeps_the_port(host, port, expected):
    assert base_url(host, port) == expected


def test_configured_base_url_reads_the_saved_config(tmp_path, monkeypatch):
    path = tmp_path / "config.toml"
    path.write_text(
        'config_version = 2\n[server]\nhost = "0.0.0.0"\nport = 9911\n', encoding="utf-8"
    )
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    monkeypatch.setenv("HOME", str(tmp_path))

    assert configured_base_url() == "http://127.0.0.1:9911"
    assert configured_base_url(Config()) == "http://127.0.0.1:8756"


def test_cli_and_menubar_use_the_shared_helper():
    assert cli_module.BASE == configured_base_url(cli_module._config)
    for name in ("cli.py", "menubar.py", "meeting_client.py"):
        source = (ROOT / "localflow" / name).read_text(encoding="utf-8")
        assert "configured_base_url(" in source, name
        assert 'f"http://127.0.0.1:' not in source, name
