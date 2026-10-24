#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "pillow==12.3.0",
# ]
# ///
"""Rasterize the SemiBold reading faces for the Type Weight "Heavier" option.

Emits display/src/literata_semibold_generated.rs: Literata SemiBold (600) and
SemiBold Italic at the three reading sizes (19/22/26px), matching the
line_height/baseline of the Regular sets so a weight change is vertically
interchangeable at a given size. Heavy mode maps Regular->SemiBold and
Italic->SemiBoldItalic; Bold emphasis keeps the existing Bold (700) set, which
stays a step heavier so emphasis remains visible.

Like the other generators, the default Regular sets are deliberately not
touched here: glyph rasterization differs across Pillow/freetype builds, so
this file is a frozen artifact -- generate once, check in, do not re-run
casually or it churns the Heavy-mode goldens. It is reproducible only with
the pinned Pillow 12.3.0 / FreeType 2.14.3, an intentional change must
refresh the goldens and bump READER_LAYOUT_VERSION, and it refuses to
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
OUT = ROOT / "display" / "src" / "literata_semibold_generated.rs"

FONTS = [
    ("SEMIBOLD", "Literata-SemiBold.ttf"),
    ("SEMIBOLD_ITALIC", "Literata-SemiBoldItalic.ttf"),
]

# (px, line_height, baseline) -- identical to the Regular sets so Heavy and
# Normal share one vertical grid: 19 from generate_literata_sizes.py, 22 from
# generate_literata.py, 26 from generate_literata_sizes.py.
SIZES = [
    (19, 26, 20),
    (22, 30, 23),
    (26, 36, 27),
]

# Same coverage as the Regular reading sets so a weight change never swaps a
# rendered glyph for the '?' fallback.
RANGES = [
    (0x0020, 0x007E),  # Basic Latin
    (0x00A0, 0x024F),  # Latin-1, Latin Extended-A/B
    (0x0370, 0x03FF),  # Greek and Coptic
    (0x0400, 0x04FF),  # Cyrillic
    (0x1E00, 0x1EFF),  # Latin Extended Additional
    (0x2000, 0x206F),  # General Punctuation
    (0x20A0, 0x20CF),  # Currency Symbols
    (0x2100, 0x214F),  # Letterlike Symbols
    (0x2190, 0x21FF),  # Arrows
    (0x25A0, 0x25FF),  # Geometric Shapes
]


def codepoints():
    return codepoints_from_ranges(RANGES)


def main(argv=None):
    parse_generator_args(__doc__, OUT, reading_face=True, argv=argv)
    ensure_fonts(FONT_DIR, [name for _, name in FONTS])
    cps = codepoints()
    module = FontModule("tools/generate_literata_semibold.py", "SEMIBOLD")
    for px, line_height, baseline in SIZES:
        for style, filename in FONTS:
            path = FONT_DIR / filename
            font = load_font(path, px)
            glyphs = [rasterize_glyph(font, code, threshold_for(style)) for code in cps]
            module.add_face(
                f"LITERATA_{px}_{style}", line_height, baseline, cps, glyphs,
                kerning_entries(path, cps, px), missing_codepoints(path, cps),
            )
    module.write(OUT)


if __name__ == "__main__":
    main()
