import stat

from localflow import apitoken


def test_load_or_create_overwrites_token_with_private_file(monkeypatch, tmp_path):
    path = tmp_path / "api-token"
    monkeypatch.setattr(apitoken, "token_path", lambda: path)

    first = apitoken.load_or_create()
    second = apitoken.load_or_create()

    assert first != second
    assert path.read_text() == second + "\n"
    assert stat.S_IMODE(path.stat().st_mode) == 0o600


def test_read_returns_token_or_none(monkeypatch, tmp_path):
    path = tmp_path / "api-token"
    monkeypatch.setattr(apitoken, "token_path", lambda: path)

    assert apitoken.read() is None
    path.write_text("token-value\n")
    assert apitoken.read() == "token-value"
