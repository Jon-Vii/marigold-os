"""Draw the two images the inline-image tests and goldens set inside lines:

- inline-symbol.png, 17x12: an outlined parallelogram, the size of the
  symbol Euclid sets mid-sentence ("Complete the [symbol] AEBG").
- inline-initial.png, 75x75: a decorated initial "T" on a hatched ground in
  a frame, the size of the drop caps in Rackham's Alice. Inside a line it
  is fitted to the 64-row cap.

Both are 8-bit gray, pure black and white apart from the initial's hatched
ground, so the 1 bpp dither keeps their edges. Standard library only. Run
from the repo root:

    python3 -I fixtures/images/make_inline_images.py
"""

import struct
import zlib

WHITE, BLACK = 255, 0


def write_gray8(path, rows):
    def chunk(kind, body):
        crc = zlib.crc32(kind + body) & 0xFFFFFFFF
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", crc)

    height, width = len(rows), len(rows[0])
    raw = b"".join(b"\x00" + bytes(row) for row in rows)
    with open(path, "wb") as out:
        out.write(b"\x89PNG\r\n\x1a\n")
        out.write(chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 0, 0, 0, 0)))
        out.write(chunk(b"IDAT", zlib.compress(raw, 9)))
        out.write(chunk(b"IEND", b""))


def near_segment(px, py, a, b, reach):
    """Whether pixel centre (px, py) lies within `reach` of segment a-b."""
    (ax, ay), (bx, by) = a, b
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    cx, cy = ax + t * dx, ay + t * dy
    return (px - cx) ** 2 + (py - cy) ** 2 <= reach * reach


def symbol():
    width, height = 17, 12
    corners = [(4.5, 1.5), (15.5, 1.5), (12.5, 10.5), (1.5, 10.5)]
    edges = list(zip(corners, corners[1:] + corners[:1]))
    return [
        [
            BLACK
            if any(near_segment(x + 0.5, y + 0.5, a, b, 0.75) for a, b in edges)
            else WHITE
            for x in range(width)
        ]
        for y in range(height)
    ]


def initial():
    size = 75
    rows = []
    for y in range(size):
        row = []
        for x in range(size):
            frame = x < 3 or y < 3 or x >= size - 3 or y >= size - 3
            inner_rule = x in (6, size - 7) or y in (6, size - 7)
            # The letter: a top bar with serifs at its ends, and a stem with
            # a foot.
            bar = 14 <= x <= 60 and 13 <= y <= 22
            serifs = (14 <= x <= 19 or 55 <= x <= 60) and 22 < y <= 28
            stem = 31 <= x <= 43 and 22 < y <= 62
            foot = 25 <= x <= 49 and 58 <= y <= 62
            if frame or bar or serifs or stem or foot:
                row.append(BLACK)
            elif inner_rule and 6 <= x <= size - 7 and 6 <= y <= size - 7:
                row.append(BLACK)
            elif 6 < x < size - 7 and 6 < y < size - 7 and (x + y) % 6 == 0:
                # A hatched ground, mid gray, which the dither renders as
                # a fine stipple.
                row.append(150)
            else:
                row.append(WHITE)
        rows.append(row)
    return rows


write_gray8("fixtures/images/inline-symbol.png", symbol())
write_gray8("fixtures/images/inline-initial.png", initial())
