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
