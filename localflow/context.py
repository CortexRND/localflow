"""Frontmost-app metadata and cleanup-style selection."""

from __future__ import annotations

import logging
import subprocess
import sys
from dataclasses import dataclass

log = logging.getLogger("localflow.context")


@dataclass(frozen=True)
class AppInfo:
    name: str
    bundle_id: str = ""


_BUNDLE_CATEGORIES = {
    "com.tinyspeck.slackmacgap": "work",
    "com.microsoft.teams2": "work",
    "com.microsoft.teams": "work",
    "com.linear": "work",
    "notion.id": "work",
    "com.apple.mail": "email",
    "com.microsoft.outlook": "email",
    "com.superhuman.electron": "email",
    "com.readdle.sparkdesktop": "email",
    "com.apple.mobilesms": "personal",
    "net.whatsapp.whatsapp": "personal",
    "ru.keepcoder.telegram": "personal",
    "com.hnc.discord": "personal",
}
_NAME_CATEGORIES = {
    "slack": "work",
    "microsoft teams": "work",
    "linear": "work",
    "notion": "work",
    "mail": "email",
    "outlook": "email",
    "microsoft outlook": "email",
    "apple mail": "email",
    "superhuman": "email",
    "spark": "email",
    "messages": "personal",
    "whatsapp": "personal",
    "telegram": "personal",
    "discord": "personal",
}
_CATEGORIES = {"work", "email", "personal", "other"}
_FRONTMOST_SCRIPT = (
    'tell application "System Events" to get {name, bundle identifier} of '
    "first process whose frontmost is true"
)


def frontmost_app() -> AppInfo | None:
    if sys.platform != "darwin":
        return None
    try:
        from AppKit import NSWorkspace

        application = NSWorkspace.sharedWorkspace().frontmostApplication()
        if application is None:
            return None
        name = application.localizedName() or ""
        bundle_id = application.bundleIdentifier() or ""
        return AppInfo(str(name), str(bundle_id))
    except ImportError:
        pass
    except Exception:
        log.debug("PyObjC frontmost-app lookup failed", exc_info=True)
    try:
        result = subprocess.run(
            ["osascript", "-e", _FRONTMOST_SCRIPT],
            check=True,
            capture_output=True,
            text=True,
            timeout=1,
        )
        value = result.stdout.strip()
        if not value:
            return None
        if ", " in value:
            name, bundle_id = value.rsplit(", ", 1)
        else:
            name, bundle_id = value, ""
        return AppInfo(name, bundle_id)
    except (OSError, subprocess.SubprocessError):
        log.debug("AppleScript frontmost-app lookup failed", exc_info=True)
        return None


def category_for(
    app: AppInfo | None,
    overrides: list[str] | tuple[str, ...] = (),
) -> str:
    if app is None:
        return "other"
    bundle_id = app.bundle_id.casefold()
    name = app.name.casefold()
    for override in overrides:
        if not isinstance(override, str) or "=" not in override:
            continue
        target, category = (part.strip() for part in override.split("=", 1))
        category = category.casefold()
        if category not in _CATEGORIES:
            continue
        target = target.casefold()
        if target and target in {bundle_id, name}:
            return category
    return _BUNDLE_CATEGORIES.get(bundle_id, _NAME_CATEGORIES.get(name, "other"))
