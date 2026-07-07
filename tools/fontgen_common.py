from pathlib import Path
import os
import struct

from PIL import Image, ImageDraw


ADVANCE_SCALE = 16
DEFAULT_THRESHOLD = 128
MIN_KERNING_ADJUST_FP = 8
MAX_KERNING_ENTRIES = 1024
KERNING_CODEPOINTS = frozenset(
    list(range(0x20, 0x7F))
    + [0x2018, 0x2019, 0x201C, 0x201D, 0x2013, 0x2014, 0x2026]
)


def text_render_threshold() -> int:
    value = int(os.environ.get("TEXT_RENDER_THRESHOLD", str(DEFAULT_THRESHOLD)))
    if not 0 <= value <= 255:
        raise ValueError("TEXT_RENDER_THRESHOLD must be in 0..=255")
    return value


THRESHOLD = text_render_threshold()


def advance_fp(font, text: str) -> int:
    return max(int(round(font.getlength(text) * ADVANCE_SCALE)), ADVANCE_SCALE)


def rasterize_glyph(font, code: int):
    ch = chr(code)
    bbox = font.getbbox(ch, anchor="ls")
    advance = advance_fp(font, ch)
    if bbox is None:
        return (0, 0, 0, 0, advance, [])
    left, top, right, bottom = bbox
    width = max(0, right - left)
    height = max(0, bottom - top)
    if width == 0 or height == 0:
        return (0, 0, left, top, advance, [])
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
    return (width, height, left, top, advance, rows)


def codepoints_from_ranges(ranges):
    values = []
    for start, end in ranges:
        values.extend(range(start, end + 1))
    return sorted(set(values))


def u16(data, offset):
    return struct.unpack_from(">H", data, offset)[0]


def i16(data, offset):
    return struct.unpack_from(">h", data, offset)[0]


def u32(data, offset):
    return struct.unpack_from(">I", data, offset)[0]


def font_tables(path: Path):
    data = path.read_bytes()
    num_tables = u16(data, 4)
    tables = {}
    for i in range(num_tables):
        offset = 12 + i * 16
        tag = data[offset : offset + 4].decode("ascii", errors="replace")
        tables[tag] = (u32(data, offset + 8), u32(data, offset + 12))
    return data, tables


def parse_cmap(data, tables):
    if "cmap" not in tables:
        return {}
    base, _ = tables["cmap"]
    count = u16(data, base + 2)
    best = None
    best_rank = -1
    for i in range(count):
        rec = base + 4 + i * 8
        platform = u16(data, rec)
        encoding = u16(data, rec + 2)
        offset = u32(data, rec + 4)
        table = base + offset
        fmt = u16(data, table)
        rank = {
            (12, 3, 10): 5,
            (12, 0, 4): 4,
            (4, 3, 1): 3,
            (4, 0, 3): 2,
            (4, 0, 1): 1,
        }.get((fmt, platform, encoding), 0)
        if rank > best_rank:
            best = table
            best_rank = rank
    if best is None:
        return {}
    fmt = u16(data, best)
    mapping = {}
    if fmt == 4:
        seg_count = u16(data, best + 6) // 2
        end_codes = best + 14
        start_codes = end_codes + seg_count * 2 + 2
        id_deltas = start_codes + seg_count * 2
        id_range_offsets = id_deltas + seg_count * 2
        for i in range(seg_count):
            end = u16(data, end_codes + i * 2)
            start = u16(data, start_codes + i * 2)
            delta = i16(data, id_deltas + i * 2)
            range_offset = u16(data, id_range_offsets + i * 2)
            for cp in range(start, end + 1):
                if cp == 0xFFFF:
                    continue
                if range_offset == 0:
                    glyph = (cp + delta) & 0xFFFF
                else:
                    glyph_offset = id_range_offsets + i * 2 + range_offset + (cp - start) * 2
                    glyph = u16(data, glyph_offset)
                    if glyph:
                        glyph = (glyph + delta) & 0xFFFF
                if glyph:
                    mapping[cp] = glyph
    elif fmt == 12:
        group_count = u32(data, best + 12)
        pos = best + 16
        for _ in range(group_count):
            start = u32(data, pos)
            end = u32(data, pos + 4)
            glyph_start = u32(data, pos + 8)
            for cp in range(start, end + 1):
                if cp <= 0xFFFF:
                    mapping[cp] = glyph_start + cp - start
            pos += 12
    return mapping


def parse_coverage(data, base):
    fmt = u16(data, base)
    glyphs = []
    if fmt == 1:
        count = u16(data, base + 2)
        glyphs = [u16(data, base + 4 + i * 2) for i in range(count)]
    elif fmt == 2:
        count = u16(data, base + 2)
        pos = base + 4
        for _ in range(count):
            start = u16(data, pos)
            end = u16(data, pos + 2)
            glyphs.extend(range(start, end + 1))
            pos += 6
    return glyphs


def parse_class_def(data, base):
    fmt = u16(data, base)
    classes = {}
    if fmt == 1:
        start = u16(data, base + 2)
        count = u16(data, base + 4)
        for i in range(count):
            classes[start + i] = u16(data, base + 6 + i * 2)
    elif fmt == 2:
        count = u16(data, base + 2)
        pos = base + 4
        for _ in range(count):
            start = u16(data, pos)
            end = u16(data, pos + 2)
            cls = u16(data, pos + 4)
            for glyph in range(start, end + 1):
                classes[glyph] = cls
            pos += 6
    return classes


def value_record_size(fmt):
    return sum(2 for bit in range(8) if fmt & (1 << bit))


def read_x_advance(data, offset, fmt):
    value = 0
    pos = offset
    for bit in range(8):
        if not (fmt & (1 << bit)):
            continue
        if bit == 2:
            value = i16(data, pos)
        pos += 2
    return value


def parse_gpos_pair_adjustments(data, tables):
    if "GPOS" not in tables:
        return {}
    base, _ = tables["GPOS"]
    feature_list = base + u16(data, base + 6)
    lookup_list = base + u16(data, base + 8)

    feature_count = u16(data, feature_list)
    lookup_indices = set()
    for i in range(feature_count):
        rec = feature_list + 2 + i * 6
        tag = data[rec : rec + 4].decode("ascii", errors="replace")
        if tag != "kern":
            continue
        feature = feature_list + u16(data, rec + 4)
        count = u16(data, feature + 2)
        for j in range(count):
            lookup_indices.add(u16(data, feature + 4 + j * 2))

    adjustments = {}
    lookup_count = u16(data, lookup_list)
    for lookup_index in sorted(lookup_indices):
        if lookup_index >= lookup_count:
            continue
        lookup = lookup_list + u16(data, lookup_list + 2 + lookup_index * 2)
        lookup_type = u16(data, lookup)
        if lookup_type not in (2, 9):
            continue
        subtable_count = u16(data, lookup + 4)
        for i in range(subtable_count):
            sub = lookup + u16(data, lookup + 6 + i * 2)
            if lookup_type == 9:
                if u16(data, sub) != 1 or u16(data, sub + 2) != 2:
                    continue
                sub = sub + u32(data, sub + 4)
            parse_gpos_pair_subtable(data, sub, adjustments)
    return adjustments


def parse_gpos_pair_subtable(data, sub, adjustments):
    pos_format = u16(data, sub)
    coverage = parse_coverage(data, sub + u16(data, sub + 2))
    value_format1 = u16(data, sub + 4)
    value_format2 = u16(data, sub + 6)
    size1 = value_record_size(value_format1)
    size2 = value_record_size(value_format2)
    if pos_format == 1:
        pair_set_count = u16(data, sub + 8)
        for first_index in range(min(pair_set_count, len(coverage))):
            first = coverage[first_index]
            pair_set = sub + u16(data, sub + 10 + first_index * 2)
            pair_count = u16(data, pair_set)
            pos = pair_set + 2
            for _ in range(pair_count):
                second = u16(data, pos)
                value = read_x_advance(data, pos + 2, value_format1)
                if value:
                    adjustments[(first, second)] = value
                pos += 2 + size1 + size2
    elif pos_format == 2:
        class_def1 = parse_class_def(data, sub + u16(data, sub + 8))
        class_def2 = parse_class_def(data, sub + u16(data, sub + 10))
        class1_count = u16(data, sub + 12)
        class2_count = u16(data, sub + 14)
        glyphs_by_class1 = {}
        glyphs_by_class2 = {}
        for glyph in coverage:
            glyphs_by_class1.setdefault(class_def1.get(glyph, 0), []).append(glyph)
        for glyph, cls in class_def2.items():
            glyphs_by_class2.setdefault(cls, []).append(glyph)
        pos = sub + 16
        cell_size = size1 + size2
        for c1 in range(class1_count):
            for c2 in range(class2_count):
                value = read_x_advance(data, pos, value_format1)
                if value:
                    for first in glyphs_by_class1.get(c1, []):
                        for second in glyphs_by_class2.get(c2, []):
                            adjustments[(first, second)] = value
                pos += cell_size


def parse_kern_adjustments(data, tables):
    if "kern" not in tables:
        return {}
    base, _ = tables["kern"]
    adjustments = {}
    version = u16(data, base)
    if version != 0:
        return adjustments
    count = u16(data, base + 2)
    pos = base + 4
    for _ in range(count):
        length = u16(data, pos + 2)
        coverage = u16(data, pos + 4)
        fmt = coverage >> 8
        horizontal = coverage & 1
        if fmt == 0 and horizontal:
            pair_count = u16(data, pos + 6)
            pair_pos = pos + 14
            for _ in range(pair_count):
                left = u16(data, pair_pos)
                right = u16(data, pair_pos + 2)
                value = i16(data, pair_pos + 4)
                if value:
                    adjustments[(left, right)] = value
                pair_pos += 6
        pos += length
    return adjustments


def kerning_entries(font_path: Path, cps, px: int):
    data, tables = font_tables(font_path)
    units_per_em = u16(data, tables["head"][0] + 18)
    cmap = parse_cmap(data, tables)
    adjustments = parse_kern_adjustments(data, tables)
    adjustments.update(parse_gpos_pair_adjustments(data, tables))

    cps_by_glyph = {}
    for cp in cps:
        if cp not in KERNING_CODEPOINTS:
            continue
        glyph = cmap.get(cp)
        if glyph is not None:
            cps_by_glyph.setdefault(glyph, []).append(cp)

    entries = []
    for (left_glyph, right_glyph), value in adjustments.items():
        adjust_fp = int(round(value * px * ADVANCE_SCALE / units_per_em))
        if abs(adjust_fp) < MIN_KERNING_ADJUST_FP:
            continue
        for left in cps_by_glyph.get(left_glyph, []):
            for right in cps_by_glyph.get(right_glyph, []):
                entries.append((left, right, adjust_fp))
    entries = sorted(set(entries), key=lambda entry: (-abs(entry[2]), entry[0], entry[1]))
    return sorted(entries[:MAX_KERNING_ENTRIES])


def write_kerning(out, name, entries, count_name=None):
    count = count_name or f"{name}_KERNING_COUNT"
    out.append(f"pub const {count}: usize = {len(entries)};\n")
    out.append(f"pub static {name}_KERNING: [KerningEntry; {count}] = [\n")
    for left, right, adjust_fp in entries:
        out.append(
            "    KerningEntry { "
            f"left: 0x{left:04X}, right: 0x{right:04X}, adjust_fp: {adjust_fp} }},\n"
        )
    out.append("];\n\n")
