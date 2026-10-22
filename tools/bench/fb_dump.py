#!/usr/bin/env python3
"""Capture the panel's framebuffer from a `bench-serial` build as a PNG.

Sends `f` (arm a dump after the next render), then the given navigation keys,
and collects the `fbrow` lines the display task prints after that render. The
PNG is turned to the reading orientation given by --frame, so it shows the
page as the reader holds the device.

    python3 tools/bench/fb_dump.py --port /dev/ttyACM0 --keys n --frame landscape --out page.png

Keys use the send.py vocabulary (o/n/p/b/s). With --keys "" the dump waits for
whatever render happens next. Standard library only.
"""

import argparse
import fcntl
import os
import re
import struct
import sys
import termios
import time
import zlib
from pathlib import Path

BEGIN = re.compile(rb"bench: fb_dump begin width=(\d+) height=(\d+) row_bytes=(\d+)")
ROW = re.compile(rb"fbrow (\d+) (w|[0-9a-f]+)")


def open_port(port: str) -> int:
    fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    attrs = termios.tcgetattr(fd)
    attrs[0] = attrs[1] = attrs[3] = 0
    attrs[2] = termios.CREAD | termios.CLOCAL | termios.CS8
    termios.tcsetattr(fd, termios.TCSANOW, attrs)
    # Same line state as send.py: DTR set (console TX on), RTS clear (no reset).
    fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack("i", termios.TIOCM_DTR))
    fcntl.ioctl(fd, termios.TIOCMBIC, struct.pack("i", termios.TIOCM_RTS))
    return fd


def capture(port: str, keys: str, interval: float, timeout: float) -> tuple[int, int, int, dict[int, bytes]]:
    fd = open_port(port)
    buffer = bytearray()

    def drain_for(seconds: float) -> bool:
        # Keep reading while waiting: the kernel's tty buffer is small, and a
        # dump that arrives while nobody reads overflows it.
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            try:
                buffer.extend(os.read(fd, 65536))
            except BlockingIOError:
                time.sleep(0.002)
            if b"bench: fb_dump end" in buffer:
                return True
        return False

    try:
        os.write(fd, b"f")
        drain_for(0.2)
        for key in keys:
            os.write(fd, key.encode("ascii"))
            if drain_for(interval):
                break
        drain_for(timeout)
    finally:
        os.close(fd)
    return parse(bytes(buffer))


def parse(data: bytes) -> tuple[int, int, int, dict[int, bytes]]:
    begin = BEGIN.search(data)
    if not begin:
        sys.exit("no fb_dump in the capture (is this a bench-serial build?)")
    width, height, row_bytes = (int(v) for v in begin.groups())
    rows = {}
    for match in ROW.finditer(data, begin.end()):
        y, hex_row = int(match.group(1)), match.group(2)
        if hex_row == b"w":
            rows[y] = b"\xff" * row_bytes
        elif len(hex_row) == 2 * row_bytes:
            rows[y] = bytes.fromhex(hex_row.decode())
    return width, height, row_bytes, rows


def to_png(path: Path, width: int, height: int, row_bytes: int, rows: dict[int, bytes], frame: str) -> int:
    def native(x: int, y: int) -> int:
        row = rows.get(y)
        if row is None:
            return 128  # a missing row shows as gray
        return 255 if row[x // 8] >> (7 - x % 8) & 1 else 0

    # Frame coordinates to native, as display::fb::Framebuffer::map does.
    if frame == "landscape":
        out_w, out_h, pixel = width, height, lambda x, y: native(x, height - 1 - y)
    elif frame == "flipped":
        out_w, out_h, pixel = width, height, lambda x, y: native(width - 1 - x, y)
    elif frame == "portrait":
        out_w, out_h, pixel = height, width, lambda x, y: native(width - 1 - y, height - 1 - x)
    else:
        out_w, out_h, pixel = width, height, native
    raw = b"".join(b"\0" + bytes(pixel(x, y) for x in range(out_w)) for y in range(out_h))

    def chunk(kind: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", out_w, out_h, 8, 0, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )
    return len(rows)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", default="/dev/cu.usbmodem101")
    parser.add_argument("--keys", default="n", help="keys to send after arming (o/n/p/b/s)")
    parser.add_argument("--interval", type=float, default=1.0)
    parser.add_argument("--timeout", type=float, default=20.0)
    parser.add_argument("--frame", choices=["landscape", "flipped", "portrait", "native"], default="landscape")
    parser.add_argument("--from-log", type=Path, help="convert an existing capture instead of reading the port")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if any(key not in "onpbs" for key in args.keys):
        parser.error("--keys takes only o/n/p/b/s")
    if args.from_log:
        width, height, row_bytes, rows = parse(args.from_log.read_bytes())
    else:
        width, height, row_bytes, rows = capture(args.port, args.keys, args.interval, args.timeout)
    got = to_png(args.out, width, height, row_bytes, rows, args.frame)
    print(f"{args.out}: {got}/{height} rows of a {width}x{height} framebuffer, frame={args.frame}")
    return 0 if got == height else 1


if __name__ == "__main__":
    sys.exit(main())
