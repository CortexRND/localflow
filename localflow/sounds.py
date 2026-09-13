"""Short audio cues through the active platform backend."""

from localflow.platform import current


def play(cue: str) -> None:
    current().play_sound(cue)
