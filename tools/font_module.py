"""The generated-module side of the compact font tables.

display::font::BitmapFont describes the layout; this module writes it
(FontModule, glyph_record) and reads it back (read_module_glyphs). It
depends on nothing outside the standard library, so both the Pillow
generators (through fontgen_common) and tools/font_pack_to_rust.py use it.
"""
from pathlib import Path
import re

# Codepoints kerning pairs can name.
KERNING_CODEPOINTS = frozenset(
    list(range(0x20, 0x7F))
    + [0x2018, 0x2019, 0x201C, 0x201D, 0x2013, 0x2014, 0x2026]
)


def write_generated(path: Path, out) -> None:
    """Write a generated module ending in exactly one newline, as rustfmt leaves it."""
    path.write_text("".join(out).rstrip("\n") + "\n")
    print(f"wrote {path}")


# Kerning-index order, which must match display::font::KERNING_CODEPOINTS:
# printable ASCII, then the dashes, curly quotes and ellipsis.
KERNING_ORDER = sorted(KERNING_CODEPOINTS)
KERN_ROWS = len(KERNING_ORDER) + 1
KERNING_INDEX = {cp: i for i, cp in enumerate(KERNING_ORDER)}
MAX_GLYPH_WIDTH = 64


def glyph_record(width: int, height: int, rows) -> bytes:
    """Re-code a glyph's rows (MSB first, each padded to whole bytes, as
    rasterize_glyph returns them) as a bitmap record: per row a 0 bit when
    it repeats the row above (an empty row above the first), else a 1 bit
    and the row's `width` bits. The record is padded to a whole byte."""
    if width > MAX_GLYPH_WIDTH:
        raise ValueError(f"glyph is {width}px wide; the table format stops at {MAX_GLYPH_WIDTH}")
    row_bytes = (width + 7) // 8
    value = 0
    count = 0
    previous = 0
    for y in range(height):
        row = int.from_bytes(bytes(rows[y * row_bytes : (y + 1) * row_bytes]), "big")
        row >>= row_bytes * 8 - width
        if row == previous:
            value, count = value << 1, count + 1
        else:
            value, count = (value << (1 + width)) | (1 << width) | row, count + 1 + width
        previous = row
    pad = -count % 8
    return (value << pad).to_bytes((count + pad) // 8, "big")


def decode_glyph_record(record: bytes, width: int, height: int):
    """The inverse of glyph_record: rows padded to whole bytes."""
    bits = int.from_bytes(record, "big")
    left = len(record) * 8
    row_bytes = (width + 7) // 8
    rows = []
    previous = 0
    for _ in range(height):
        left -= 1
        if (bits >> left) & 1:
            left -= width
            previous = (bits >> left) & ((1 << width) - 1)
        rows.extend((previous << (row_bytes * 8 - width)).to_bytes(row_bytes, "big"))
    return rows


def codepoint_ranges(cps):
    """[(first, last, first slot)] for a sorted codepoint list."""
    ranges = []
    for slot, cp in enumerate(cps):
        if ranges and ranges[-1][1] + 1 == cp:
            ranges[-1][1] = cp
        else:
            ranges.append([cp, cp, slot])
    return [tuple(r) for r in ranges]


def _rust_list(items, per_line, fmt=str):
    lines = []
    for i in range(0, len(items), per_line):
        lines.append("    " + ", ".join(fmt(x) for x in items[i : i + per_line]) + ",\n")
    return "".join(lines)


class FontModule:
    """Collects faces and writes one generated module of BitmapFont statics.

    Faces with the same codepoints share one range table, and faces missing
    the same codepoints share one presence table. `prefix` names the shared
    tables; `font_path` is where the display font types live from the
    module's crate. `header` lines go before the `use` line (inner
    attributes), `preamble` lines after it.
    """

    def __init__(self, generator: str, prefix: str, font_path: str = "crate::font", header=(), preamble=()):
        self.generator = generator
        self.prefix = prefix
        self.font_path = font_path
        self.header = list(header)
        self.preamble = list(preamble)
        self.items = []
        self.shared = {}

    def _shared(self, kind, key, emit):
        if key not in self.shared:
            name = f"{self.prefix}_{kind}_{sum(1 for k in self.shared if k[0] == kind)}"
            self.shared[key] = name
            self.items.append(emit(name))
        return self.shared[key]

    def add_face(self, name, line_height, baseline, cps, glyphs, kerning, missing=()):
        """glyphs: rasterize_glyph tuples, one per codepoint in `cps`.
        kerning: (left, right, adjust_fp) entries. missing: codepoints the
        face draws its missing-glyph box for; their glyphs must be equal."""
        missing = set(missing)
        assert len(glyphs) == len(cps)
        ranges = codepoint_ranges(cps)
        ranges_name = self._shared(
            "RANGES",
            ("RANGES", tuple(ranges)),
            lambda n: (
                f"#[rustfmt::skip]\npub static {n}: [CodepointRange; {len(ranges)}] = [\n"
                + "".join(f"    CodepointRange {{ first: 0x{a:04X}, last: 0x{b:04X}, slot: {c} }},\n" for a, b, c in ranges)
                + "];\n\n"
            ),
        )
        present_name = None
        if missing:
            words = []
            rank = []
            count = 0
            for start in range(0, len(cps), 32):
                word = 0
                rank.append(count)
                for bit, cp in enumerate(cps[start : start + 32]):
                    if cp not in missing:
                        word |= 1 << bit
                        count += 1
                words.append(word)
            present_name = self._shared(
                "PRESENT",
                ("PRESENT", tuple(words)),
                lambda n: (
                    f"#[rustfmt::skip]\npub static {n}: [u32; {len(words)}] = [\n"
                    + _rust_list(words, 8, lambda w: f"0x{w:08X}")
                    + "];\n\n"
                    f"#[rustfmt::skip]\npub static {n}_RANK: [u16; {len(rank)}] = [\n"
                    + _rust_list(rank, 16)
                    + "];\n\n"
                ),
            )
        bitmap = bytearray()
        records = {}
        entries = []
        missing_glyph = None
        for cp, glyph in zip(cps, glyphs):
            width, height, x_offset, y_offset, advance, rows = glyph
            key = (width, height, tuple(rows))
            if key not in records:
                records[key] = len(bitmap)
                if width and height:
                    bitmap += glyph_record(width, height, rows)
            entry = (records[key], width, height, x_offset, y_offset, advance)
            if cp in missing:
                if missing_glyph is not None and missing_glyph[1:] != entry[1:]:
                    raise ValueError(f"{name}: U+{cp:04X} is missing but draws differently")
                missing_glyph = entry
            else:
                entries.append(entry)
        if missing_glyph is not None:
            entries.append(missing_glyph)
        if len(bitmap) > 0xFFFF:
            raise ValueError(f"{name}: {len(bitmap)} bitmap bytes; glyph offsets are 16 bits")
        runs = [[] for _ in KERNING_ORDER]
        for left, right, adjust_fp in kerning:
            if not -256 <= adjust_fp <= 255:
                raise ValueError(f"{name}: kerning {left:04X}/{right:04X} {adjust_fp} needs more than 9 bits")
            runs[KERNING_INDEX[left]].append((KERNING_INDEX[right], adjust_fp))
        kern_rows = [0]
        kern = []
        for run in runs:
            kern.extend((right << 9) | (adjust & 0x1FF) for right, adjust in sorted(run))
            kern_rows.append(len(kern))
        out = []
        out.append(f"#[rustfmt::skip]\npub static {name}_GLYPHS: [GlyphEntry; {len(entries)}] = [\n")
        out.extend(f"    GlyphEntry::new({o}, {w}, {h}, {x}, {y}, {a}),\n" for o, w, h, x, y, a in entries)
        out.append("];\n\n")
        out.append(f"#[rustfmt::skip]\npub static {name}_BITMAP: [u8; {len(bitmap)}] = [\n")
        out.append(_rust_list(list(bitmap), 16, lambda b: f"0x{b:02X}"))
        out.append("];\n\n")
        out.append(f"#[rustfmt::skip]\npub static {name}_KERN_ROWS: [u16; {KERN_ROWS}] = [\n")
        out.append(_rust_list(kern_rows, 16))
        out.append("];\n\n")
        out.append(f"#[rustfmt::skip]\npub static {name}_KERN: [u16; {len(kern)}] = [\n")
        out.append(_rust_list(kern, 12, lambda v: f"0x{v:04X}"))
        out.append("];\n\n")
        present = f"&{present_name}" if present_name else "&[]"
        present_rank = f"&{present_name}_RANK" if present_name else "&[]"
        out.append(
            f"#[rustfmt::skip]\npub static {name}: BitmapFont = BitmapFont {{\n"
            f"    line_height: {line_height},\n"
            f"    baseline: {baseline},\n"
            f"    ranges: &{ranges_name},\n"
            f"    present: {present},\n"
            f"    present_rank: {present_rank},\n"
            f"    glyphs: &{name}_GLYPHS,\n"
            f"    bitmap: &{name}_BITMAP,\n"
            f"    kern_rows: &{name}_KERN_ROWS,\n"
            f"    kern: &{name}_KERN,\n"
            "};\n\n"
        )
        self.items.append("".join(out))

    def write(self, path: Path) -> None:
        out = [f"// Generated by {self.generator}. Do not edit by hand.\n"]
        out.extend(self.header)
        out.append(f"use {self.font_path}::{{BitmapFont, CodepointRange, GlyphEntry}};\n\n")
        out.extend(self.preamble)
        out.extend(self.items)
        write_generated(path, out)


def read_module_glyphs(path: Path, face: str, codepoints):
    """Glyphs of `face` from a module FontModule wrote, as rasterize_glyph
    tuples keyed by codepoint. Lets a generator carry a checked-in glyph
    over instead of re-rasterizing it."""
    text = Path(path).read_text()

    def body(static):
        match = re.search(rf"pub static {static}: \[[^\]]*\] = \[\n(.*?)\n\];", text, re.S)
        if match is None:
            raise ValueError(f"{path}: no table {static}")
        return match.group(1)

    font = re.search(rf"pub static {face}: BitmapFont = BitmapFont \{{(.*?)\n\}};", text, re.S)
    if font is None:
        raise ValueError(f"{path}: no face {face}")
    fields = dict(re.findall(r"(\w+): &?(\w+|\[\]),", font.group(1)))
    ranges = [tuple(int(v, 0) for v in m) for m in re.findall(
        r"first: (0x[0-9A-F]+), last: (0x[0-9A-F]+), slot: (\d+)", body(fields["ranges"]))]
    present = [] if fields["present"] == "[]" else [int(v, 16) for v in re.findall(r"0x([0-9A-F]+)", body(fields["present"]))]
    entries = [tuple(int(v) for v in m) for m in re.findall(
        r"GlyphEntry::new\((-?\d+), (-?\d+), (-?\d+), (-?\d+), (-?\d+), (-?\d+)\)", body(fields["glyphs"]))]
    bitmap = bytes(int(v, 16) for v in re.findall(r"0x([0-9A-F]{2})", body(fields["bitmap"])))
    out = {}
    for cp in codepoints:
        slot = next(s + cp - a for a, b, s in ranges if a <= cp <= b)
        if present and not (present[slot // 32] >> (slot % 32)) & 1:
            index = len(entries) - 1
        else:
            index = sum(bin(w).count("1") for w in present[: slot // 32]) + bin(
                present[slot // 32] & ((1 << (slot % 32)) - 1)).count("1") if present else slot
        offset, width, height, x_offset, y_offset, advance = entries[index]
        rows = decode_glyph_record(bitmap[offset:], width, height) if width and height else []
        out[cp] = (width, height, x_offset, y_offset, advance, rows)
    return out
