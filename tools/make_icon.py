#!/usr/bin/env python3
"""Regenerates the FeatherClick icon assets.

    python tools/make_icon.py

Writes:
  assets/icon.ico    multi-size Windows icon, classic 32-bit DIB entries
  assets/icon.rgba   64x64 raw RGBA, embedded in the binary for the window icon

The mark is drawn procedurally at 3x and box-downsampled, so the outputs are
reproducible and the repo needs no binary image editor.
"""

from __future__ import annotations

import math
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BG = (0x12, 0x15, 0x1D)  # surface
RING = (0x4C, 0x8D, 0xFF)  # accent
DOT = (0xFF, 0xFF, 0xFF)
SS = 3  # supersampling factor
ICO_SIZES = (16, 32, 48, 64, 128, 256)
RGBA_SIZE = 64


def _rounded_rect_hit(x: float, y: float, radius: float) -> bool:
    dx = abs(x - 0.5) - (0.5 - radius)
    dy = abs(y - 0.5) - (0.5 - radius)
    if dx <= 0.0 or dy <= 0.0:
        return max(dx, dy) <= 0.0
    return math.hypot(dx, dy) <= radius


def _sample(x: float, y: float) -> tuple[int, int, int, int]:
    if not _rounded_rect_hit(x, y, 0.215):
        return (0, 0, 0, 0)

    dx, dy = x - 0.5, y - 0.5
    r = math.hypot(dx, dy)
    colour = BG
    if r <= 0.105:
        colour = DOT
    elif 0.235 <= r <= 0.315:
        colour = RING
    elif 0.375 <= r <= 0.435:
        # Four tick marks on the axes: a click "pulse".
        angle = abs(math.degrees(math.atan2(dy, dx))) % 90.0
        if min(angle, 90.0 - angle) <= 9.0:
            colour = RING
    return (*colour, 255)


def render(size: int) -> list[list[tuple[int, int, int, int]]]:
    """Returns `size` rows of `size` RGBA pixels, top-down."""
    big = size * SS
    samples = [[_sample((i + 0.5) / big, (j + 0.5) / big) for i in range(big)] for j in range(big)]
    rows: list[list[tuple[int, int, int, int]]] = []
    for j in range(size):
        row = []
        for i in range(size):
            r = g = b = a = 0
            for sj in range(SS):
                for si in range(SS):
                    sr, sg, sb, sa = samples[j * SS + sj][i * SS + si]
                    # Premultiplied accumulation keeps edges from fringing to black.
                    r += sr * sa
                    g += sg * sa
                    b += sb * sa
                    a += sa
            if a == 0:
                row.append((0, 0, 0, 0))
            else:
                n = SS * SS
                row.append((r // a, g // a, b // a, a // n))
        rows.append(row)
    return rows


def ico_entry(rows: list[list[tuple[int, int, int, int]]]) -> bytes:
    size = len(rows)
    # XOR bitmap: bottom-up BGRA rows.
    xor = bytearray()
    for row in reversed(rows):
        for r, g, b, a in row:
            xor += bytes((b, g, r, a))
    # AND mask: 1bpp, rows padded to 4 bytes. Zero everywhere; alpha does the work.
    stride = ((size + 31) // 32) * 4
    mask = bytes(stride * size)
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, len(xor) + len(mask), 0, 0, 0, 0)
    return header + bytes(xor) + mask


def write_ico(path: Path) -> None:
    images = [(s, ico_entry(render(s))) for s in ICO_SIZES]
    out = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    offset = 6 + 16 * len(images)
    for size, data in images:
        dim = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    for _, data in images:
        out += data
    path.write_bytes(out)


def write_rgba(path: Path) -> None:
    rows = render(RGBA_SIZE)
    path.write_bytes(bytes(channel for row in rows for pixel in row for channel in pixel))


def write_png(path: Path, size: int) -> None:
    """Minimal RGBA PNG encoder (no third-party imports)."""
    rows = render(size)
    raw = b"".join(b"\x00" + bytes(c for pixel in row for c in pixel) for row in rows)

    def chunk(kind: bytes, payload: bytes) -> bytes:
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    path.write_bytes(png)


if __name__ == "__main__":
    assets = ROOT / "assets"
    assets.mkdir(exist_ok=True)
    write_ico(assets / "icon.ico")
    write_rgba(assets / "icon.rgba")
    write_png(assets / "icon.png", 256)
    for name in ("icon.ico", "icon.rgba", "icon.png"):
        print("wrote assets/%s (%d bytes)" % (name, (assets / name).stat().st_size))
