#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "pillow==12.3.0",
# ]
# ///
"""Rasterize the Imprint UI Literata sizes for the firmware display crate.

Emits display/src/literata_extra_generated.rs: a 16px apparatus set
(regular/bold/italic) and a 46px display set (regular). The 22px reading
set stays in literata_generated.rs via tools/generate_literata.py.

Reproducible only with the pinned Pillow 12.3.0 / FreeType 2.14.3; an
intentional change must refresh the goldens, and the script refuses to
overwrite the checked-in output without --force.
"""
from pathlib import Path

from fontgen_common import (
    FontModule,
    codepoints_from_ranges,
    ensure_fonts,
    kerning_entries,
    load_font,
    missing_codepoints,
    parse_generator_args,
    rasterize_glyph,
    threshold_for,
)

ROOT = Path(__file__).resolve().parents[1]
FONT_DIR = ROOT / "tools" / "fonts" / "literata"
OUT = ROOT / "display" / "src" / "literata_extra_generated.rs"

# (size name, px, line_height, baseline, [(style name, ttf)])
SETS = [
    ("SMALL", 16, 22, 17, [
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
]


def codepoints():
    return codepoints_from_ranges(RANGES)


def main(argv=None):
    parse_generator_args(__doc__, OUT, reading_face=False, argv=argv)
    ensure_fonts(FONT_DIR, sorted({name for *_, styles in SETS for _, name in styles}))
    cps = codepoints()
    module = FontModule("tools/generate_literata_extra.py", "EXTRA")
    for size_name, px, line_height, baseline, styles in SETS:
        for style, filename in styles:
            path = FONT_DIR / filename
            font = load_font(path, px)
            glyphs = [rasterize_glyph(font, code, threshold_for(style)) for code in cps]
            module.add_face(
                f"LITERATA_{size_name}_{style}", line_height, baseline, cps, glyphs,
                kerning_entries(path, cps, px), missing_codepoints(path, cps),
            )
    module.write(OUT)


if __name__ == "__main__":
    main()
