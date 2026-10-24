#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "pillow==12.3.0",
# ]
# ///
"""Rasterize the non-default Literata reading sizes for the display crate.

Emits display/src/literata_sizes_generated.rs: the 19px and 26px reading
sets (4 styles each) behind the Type Size setting. The default 22px set
stays in literata_generated.rs via tools/generate_literata.py — that file
is deliberately not regenerated here because glyph rasterization differs
across Pillow/freetype builds and regenerating it would churn every
checked-in golden frame that uses the default size.

The same caveats apply to this file: it is reproducible only with the pinned
Pillow 12.3.0 / FreeType 2.14.3, an intentional change must refresh the
goldens and bump READER_LAYOUT_VERSION, and it refuses to overwrite the
checked-in output without --force.
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
OUT = ROOT / "display" / "src" / "literata_sizes_generated.rs"

FONTS = [
    ("REGULAR", "Literata-Regular.ttf"),
    ("ITALIC", "Literata-Italic.ttf"),
    ("BOLD", "Literata-Bold.ttf"),
    ("BOLD_ITALIC", "Literata-BoldItalic.ttf"),
]

# (px, line_height, baseline) — keep ratios close to the 22px set's 30/23.
SIZES = [
    (19, 26, 20),
    (26, 36, 27),
]

# Same coverage as the 22px reading set so a size change never swaps a
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
    module = FontModule("tools/generate_literata_sizes.py", "SIZES")
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
