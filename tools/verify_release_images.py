#!/usr/bin/env python3
"""Validate canonical ESP32-C3 release images before publishing them."""
import hashlib
from pathlib import Path
import struct
import sys


def verify(path, device):
    data = path.read_bytes()
    assert 288 <= len(data) <= 0x640000, "image does not fit the app slot"
    assert data[0] == 0xE9 and 1 <= data[1] <= 16, "invalid app header"
    assert struct.unpack_from('<H', data, 12)[0] == 5, "not an ESP32-C3 image"
    assert data[23] == 1, "missing SHA-256 trailer"
    assert struct.unpack_from('<I', data, 32)[0] == 0xABCD5432, "missing app descriptor"
    assert data[80:112].split(b'\0')[0] == f'xteink-{device}'.encode(), "wrong device image"
    for marker in (b'bench: serial_ready', b'bench: font_selftest', b'bench: serial_sleep'):
        assert marker not in data, "development bench feature enabled"
    position, checksum = 24, 0xEF
    for _ in range(data[1]):
        assert position + 8 <= len(data), "truncated segment header"
        size = struct.unpack_from('<I', data, position + 4)[0]
        position += 8
        assert position + size <= len(data), "truncated segment"
        for value in data[position:position + size]:
            checksum ^= value
        position += size
    end = (position + 16) & ~15
    assert end + 32 == len(data), "unexpected image length"
    assert data[end - 1] == checksum, "invalid XOR checksum"
    assert hashlib.sha256(data[:end]).digest() == data[end:], "invalid SHA-256 trailer"
    print(f'{path.name}: valid {device} app image ({len(data)} bytes)')


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    for device in ('x4', 'x3'):
        path = directory / f'firmware-{device}.bin'
        try:
            verify(path, device)
        except (AssertionError, OSError, struct.error) as error:
            raise SystemExit(f'{path}: {error}') from error
