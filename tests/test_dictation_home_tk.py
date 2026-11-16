import sys
from dataclasses import replace

import pytest

pytest.importorskip("tkinter")
pytestmark = pytest.mark.skipif(
    sys.platform == "darwin",
    reason="macOS uses the AppKit Hub and mac UI snapshots",
)

from test_hub_tk import _window

from localflow import config


@pytest.mark.parametrize("theme", ["light", "dark"])
def test_home_toggle_and_external_refresh(tmp_path, monkeypatch, theme):
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))
    config.save_config(config.Config())
    root, window, _history, _dictionary = _window(tmp_path, theme=theme)
    try:
        root.update()
        assert window.dictation_button.cget("text") == "Turn Dictation Off"
        window.dictation_button.invoke()
        root.update()
        assert not config.load_config().dictation_enabled
        assert (
            window.dictation_status.cget("text")
            == "Dictation is off — push-to-talk is paused"
        )
        assert window.dictation_status.cget("fg") == window.tokens["muted"]
        assert window.dictation_button.cget("text") == "Turn Dictation On"
        config.save_config(replace(config.load_config(), dictation_enabled=True))
        window._poll_dictation()
        assert (
            window.dictation_status.cget("text")
            == "Dictation is on — push-to-talk is ready"
        )

        def fail(_config):
            raise OSError("read-only")

        monkeypatch.setattr("localflow.hub_tk.save_config", fail)
        window.dictation_button.invoke()
        assert config.load_config().dictation_enabled
        assert "read-only" in window.page_errors["Home"]
    finally:
        for callback in root.tk.call("after", "info"):
            root.after_cancel(callback)
        root.destroy()
