#!/usr/bin/env python3
"""Validate canonical ESP32-C3 release images before publishing them.

    verify_release_images.py [--expect-version TAG] DIR
    verify_release_images.py [--expect-version TAG] --device {x4,x3} IMAGE...

A directory must hold firmware-x4.bin and firmware-x3.bin (the published
release assets); image files are checked against the given device. With
--expect-version, the app descriptor's version field must equal TAG (the
firmware build truncates it to 31 bytes, and so does the comparison).
"""
import argparse
import hashlib
from pathlib import Path
import struct
import sys

DEVICES = ('x4', 'x3')
APP_SLOT_BYTES = 0x640000  # app0/app1 size in partitions.csv
DESCRIPTOR_MAGIC = 0xABCD5432
VERSION_FIELD = slice(48, 80)  # descriptor at 0x20, version at +16
PROJECT_FIELD = slice(80, 112)  # descriptor at 0x20, project_name at +48

# Strings only development features print. Release images must not carry
# them: bench-serial adds USB button injection and a timer wake, and
# ota-selftest rewrites the inactive OTA slot and otadata on first boot.
DEVELOPMENT_MARKERS = (
    (b'bench: serial_ready', 'bench-serial'),
    (b'bench: font_selftest', 'bench-serial'),
    (b'bench: serial_sleep', 'bench-serial'),
    (b'selftest: ', 'ota-selftest'),
)


class InvalidImage(Exception):
    pass


def require(condition, message):
    if not condition:
        raise InvalidImage(message)


def field_text(data, field):
    return data[field].split(b'\0')[0]


def verify(path, device, expect_version=None):
    data = path.read_bytes()
    require(288 <= len(data) <= APP_SLOT_BYTES, "image does not fit the app slot")
    require(data[0] == 0xE9 and 1 <= data[1] <= 16, "invalid app header")
    require(struct.unpack_from('<H', data, 12)[0] == 5, "not an ESP32-C3 image")
    require(data[23] == 1, "missing SHA-256 trailer")
    require(struct.unpack_from('<I', data, 32)[0] == DESCRIPTOR_MAGIC, "missing app descriptor")
    require(field_text(data, PROJECT_FIELD) == f'xteink-{device}'.encode(), "wrong device image")
    version = field_text(data, VERSION_FIELD)
    if expect_version is not None:
        expected = expect_version.encode()[:VERSION_FIELD.stop - VERSION_FIELD.start - 1]
        require(
            version == expected,
            f"descriptor version {version.decode(errors='replace')!r}, "
            f"expected {expected.decode(errors='replace')!r}",
        )
    for marker, feature in DEVELOPMENT_MARKERS:
        require(marker not in data, f"development feature {feature} enabled")
    position, checksum = 24, 0xEF
    for _ in range(data[1]):
        require(position + 8 <= len(data), "truncated segment header")
        size = struct.unpack_from('<I', data, position + 4)[0]
        position += 8
        require(position + size <= len(data), "truncated segment")
        for value in data[position:position + size]:
            checksum ^= value
        position += size
    end = (position + 16) & ~15
    require(end + 32 == len(data), "unexpected image length")
    require(data[end - 1] == checksum, "invalid XOR checksum")
    require(hashlib.sha256(data[:end]).digest() == data[end:], "invalid SHA-256 trailer")
    label = version.decode(errors='replace') or 'no version'
    print(f'{path.name}: valid {device} app image, {label} ({len(data)} bytes)')


def main(argv):
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('paths', nargs='+', type=Path, metavar='PATH')
    parser.add_argument('--device', choices=DEVICES,
                        help='check image files for this device instead of a release directory')
    parser.add_argument('--expect-version', metavar='TAG',
                        help='require this version string in the app descriptor')
    args = parser.parse_args(argv)

    if args.device:
        targets = [(path, args.device) for path in args.paths]
    else:
        if len(args.paths) != 1 or not args.paths[0].is_dir():
            parser.error('without --device, pass one release directory')
        targets = [(args.paths[0] / f'firmware-{device}.bin', device) for device in DEVICES]

    for path, device in targets:
        try:
            verify(path, device, args.expect_version)
        except (InvalidImage, OSError, struct.error) as error:
            raise SystemExit(f'{path}: {error}') from error


if __name__ == '__main__':
    main(sys.argv[1:])
