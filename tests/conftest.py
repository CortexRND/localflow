"""Suite-wide guard: no test may start a real LocalFlow Preview.

`lf hub` and bare `lf phrases` prefer `lf-desktop` on Linux and Windows.
Outside tests/test_desktop_launch.py (which restores the real discovery against
its own temporary HOME and LOCALAPPDATA), discovery always reports "not
installed", so those commands open the legacy window silently whatever the
developer has installed or exported. The launcher is imported lazily so jobs
that install only pytest (e.g. the installer checksum job) can still collect.
"""

import pytest

# desktop_launch.UI_ENV / BIN_ENV, spelled out so they're dropped without the import.
_LAUNCHER_ENVS = ("LOCALFLOW_UI", "LOCALFLOW_DESKTOP_BIN")


@pytest.fixture(autouse=True)
def _no_real_lf_desktop(monkeypatch):
    for name in _LAUNCHER_ENVS:
        monkeypatch.delenv(name, raising=False)
    try:
        from localflow import desktop_launch
    except ImportError:  # project dependencies missing: nothing can reach the launcher
        return
    monkeypatch.setattr(
        desktop_launch,
        "find_desktop",
        lambda env, platform=None, home=None: (None, desktop_launch.NOTICE_NOT_INSTALLED),
    )
