#!/usr/bin/env python3
"""Generate the extension icons (16/48/128) — no image library, raw PNG.

A "portal" motif: a dark rounded tile with two concentric rings and a bright
core — an egress point. Brand colours match the Platform Manager (purple
#a371f7 on #0d1117). Deterministic: same bytes every run (reproducible build).
"""
import struct
import zlib
import math
import os

BG = (13, 17, 23)        # #0d1117
RING = (163, 113, 247)   # #a371f7
CORE = (224, 224, 255)


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def pixel(x, y, n):
    c = (n - 1) / 2.0
    # Rounded-tile alpha: transparent corners (a soft superellipse mask).
    nx, ny = (x - c) / (n / 2.0), (y - c) / (n / 2.0)
    tile = (abs(nx) ** 4 + abs(ny) ** 4)
    if tile > 1.0:
        return (0, 0, 0, 0)
    edge = max(0.0, min(1.0, (1.0 - tile) * n * 0.25))  # soft outer edge

    r = math.hypot(x - c, y - c) / (n / 2.0)  # 0 at centre, 1 at edge
    col = BG
    # Two rings + a bright core.
    for lo, hi in ((0.42, 0.58), (0.70, 0.86)):
        if lo <= r <= hi:
            mid = (lo + hi) / 2
            t = 1.0 - abs(r - mid) / ((hi - lo) / 2)
            col = lerp(BG, RING, max(0.0, min(1.0, t)))
    if r < 0.22:
        col = lerp(CORE, RING, min(1.0, r / 0.22))
    a = round(255 * edge)
    return (col[0], col[1], col[2], a)


def png(n):
    raw = bytearray()
    for y in range(n):
        raw.append(0)  # filter type 0
        for x in range(n):
            raw.extend(pixel(x, y, n))
    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))
    ihdr = struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
            + chunk(b"IEND", b""))


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    for n in (16, 48, 128):
        with open(os.path.join(here, f"icon{n}.png"), "wb") as f:
            f.write(png(n))
        print(f"wrote icon{n}.png")


if __name__ == "__main__":
    main()
