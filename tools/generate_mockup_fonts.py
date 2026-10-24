#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "pillow==12.3.0",
# ]
# ///
"""Rasterize extra Literata sizes for the host-side design mockup gallery.

Emits tools/preview/src/mockup_fonts_generated.rs. These sizes are mockup-only;
the firmware display crate keeps its single 22px set until a design direction
is chosen and the needed sizes are promoted into display/src.
"""
from pathlib import Path

from PIL import Image, ImageDraw

from fontgen_common import (
    ADVANCE_SCALE,
    THRESHOLD,
    FontModule,
    ensure_fonts,
    load_font,
    missing_codepoints,
)

ROOT = Path(__file__).resolve().parents[1]
FONT_DIR = ROOT / "tools" / "fonts" / "literata"
OUT = ROOT / "tools" / "preview" / "src" / "mockup_fonts_generated.rs"

# (size name, px, line_height, baseline, [(style name, ttf)])
SETS = [
    ("SMALL", 16, 22, 17, [
        ("REGULAR", "Literata-Regular.ttf"),
        ("BOLD", "Literata-Bold.ttf"),
        ("ITALIC", "Literata-Italic.ttf"),
    ]),
    ("TITLE", 30, 41, 31, [
        ("REGULAR", "Literata-Regular.ttf"),
        ("BOLD", "Literata-Bold.ttf"),
        ("ITALIC", "Literata-Italic.ttf"),
    ]),
    ("DISPLAY", 46, 62, 48, [
        ("REGULAR", "Literata-Regular.ttf"),
    ]),
]

RANGES = [
    (0x0020, 0x007E),  # Basic Latin
    (0x00A0, 0x00FF),  # Latin-1 Supplement
    (0x2010, 0x2026),  # dashes, curly quotes, bullet, ellipsis
    (0x2190, 0x2193),  # arrows
    (0x25A0, 0x25CF),  # geometric shapes
]


def codepoints():
    values = []
    for start, end in RANGES:
        values.extend(range(start, end + 1))
    return sorted(set(values))


def glyph(font, code):
    ch = chr(code)
    bbox = font.getbbox(ch, anchor="ls")
    advance = max(int(round(font.getlength(ch) * ADVANCE_SCALE)), ADVANCE_SCALE)
    if bbox is None:
        return (0, 0, 0, 0, max(advance, 1), [])
    left, top, right, bottom = bbox
    width = max(0, right - left)
    height = max(0, bottom - top)
    if width == 0 or height == 0:
        return (0, 0, left, top, max(advance, 1), [])
    image = Image.new("L", (width, height), 0)
    draw = ImageDraw.Draw(image)
    draw.text((-left, -top), ch, font=font, fill=255, anchor="ls")
    rows = []
    for y in range(height):
        byte = 0
        bits = 0
        for x in range(width):
            if image.getpixel((x, y)) >= THRESHOLD:
                byte |= 0x80 >> bits
            bits += 1
            if bits == 8:
                rows.append(byte)
                byte = 0
                bits = 0
        if bits:
            rows.append(byte)
    return (width, height, left, top, max(advance, 1), rows)


def main():
    ensure_fonts(FONT_DIR, sorted({name for *_, styles in SETS for _, name in styles}))
    cps = codepoints()
    module = FontModule(
        "tools/generate_mockup_fonts.py", "MOCKUP", "display::font", header=["#![allow(dead_code)]\n"]
    )
    for size_name, px, line_height, baseline, styles in SETS:
        for style, filename in styles:
            path = FONT_DIR / filename
            font = load_font(path, px)
            glyphs = [glyph(font, code) for code in cps]
            module.add_face(
                f"{size_name}_{style}", line_height, baseline, cps, glyphs, [],
                missing_codepoints(path, cps),
            )
    module.write(OUT)

if __name__ == "__main__":
    main()
