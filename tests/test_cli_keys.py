import pytest
from click.testing import CliRunner

from localflow import cli as cli_module
from localflow import provider_keys as keys


@pytest.fixture
def vault(monkeypatch):
    store = {}
    monkeypatch.setattr(keys.secrets, "get_secret", lambda name: store.get(name, ""))
    monkeypatch.setattr(
        keys.secrets, "set_secret", lambda name, value: store.__setitem__(name, value)
    )
    monkeypatch.setattr(
        keys.secrets, "delete_secret", lambda name: store.pop(name, None)
    )
    monkeypatch.setattr(keys, "detect_imports", lambda file=None: [])
    return store


def test_keys_list_masks_keys(vault):
    vault["provider_key_openai"] = "private-key-1234"
    result = CliRunner().invoke(cli_module.cli, ["keys", "list"])
    assert result.exit_code == 0, result.output
    assert "OpenAI: ••••1234" in result.output
    assert "Fireworks: No key" in result.output
    assert "Local (Ollama): No key needed" in result.output
    assert "private-key" not in result.output


def test_keys_import_empty(vault):
    result = CliRunner().invoke(cli_module.cli, ["keys", "import"])
    assert result.exit_code == 0, result.output
    assert "No keys found" in result.output
    assert not vault


@pytest.mark.parametrize("yes", [False, True])
def test_keys_import_only_confirmed(vault, monkeypatch, tmp_path, yes):
    path = tmp_path / ".env"
    path.write_text("# mock source")
    seen = []

    def detect(file=None):
        seen.append(file)
        return [
            keys.ImportCandidate("fireworks", "environment", "private-first-1234"),
            keys.ImportCandidate("openai", str(path), "private-second-5678"),
        ]

    monkeypatch.setattr(keys, "detect_imports", detect)
    args = ["keys", "import", "--file", str(path)] + (["--yes"] if yes else [])
    result = CliRunner().invoke(cli_module.cli, args, input="n\ny\n")
    assert result.exit_code == 0, result.output
    assert seen == [path]
    assert "••••1234" in result.output and "••••5678" in result.output
    assert "private-" not in result.output
    assert vault["provider_key_openai"] == "private-second-5678"
    assert ("provider_key_fireworks" in vault) is yes
    if yes:
        assert vault["fireworks_api_key"] == "private-first-1234"
        assert "Save this key?" not in result.output


def test_keys_import_errors_are_sanitized(vault, monkeypatch):
    def fail_read(file=None):
        raise OSError("private-key")

    monkeypatch.setattr(keys, "detect_imports", fail_read)
    result = CliRunner().invoke(cli_module.cli, ["keys", "import", "--yes"])
    assert result.exit_code == 1
    assert "Unable to read" in result.output
    assert "private-key" not in result.output

    monkeypatch.setattr(
        keys,
        "detect_imports",
        lambda file=None: [
            keys.ImportCandidate("openai", "environment", "private-key-1234")
        ],
    )

    def fail_save(*args):
        raise keys.secrets.SecretsUnavailable("private-key")

    monkeypatch.setattr(keys.secrets, "set_secret", fail_save)
    result = CliRunner().invoke(cli_module.cli, ["keys", "import", "--yes"])
    assert result.exit_code == 1
    assert "Unable to save keys; check Keychain access" in result.output
    assert "private-key" not in result.output
