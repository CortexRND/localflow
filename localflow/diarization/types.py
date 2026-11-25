from dataclasses import dataclass


@dataclass(frozen=True)
class TimedText:            # one STT segment inside a chunk
    start: float            # seconds from chunk start
    end: float
    text: str
