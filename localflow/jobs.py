import time
from dataclasses import dataclass, field
from typing import Literal

import numpy as np

from localflow.context import AppInfo


@dataclass(frozen=True)
class Job:
    kind: Literal["dictate", "command", "transform"]
    audio: np.ndarray | None = None
    app_info: AppInfo | None = None
    selection: str = ""
    preset: str | None = None
    clipboard_before: str | None = None
    # time.monotonic() when queued (the perf ledger's queue_ms); not compared.
    queued_at: float | None = field(default_factory=time.monotonic, compare=False)
