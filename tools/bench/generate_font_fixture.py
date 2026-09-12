#!/usr/bin/env python3
"""Regenerate the tiny synthetic X4FT glyph fixture used by bench-serial.

All printable ASCII entries deliberately share the hand-drawn 8x8 A bitmap;
the fixture validates SD metrics/bitmap caching, not typeface coverage. Metrics
and bitmap occupy different sectors so a cold glyph requires real SD reads.
"""
import hashlib
from pathlib import Path
import struct


def fixture() -> bytes:
    data = bytearray(2056)
    name = b"Marigold bench fixture"
    struct.pack_into("<4sHHIQHHHHIIII", data, 0, b"X4FT", 1, 64, len(data),
                     0, 1, 95, len(name), 1, 64, 128, 318, 512)
    struct.pack_into("<BBBBIIIIIII", data, 64, 22, 0, 12, 8, 1,
                     512, 95, 2048, 8, 2056, 0)
    for index in range(95):
        struct.pack_into("<H", data, 128 + index * 2, 0x20 + index)
        struct.pack_into("<IHBBbbH", data, 512 + index * 12, 0, 8, 8, 8, 0, -8, 9 * 16)
    data[318:318 + len(name)] = name
    data[2048:] = bytes([0x18, 0x24, 0x42, 0x7e, 0x42, 0x42, 0x42, 0])
    data[12:20] = hashlib.blake2s(data, digest_size=8).digest()
    return bytes(data)


if __name__ == "__main__":
    output = Path(__file__).resolve().parents[2] / "fw/src/custom_font/bench_font.fnt"
    output.write_bytes(fixture())
    print(output)
