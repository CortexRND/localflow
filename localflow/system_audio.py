"""System ("loopback") audio capture for meetings: what the other side says.

The mic only hears the user (plus speaker bleed). To hear remote participants
we capture what the machine plays:

* macOS 13+: ScreenCaptureKit audio capture via PyObjC
  (``pip install -e '.[system-audio]'``). Needs Screen Recording permission
  for the Python binary running localflow.
* Fallback on any OS: a loopback input device read with sounddevice — on
  macOS a BlackHole multi-output device, on Linux a PulseAudio/PipeWire
  ``.monitor`` source, on Windows "Stereo Mix".

Every source delivers mono float32 at the session sample rate to a callback.
"""

import logging
import sys
import threading
from collections.abc import Callable

import numpy as np
import sounddevice as sd

log = logging.getLogger("localflow.system_audio")

AudioCallback = Callable[[np.ndarray], None]

# Substrings of loopback device names tried when no device is configured.
_AUTO_DEVICE_HINTS = ("blackhole", "loopback", ".monitor", "monitor of", "stereo mix")
_SCK_RATES = (8000, 16000, 24000, 48000)


class SystemAudioUnavailable(RuntimeError):
    """No system-audio source could be opened."""


def resample(audio: np.ndarray, src_rate: int, dst_rate: int) -> np.ndarray:
    """Linear-interpolation resample; plenty for speech going into whisper."""
    audio = np.asarray(audio, dtype=np.float32)
    if src_rate == dst_rate or audio.size == 0:
        return audio
    n_out = max(1, round(audio.size * dst_rate / src_rate))
    src_times = np.arange(audio.size, dtype=np.float64) / src_rate
    dst_times = np.arange(n_out, dtype=np.float64) / dst_rate
    return np.interp(dst_times, src_times, audio).astype(np.float32)


def to_mono(frames: np.ndarray) -> np.ndarray:
    if frames.ndim == 1:
        return frames.astype(np.float32, copy=False)
    return frames.mean(axis=1).astype(np.float32)


# ------------------------------------------------------- loopback device ---

def find_loopback_device(name: str = "") -> int | None:
    """Index of the input device whose name contains ``name`` (case-insensitive).

    With no name, the first device matching a known loopback hint.
    """
    try:
        devices = list(sd.query_devices())
    except Exception as exc:  # noqa: BLE001 - PortAudio errors aren't typed
        log.warning("could not list audio devices: %s", exc)
        return None
    needles = (name.lower(),) if name else _AUTO_DEVICE_HINTS
    for needle in needles:
        for index, device in enumerate(devices):
            if device.get("max_input_channels", 0) > 0 and needle in str(
                device.get("name", "")
            ).lower():
                return index
    return None


class LoopbackDeviceSource:
    """Reads a loopback input device (BlackHole, a Pulse monitor, Stereo Mix)."""

    def __init__(self, device: int, sample_rate: int):
        self.device = device
        self.sample_rate = sample_rate
        info = sd.query_devices(device)
        self.name = str(info.get("name", device))
        self.channels = max(1, min(2, int(info.get("max_input_channels", 1))))
        self.device_rate = int(info.get("default_samplerate") or sample_rate)
        self._stream: sd.InputStream | None = None
        self._callback: AudioCallback | None = None

    @property
    def label(self) -> str:
        return f"device: {self.name}"

    def start(self, callback: AudioCallback) -> None:
        self._callback = callback
        self._stream = sd.InputStream(
            device=self.device,
            samplerate=self.device_rate,
            channels=self.channels,
            dtype="float32",
            callback=self._on_audio,
        )
        self._stream.start()

    def _on_audio(self, indata, frames, time_info, status) -> None:
        if self._callback is not None:
            mono = to_mono(np.asarray(indata))
            self._callback(resample(mono, self.device_rate, self.sample_rate))

    def stop(self) -> None:
        if self._stream is not None:
            self._stream.stop()
            self._stream.close()
            self._stream = None


# ------------------------------------------------------ ScreenCaptureKit ---

class ScreenCaptureKitSource:
    """macOS 13+ system audio via ScreenCaptureKit (no virtual device needed).

    Captures the main display's audio mix, excluding localflow's own sounds.
    Video is configured to a 2x2, 1 fps stream that we never read.
    """

    label = "ScreenCaptureKit"

    def __init__(self, sample_rate: int, timeout: float = 10.0):
        import CoreMedia  # noqa: F401 - fail fast when the framework is missing
        import ScreenCaptureKit  # noqa: F401

        self.sample_rate = sample_rate
        self.capture_rate = sample_rate if sample_rate in _SCK_RATES else 48000
        self.timeout = timeout
        self._stream = None
        self._output = None
        self._callback: AudioCallback | None = None

    def _wait(self, start: Callable[[Callable], None], what: str):
        done = threading.Event()
        result: dict = {}

        def handler(*args):
            result["args"] = args
            done.set()

        start(handler)
        if not done.wait(self.timeout):
            raise SystemAudioUnavailable(f"ScreenCaptureKit: {what} timed out")
        return result["args"]

    def start(self, callback: AudioCallback) -> None:
        import CoreMedia
        import ScreenCaptureKit as SCK

        self._callback = callback
        content, error = self._wait(
            SCK.SCShareableContent.getShareableContentWithCompletionHandler_,
            "listing shareable content",
        )
        if error is not None or content is None or not content.displays():
            raise SystemAudioUnavailable(
                "ScreenCaptureKit: no display available — grant Screen Recording "
                f"permission to this Python binary ({error})"
            )
        content_filter = SCK.SCContentFilter.alloc().initWithDisplay_excludingWindows_(
            content.displays()[0], []
        )
        config = SCK.SCStreamConfiguration.alloc().init()
        config.setCapturesAudio_(True)
        config.setExcludesCurrentProcessAudio_(True)
        config.setSampleRate_(self.capture_rate)
        config.setChannelCount_(1)
        config.setWidth_(2)
        config.setHeight_(2)
        config.setMinimumFrameInterval_(CoreMedia.CMTimeMake(1, 1))

        self._output = _sck_output_class().alloc().init()
        self._output.on_audio = self._on_sample_buffer
        stream = SCK.SCStream.alloc().initWithFilter_configuration_delegate_(
            content_filter, config, None
        )
        ok, err = stream.addStreamOutput_type_sampleHandlerQueue_error_(
            self._output, SCK.SCStreamOutputTypeAudio, None, None
        )
        if not ok:
            raise SystemAudioUnavailable(f"ScreenCaptureKit: addStreamOutput failed ({err})")
        (start_error,) = self._wait(stream.startCaptureWithCompletionHandler_, "start")
        if start_error is not None:
            raise SystemAudioUnavailable(f"ScreenCaptureKit: start failed ({start_error})")
        self._stream = stream

    def _on_sample_buffer(self, sample_buffer) -> None:
        import CoreMedia

        block = CoreMedia.CMSampleBufferGetDataBuffer(sample_buffer)
        if block is None:
            return
        length = CoreMedia.CMBlockBufferGetDataLength(block)
        if not length:
            return
        status, data = CoreMedia.CMBlockBufferCopyDataBytes(block, 0, length, None)
        if status != 0 or data is None:
            return
        # Configured for one channel; SCK delivers 32-bit float PCM.
        audio = np.frombuffer(bytes(data), dtype=np.float32)
        if self._callback is not None:
            self._callback(resample(audio, self.capture_rate, self.sample_rate))

    def stop(self) -> None:
        stream, self._stream = self._stream, None
        if stream is None:
            return
        try:
            self._wait(stream.stopCaptureWithCompletionHandler_, "stop")
        except SystemAudioUnavailable as exc:
            log.warning("%s", exc)


_SCK_OUTPUT_CLASS = None


def _sck_output_class():
    """Define the SCStreamOutput delegate lazily (PyObjC is macOS-only)."""
    global _SCK_OUTPUT_CLASS
    if _SCK_OUTPUT_CLASS is not None:
        return _SCK_OUTPUT_CLASS
    import objc
    import ScreenCaptureKit as SCK
    from Foundation import NSObject

    class LocalflowSystemAudioOutput(NSObject, protocols=[objc.protocolNamed("SCStreamOutput")]):
        on_audio = None

        def stream_didOutputSampleBuffer_ofType_(self, stream, sample_buffer, output_type):
            if output_type != SCK.SCStreamOutputTypeAudio or self.on_audio is None:
                return
            try:
                self.on_audio(sample_buffer)
            except Exception:  # never raise into the capture queue
                log.exception("system audio buffer conversion failed")

    _SCK_OUTPUT_CLASS = LocalflowSystemAudioOutput
    return _SCK_OUTPUT_CLASS


# -------------------------------------------------------------- factory ---

def open_system_audio(sample_rate: int, device: str = ""):
    """Pick a system-audio source; raises SystemAudioUnavailable with a hint.

    A configured ``device`` always means "use that loopback device". Otherwise
    macOS tries ScreenCaptureKit first, then any known loopback device.
    """
    if not device and sys.platform == "darwin":
        try:
            return ScreenCaptureKitSource(sample_rate)
        except ImportError:
            log.info("ScreenCaptureKit bindings not installed; trying a loopback device")
    index = find_loopback_device(device)
    if index is not None:
        return LoopbackDeviceSource(index, sample_rate)
    if device:
        raise SystemAudioUnavailable(f"no input device matching {device!r}")
    hint = (
        "install the ScreenCaptureKit bindings (pip install -e '.[system-audio]') "
        "or set up BlackHole and set [meetings] system_audio_device"
        if sys.platform == "darwin"
        else "set [meetings] system_audio_device to a loopback/monitor input device"
    )
    raise SystemAudioUnavailable(f"no system audio source found: {hint}")
