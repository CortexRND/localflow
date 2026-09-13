import os
import sys
from functools import lru_cache

from localflow.platform.base import Platform


@lru_cache
def current() -> Platform:
    selected = os.environ.get("LOCALFLOW_PLATFORM")
    if selected is None:
        selected = (
            "darwin"
            if sys.platform == "darwin"
            else "win32"
            if sys.platform == "win32"
            else "linux"
        )
    if selected == "darwin":
        from localflow.platform.darwin import DarwinPlatform

        return DarwinPlatform()
    if selected == "win32":
        from localflow.platform.win32 import Win32Platform

        return Win32Platform()
    if selected == "linux":
        from localflow.platform.linux import LinuxPlatform

        return LinuxPlatform()
    raise ValueError(f"unknown platform: {selected!r}")


def reset() -> None:
    current.cache_clear()


__all__ = ["Platform", "current", "reset"]
