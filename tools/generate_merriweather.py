#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "pillow==12.3.0",
# ]
# ///
"""Rasterize the Merriweather reading faces for the display crate.

Emits display/src/merriweather_generated.rs: all three reading sizes (19,
22, 26px) in four styles each, behind the Font setting. Unlike Literata,
every Merriweather size lives in this one file.

Regenerating it does churn goldens: fixtures/golden/reading-page-merriweather-0
(X4 and X3 frames, checked by reading_page_bodies_match_goldens_merriweather
in tools/emulator/tests/reading_golden.rs) renders these glyphs. Like the
Literata tables, it is reproducible only with the pinned Pillow 12.3.0 /
FreeType 2.14.3; an intentional change must refresh those goldens and bump
READER_LAYOUT_VERSION, and the script refuses to overwrite the checked-in
output without --force.

Merriweather (Sorkin Type, SIL Open Font License) is shipped as two variable
fonts; the four styles are instanced from them at the text optical size
(opsz 18), normal width, weights 400/700. Line heights and baselines match
the Literata set at each size so pagination advances stay family-independent.
The TTFs are downloaded on demand into tools/fonts/ (gitignored) from a
pinned google/fonts commit and verified against SHA-256 hashes in
fontgen_common.py; only the generated bitmaps are committed, which OFL
permits.
"""
from pathlib import Path
import sys

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
    read_module_glyphs,
)

ROOT = Path(__file__).resolve().parents[1]
FONT_DIR = ROOT / "tools" / "fonts" / "merriweather"
OUT = ROOT / "display" / "src" / "merriweather_generated.rs"

# The two variable sources (local names; upstream URLs live in fontgen_common).
SOURCES = ["Merriweather.ttf", "Merriweather-Italic.ttf"]

# (style const suffix, source file, variation axes [wght, wdth, opsz]).
# opsz 18 is the text/reading end of the optical-size axis; wdth 100 normal.
FONTS = [
    ("REGULAR", "Merriweather.ttf", [400, 100, 18]),
    ("ITALIC", "Merriweather-Italic.ttf", [400, 100, 18]),
    ("BOLD", "Merriweather.ttf", [700, 100, 18]),
    ("BOLD_ITALIC", "Merriweather-Italic.ttf", [700, 100, 18]),
]

# (px, line_height, baseline) — identical to the Literata sets so a family
# change never moves the line grid, only the glyphs on it.
SIZES = [
    (19, 26, 20),
    (22, 30, 23),
    (26, 36, 27),
]

# Same coverage as the Literata reading sets so a family change never swaps
# a rendered glyph for the '?' fallback.
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


# Glyphs carried over from the checked-in module instead of re-rasterized.
# Merriweather has no bytecode hinting, so FreeType autohints it, and
# FreeType 2.14 gives the autohinter HarfBuzz whenever one is loaded in the
# process. Pillow's Linux wheels load their own for raqm. HarfBuzz extends
# the autohinter's glyph coverage through the font's GSUB features, which
# hints U+2116 differently: on Linux it comes out a row shorter at 19px,
# and a pixel narrower or wider at other sizes, than in the checked-in
# tables, which a FreeType without HarfBuzz made. Every other glyph comes
# out the same either way, and Literata carries its own bytecode hinting.
CARRIED_CODEPOINTS = [0x2116]


def codepoints():
    return codepoints_from_ranges(RANGES)


def main(argv=None):
    parse_generator_args(__doc__, OUT, reading_face=True, argv=argv)
    ensure_fonts(FONT_DIR, SOURCES)
    cps = codepoints()
    # Read the carried glyphs before anything overwrites the module.
    carried = {}
    if OUT.exists():
        for px, _, _ in SIZES:
            for style, _, _ in FONTS:
                name = f"MERRIWEATHER_{px}_{style}"
                carried[name] = read_module_glyphs(OUT, name, CARRIED_CODEPOINTS)
    else:
        print(f"warning: no {OUT.name} to carry U+2116 from; rasterizing it", file=sys.stderr)
    module = FontModule("tools/generate_merriweather.py", "MERRIWEATHER")
    for px, line_height, baseline in SIZES:
        for style, source, axes in FONTS:
            path = FONT_DIR / source
            font = load_font(path, px, axes)
            name = f"MERRIWEATHER_{px}_{style}"
            glyphs = []
            for code in cps:
                if code in carried.get(name, {}):
                    glyphs.append(carried[name][code])
                else:
                    glyphs.append(rasterize_glyph(font, code, threshold_for(style, "merriweather")))
            module.add_face(
                name, line_height, baseline, cps, glyphs,
                kerning_entries(path, cps, px), missing_codepoints(path, cps),
            )
    module.write(OUT)


if __name__ == "__main__":
    main()
