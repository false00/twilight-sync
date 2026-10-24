"""Generate the original moon icon's PNG and Windows ICO assets without external tools."""
from pathlib import Path
import struct
import zlib

ROOT = Path(__file__).resolve().parent.parent / "assets"
SIZES = (16, 24, 32, 48, 64, 128, 256)


def pixel(x, y):
    dx = max(abs(x - 128) - 64, 0)
    dy = max(abs(y - 128) - 64, 0)
    if dx * dx + dy * dy > 48 * 48:
        return (0, 0, 0, 0)
    moon = (x - 112) ** 2 + (y - 132) ** 2 <= 76 ** 2
    cutout = (x - 150) ** 2 + (y - 102) ** 2 < 74 ** 2
    star = (x - 188) ** 2 + (y - 80) ** 2 <= 6 ** 2
    star |= (x - 195) ** 2 + (y - 108) ** 2 <= 3 ** 2
    if (moon and not cutout) or star:
        return (255, 255, 255, 255)
    fraction = min(max((y - 16) / 224, 0), 1)
    return (0, round(120 - 42 * fraction), round(212 - 56 * fraction), 255)


def png(size):
    rgba = bytearray()
    for y in range(size):
        row = bytearray()
        for x in range(size):
            samples = [
                pixel((x + (sx + 0.5) / 4) * 256 / size,
                      (y + (sy + 0.5) / 4) * 256 / size)
                for sy in range(4) for sx in range(4)
            ]
            alpha = sum(sample[3] for sample in samples)
            color = [
                round(sum(sample[channel] * sample[3] for sample in samples) / alpha)
                if alpha else 0 for channel in range(3)
            ]
            row.extend((*color, round(alpha / 16)))
        rgba.extend(b"\0" + row)

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rgba, 9))
        + chunk(b"IEND", b"")
    )


images = [(size, png(size)) for size in SIZES]
offset = 6 + 16 * len(images)
directory = bytearray(struct.pack("<HHH", 0, 1, len(images)))
for size, image in images:
    dimension = size if size < 256 else 0
    directory.extend(struct.pack("<BBBBHHII", dimension, dimension, 0, 0, 1, 32, len(image), offset))
    offset += len(image)
ROOT.mkdir(exist_ok=True)
(ROOT / "twilight-sync.ico").write_bytes(directory + b"".join(image for _, image in images))
for size, image in images:
    if size in (32, 256):
        (ROOT / f"app-icon-{size}.png").write_bytes(image)
print("Generated 16-256px Windows icon and matching 32px/256px PNG assets.")
