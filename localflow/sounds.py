"""Short audio cues through the active platform backend."""

from localflow.platform import current

_SOUNDS = {
    "lock": "/System/Library/Sounds/Tink.aiff",
    "cancel": "/System/Library/Sounds/Funk.aiff",
    "warn": "/System/Library/Sounds/Sosumi.aiff",
    "error": "/System/Library/Sounds/Basso.aiff",
    "achievement": "/System/Library/Sounds/Glass.aiff",
}


def play(cue: str) -> None:
    platform = current()
    if cue in _SOUNDS and platform.name != "darwin":
        return
    platform.play_sound(cue)
