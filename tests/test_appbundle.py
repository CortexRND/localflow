import os
import plistlib
import stat

from localflow.appbundle import install_app, uninstall_app


def test_install_writes_bundle(tmp_path):
    bundle = install_app(dest_dir=tmp_path, python="/usr/bin/python3")
    assert bundle == tmp_path / "LocalFlow Hot Phrases.app"

    plist = plistlib.loads((bundle / "Contents" / "Info.plist").read_bytes())
    assert plist["CFBundleName"] == "LocalFlow Hot Phrases"
    assert plist["CFBundleDisplayName"] == "LocalFlow Hot Phrases"
    assert plist["CFBundleIdentifier"] == "com.cortexrnd.localflow.hotphrases"
    assert plist["CFBundleExecutable"] == "launcher"
    assert plist["CFBundlePackageType"] == "APPL"
    assert plist["CFBundleShortVersionString"] == "0.1.0"
    assert plist["NSHighResolutionCapable"] is True

    launcher = bundle / "Contents" / "MacOS" / "launcher"
    assert launcher.read_text() == (
        "#!/bin/bash\nexec /usr/bin/python3 -m localflow.phrases_window \"$@\"\n"
    )
    assert launcher.stat().st_mode & stat.S_IXUSR


def test_install_quotes_python_path(tmp_path):
    bundle = install_app(dest_dir=tmp_path, python="/opt/my python/python3")
    launcher = bundle / "Contents" / "MacOS" / "launcher"
    assert "'/opt/my python/python3'" in launcher.read_text()


def test_install_is_idempotent(tmp_path):
    first = install_app(dest_dir=tmp_path, python="/usr/bin/python3")
    (first / "Contents" / "stale").write_text("x")  # leftover file survives
    second = install_app(dest_dir=tmp_path, python="/usr/bin/python3")
    assert first == second
    launcher = second / "Contents" / "MacOS" / "launcher"
    assert os.access(launcher, os.X_OK)


def test_uninstall(tmp_path):
    assert uninstall_app(dest_dir=tmp_path) is False
    install_app(dest_dir=tmp_path, python="/usr/bin/python3")
    assert uninstall_app(dest_dir=tmp_path) is True
    assert not (tmp_path / "LocalFlow Hot Phrases.app").exists()
    assert uninstall_app(dest_dir=tmp_path) is False
