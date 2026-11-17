"""Generate committed assets with Pillow 12.0.0: python scripts/generate_app_icon.py."""

import math
import struct
from io import BytesIO
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]


def render() -> Image.Image:
    size = 1024
    scale = 2
    canvas = Image.new("RGBA", (size * scale, size * scale))
    mask = Image.new("L", canvas.size)
    ImageDraw.Draw(mask).rounded_rectangle((80, 80, 1968, 1968), radius=430, fill=255)
    pixels = canvas.load()
    for y in range(canvas.height):
        for x in range(canvas.width):
            t = (0.4 * x + 0.6 * y) / (canvas.width - 1)
            pixels[x, y] = (int(32 + 32 * t), int(153 - 95 * t), int(238 + 10 * t), 255)
    canvas.putalpha(mask)
    draw = ImageDraw.Draw(canvas)
    draw.rounded_rectangle((838, 480, 1210, 1150), radius=186, fill="white")
    angles = [math.pi * (180 - step) / 180 for step in range(181)]
    outer = [
        (1024 + 370 * math.cos(angle), 1030 + 370 * math.sin(angle))
        for angle in angles
    ]
    inner = [
        (1024 + 304 * math.cos(angle), 1030 + 304 * math.sin(angle))
        for angle in reversed(angles)
    ]
    draw.polygon(outer + inner, fill="white")
    draw.rectangle((654, 913, 720, 1031), fill="white")
    draw.rectangle((1328, 913, 1394, 1031), fill="white")
    draw.ellipse((654, 880, 720, 946), fill="white")
    draw.ellipse((1328, 880, 1394, 946), fill="white")
    draw.rounded_rectangle((991, 1360, 1057, 1550), radius=33, fill="white")
    draw.rounded_rectangle((820, 1510, 1228, 1576), radius=33, fill="white")
    return canvas.resize((size, size), Image.Resampling.LANCZOS)


def main() -> None:
    destination = ROOT / "packaging" / "macos"
    destination.mkdir(parents=True, exist_ok=True)
    icon = render()
    icon.save(destination / "LocalFlow.png", optimize=True)
    chunks = []
    for kind, size in (
        (b"icp4", 16), (b"icp5", 32), (b"icp6", 64), (b"ic07", 128),
        (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024),
    ):
        png = BytesIO()
        icon.resize((size, size), Image.Resampling.LANCZOS).save(png, format="PNG")
        data = png.getvalue()
        chunks.append(struct.pack(">4sI", kind, len(data) + 8) + data)
    payload = b"".join(chunks)
    (destination / "LocalFlow.icns").write_bytes(b"icns" + struct.pack(">I", len(payload) + 8) + payload)


if __name__ == "__main__":
    main()
