#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "pillow==12.3.0",
# ]
# ///
"""Rasterize the default 22px Literata reading set for the display crate.

WARNING: this overwrites display/src/literata_generated.rs, the checked-in
default reading face behind most golden frames and every default-size page
on the device. Do not re-run it casually.

- Regeneration is reproducible only with the pinned Pillow 12.3.0 (bundled
  FreeType 2.14.3), which the script's inline header installs when it runs
  through uv (`tools/generate_literata.py` or `uv run --script ...`). Other
  Pillow/FreeType builds drift hinting and rasterization and churn every
  glyph; the script refuses to run under them.
- Any intentional change (ranges, size, threshold, upstream font) must
  refresh the emulator goldens in fixtures/golden (REGEN_READING_GOLDEN=1
  for tools/emulator/tests/reading_golden.rs, on X4 and with
  --features device-x3, plus the fixtures/scenarios frames) and bump
  READER_LAYOUT_VERSION in ui/src/reading.rs so pagination cached under the
  old advances rebuilds.
- Without --force the script refuses to overwrite the existing file.

The TTFs are downloaded on demand into tools/fonts/ (gitignored) from a
pinned upstream commit and verified against SHA-256 hashes in
fontgen_common.py.
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
OUT = ROOT / "display" / "src" / "literata_generated.rs"

FONTS = [
    ("REGULAR", "Literata-Regular.ttf"),
    ("ITALIC", "Literata-Italic.ttf"),
    ("BOLD", "Literata-Bold.ttf"),
    ("BOLD_ITALIC", "Literata-BoldItalic.ttf"),
]

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
PX = 22
LINE_HEIGHT = 30
BASELINE = 23


def codepoints():
    return codepoints_from_ranges(RANGES)


def main(argv=None):
    parse_generator_args(__doc__, OUT, reading_face=True, argv=argv)
    ensure_fonts(FONT_DIR, [name for _, name in FONTS])
    cps = codepoints()
    module = FontModule("tools/generate_literata.py", "LITERATA")
    for style, filename in FONTS:
        path = FONT_DIR / filename
        font = load_font(path, PX)
        glyphs = [rasterize_glyph(font, code, threshold_for(style)) for code in cps]
        module.add_face(
            f"LITERATA_{style}", LINE_HEIGHT, BASELINE, cps, glyphs,
            kerning_entries(path, cps, PX), missing_codepoints(path, cps),
        )
    module.write(OUT)


if __name__ == "__main__":
    main()
