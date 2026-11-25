"""`lf speakers status|download` on a fake cache; never touches the network."""

import hashlib
import json
import socket

import pytest
from click.testing import CliRunner

from localflow import cli as cli_module
from localflow.config import Config
from localflow.diarization import models as dm
from localflow.models import DownloadCancelled

PAYLOAD = bytes(range(256)) * 1200
MODEL = dm.DiarModel(
    id="seg", label="Speaker labels: seg", repo="test-org/seg-model", revision="c" * 40,
    filename="model.onnx", sha256=hashlib.sha256(PAYLOAD).hexdigest(),
    size_bytes=len(PAYLOAD), licence="MIT", source="test fixture",
)


class FakeResponse:
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def raise_for_status(self):
        pass

    def iter_content(self, size):
        for start in range(0, len(PAYLOAD), size):
            yield PAYLOAD[start:start + size]


@pytest.fixture(autouse=True)
def isolated(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.setenv("HF_HUB_CACHE", str(tmp_path / "hub"))
    monkeypatch.delenv("HF_ENDPOINT", raising=False)
    monkeypatch.setattr(dm, "DIAR_MODELS", (MODEL,))
    monkeypatch.setattr(cli_module, "load_config", lambda: Config(speaker_diarization=True))

    def no_network(url):
        raise AssertionError(f"network: {url}")

    monkeypatch.setattr(dm, "_open_stream", no_network)


@pytest.fixture
def offline(monkeypatch):
    def refuse(*args, **kwargs):
        raise AssertionError("socket opened")

    monkeypatch.setattr(socket, "socket", refuse)
    monkeypatch.setattr(socket, "create_connection", refuse)


def run(*args):
    return CliRunner().invoke(cli_module.cli, ["speakers", *args])


def test_status_on_an_empty_cache_is_offline_and_reports_each_part(offline, tmp_path):
    result = run("status")
    assert result.exit_code == 0, result.output
    lines = result.output.splitlines()
    assert lines[0].startswith("models: not ready")
    assert "lf speakers download" in lines[0]
    assert any(line.startswith("seg: ") for line in lines)
    assert any(line.startswith("runtime (sherpa-onnx): ") for line in lines)
    assert "speaker_diarization: true" in lines
    assert "diarize_mic: false" in lines
    assert "speaker_recognition: false" in lines
    assert "pip install" not in result.output
    assert "[diarization]" not in result.output
    assert not (tmp_path / "hub").exists()


@pytest.mark.parametrize("installed", [True, False])
def test_status_reports_the_runtime_accurately(monkeypatch, installed):
    monkeypatch.setattr(dm, "runtime_installed", lambda: installed)
    result = run("status")
    expected = "installed" if installed else "not installed"
    assert f"runtime (sherpa-onnx): {expected}" in result.output.splitlines()


def test_status_json(offline):
    result = run("status", "--json")
    assert result.exit_code == 0, result.output
    info = json.loads(result.output)
    assert info["models_ready"] is False
    assert isinstance(info["extra_installed"], bool)
    assert [model["id"] for model in info["models"]] == ["seg"]
    assert info["speaker_diarization"] is True
    assert info["diarize_mic"] is False
    assert info["speaker_recognition"] is False


def test_download_success_makes_models_ready(monkeypatch):
    urls = []

    def fake_stream(url):
        urls.append(url)
        return FakeResponse()

    monkeypatch.setattr(dm, "_open_stream", fake_stream)
    result = run("download")
    assert result.exit_code == 0, result.output
    assert "downloaded" in result.output
    assert urls == [f"https://huggingface.co/test-org/seg-model/resolve/{'c' * 40}/model.onnx"]
    assert dm.models_ready() == (True, "")
    status = run("status")
    assert status.output.splitlines()[0] == "models: ready"
    assert "seg: verified" in status.output.splitlines()


def test_download_failure_is_a_friendly_click_error(monkeypatch):
    def fail(*args, **kwargs):
        raise RuntimeError("connection timed out while fetching")

    monkeypatch.setattr(dm, "download_diarization_models", fail)
    result = run("download")
    assert result.exit_code == 1
    assert "Network problem while downloading" in result.output


def test_download_cancelled_aborts(monkeypatch):
    def cancelled(*args, **kwargs):
        raise DownloadCancelled("Download cancelled")

    monkeypatch.setattr(dm, "download_diarization_models", cancelled)
    result = run("download")
    assert result.exit_code == 1
    assert "Aborted" in result.output


def test_speakers_is_hidden_from_top_level_help_but_usable():
    top = CliRunner().invoke(cli_module.cli, ["--help"])
    assert top.exit_code == 0
    assert not any(line.split()[:1] == ["speakers"] for line in top.output.splitlines())
    assert cli_module.cli.commands["speakers"].hidden is True
    group = run("--help")
    assert group.exit_code == 0
    listed = [line.split()[0] for line in group.output.split("Commands:")[1].splitlines()
              if line.strip()]
    assert listed == ["download", "status"]
