"""Derive orientation-small.png from orientation-test.png (Jan Steinke's
640x900 orientation marker): a 1/3-scale copy, 213x300, small enough to
show unscaled in every page box with text above it on the same page.

Standard library only. Each output pixel is the rounded mean of a 3x3 block
of the source (the last source column is dropped). Run from the repo root:

    python3 -I fixtures/images/shrink_orientation_marker.py
"""

import struct
import zlib

SOURCE = "fixtures/images/orientation-test.png"
TARGET = "fixtures/images/orientation-small.png"
SCALE = 3


def read_gray8(path):
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    pos, idat, header = 8, b"", None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        if kind == b"IHDR":
            header = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat += body
        pos += 12 + length
    width, height, depth, color, _, _, interlace = header
    assert (depth, color, interlace) == (8, 0, 0), "8-bit gray, not interlaced"
    raw = zlib.decompress(idat)
    rows, previous = [], bytearray(width)
    for y in range(height):
        start = y * (width + 1)
        kind, line = raw[start], bytearray(raw[start + 1:start + 1 + width])
        for x in range(width):
            left = line[x - 1] if x else 0
            up = previous[x]
            corner = previous[x - 1] if x else 0
            if kind == 1:
                line[x] = (line[x] + left) & 255
            elif kind == 2:
                line[x] = (line[x] + up) & 255
            elif kind == 3:
                line[x] = (line[x] + (left + up) // 2) & 255
            elif kind == 4:
                p = left + up - corner
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - corner)
                pred = left if pa <= pb and pa <= pc else up if pb <= pc else corner
                line[x] = (line[x] + pred) & 255
        rows.append(line)
        previous = line
    return width, height, rows


def write_gray8(path, width, height, rows):
    def chunk(kind, body):
        crc = zlib.crc32(kind + body) & 0xFFFFFFFF
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", crc)

    raw = b"".join(b"\x00" + bytes(row) for row in rows)
    with open(path, "wb") as out:
        out.write(b"\x89PNG\r\n\x1a\n")
        out.write(chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 0, 0, 0, 0)))
        out.write(chunk(b"IDAT", zlib.compress(raw, 9)))
        out.write(chunk(b"IEND", b""))


width, height, rows = read_gray8(SOURCE)
small_w, small_h = width // SCALE, height // SCALE
small = []
for y in range(small_h):
    band = rows[y * SCALE:(y + 1) * SCALE]
    small.append(bytearray(
        (sum(row[x * SCALE + dx] for row in band for dx in range(SCALE)) + SCALE * SCALE // 2)
        // (SCALE * SCALE)
        for x in range(small_w)
    ))
write_gray8(TARGET, small_w, small_h, small)
print(TARGET, small_w, small_h)
