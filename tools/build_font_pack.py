#!/usr/bin/env python3
"""Build or inspect a MarigoldOS custom typeface pack.

The firmware cannot consume TTF/OTF files directly. This tool converts a small
family of desktop font files into the fixed bitmap/metric records the reader
runtime is designed to draw.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import struct
import sys
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUT = ROOT / "target" / "fonts" / "CUSTOM.FNT"

MAGIC = b"X4FT"
VERSION = 1
HEADER_LEN = 64
FACE_RECORD_LEN = 64
METRIC_LEN = 12
KERNING_LEN = 6
MAX_NAME_BYTES = 63

STYLE_REGULAR = 0
STYLE_ITALIC = 1
STYLE_BOLD = 2
STYLE_BOLD_ITALIC = 3

STYLE_LABELS = {
    STYLE_REGULAR: "Regular",
    STYLE_ITALIC: "Italic",
    STYLE_BOLD: "Bold",
    STYLE_BOLD_ITALIC: "BoldItalic",
}

# Match the built-in reading sets so custom typefaces use the same line grid.
SIZES = [
    (19, 26, 20),
    (22, 30, 23),
    (26, 36, 27),
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


@dataclass(frozen=True)
class SourceFace:
    style: int
    path: Path
    synthetic: bool


@dataclass(frozen=True)
class BuiltFace:
    size_px: int
    line_height: int
    baseline: int
    style: int
    source: SourceFace
    metrics: bytes
    bitmap: bytes
    kerning: bytes


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build or inspect a MarigoldOS X4FT custom typeface pack."
    )
    sub = parser.add_subparsers(dest="command")

    build = sub.add_parser("build", help="Build CUSTOM.FNT from TTF/OTF inputs.")
    build.add_argument("--regular", type=Path, required=True, help="Regular TTF/OTF.")
    build.add_argument("--italic", type=Path, help="Italic TTF/OTF.")
    build.add_argument("--bold", type=Path, help="Bold TTF/OTF.")
    build.add_argument("--bold-italic", type=Path, help="BoldItalic TTF/OTF.")
    build.add_argument(
        "--name",
        required=True,
        help=f"Display name stored in the pack, max {MAX_NAME_BYTES} UTF-8 bytes.",
    )
    build.add_argument(
        "--out",
        type=Path,
        default=DEFAULT_OUT,
        help=f"Output path (default: {DEFAULT_OUT}).",
    )
    build.add_argument(
        "--json",
        action="store_true",
        help="Print machine-readable summary after building.",
    )

    inspect = sub.add_parser("inspect", help="Inspect an existing X4FT pack.")
    inspect.add_argument("pack", type=Path)
    inspect.add_argument("--json", action="store_true")

    # Keep the common happy path short: `tools/build_font_pack.py --regular ...`.
    parser.add_argument("--regular", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--italic", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--bold", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--bold-italic", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--name", help=argparse.SUPPRESS)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT, help=argparse.SUPPRESS)
    parser.add_argument("--json", action="store_true", help=argparse.SUPPRESS)

    args = parser.parse_args()
    if args.command is None:
        if args.regular and args.name:
            args.command = "build"
        else:
            parser.print_help()
            raise SystemExit(2)
    return args


def checked_name(name: str) -> bytes:
    raw = name.strip().encode("utf-8")
    if not raw:
        raise ValueError("--name must not be empty")
    if len(raw) > MAX_NAME_BYTES:
        raise ValueError(f"--name is {len(raw)} bytes; max is {MAX_NAME_BYTES}")
    return raw


def checked_font(path: Path) -> Path:
    if not path.exists():
        raise FileNotFoundError(path)
    if not path.is_file():
        raise ValueError(f"not a file: {path}")
    return path


def sources(args: argparse.Namespace) -> list[SourceFace]:
    regular = checked_font(args.regular)
    bold = checked_font(args.bold) if args.bold else regular
    italic = checked_font(args.italic) if args.italic else regular
    bold_italic = checked_font(args.bold_italic) if args.bold_italic else bold
    return [
        SourceFace(STYLE_REGULAR, regular, False),
        SourceFace(STYLE_ITALIC, italic, args.italic is None),
        SourceFace(STYLE_BOLD, bold, args.bold is None),
        SourceFace(STYLE_BOLD_ITALIC, bold_italic, args.bold_italic is None),
    ]


def codepoints_from_ranges(ranges: list[tuple[int, int]]) -> list[int]:
    values = []
    for start, end in ranges:
        values.extend(range(start, end + 1))
    return sorted(set(values))


def build_face(source: SourceFace, size_px: int, line_height: int, baseline: int, cps: list[int]) -> BuiltFace:
    try:
        from PIL import ImageFont
        from fontgen_common import kerning_entries, rasterize_glyph
    except ModuleNotFoundError as exc:
        raise RuntimeError("Pillow is required. Use .venv-font/bin/python or install pillow.") from exc

    font = ImageFont.truetype(str(source.path), size_px)
    metric_records = bytearray()
    bitmap = bytearray()
    for code in cps:
        width, height, x_offset, y_offset, advance, rows = rasterize_glyph(font, code)
        if width > 255 or height > 255:
            raise ValueError(
                f"{source.path} glyph U+{code:04X} at {size_px}px is too large: {width}x{height}"
            )
        if len(rows) > 0xFFFF:
            raise ValueError(
                f"{source.path} glyph U+{code:04X} at {size_px}px bitmap is too large"
            )
        metric_records.extend(
            struct.pack(
                "<IHBBbbH",
                len(bitmap),
                len(rows),
                width,
                height,
                x_offset,
                y_offset,
                advance,
            )
        )
        bitmap.extend(rows)

    kern = bytearray()
    for left, right, adjust_fp in kerning_entries(source.path, cps, size_px):
        kern.extend(struct.pack("<HHh", left, right, adjust_fp))

    return BuiltFace(
        size_px=size_px,
        line_height=line_height,
        baseline=baseline,
        style=source.style,
        source=source,
        metrics=bytes(metric_records),
        bitmap=bytes(bitmap),
        kerning=bytes(kern),
    )


def set_u64(buf: bytearray, offset: int, value: int) -> None:
    buf[offset : offset + 8] = struct.pack("<Q", value)


def pack_hash(data: bytes) -> int:
    digest = hashlib.blake2s(data, digest_size=8).digest()
    return struct.unpack("<Q", digest)[0]


def build_pack(args: argparse.Namespace) -> dict[str, object]:
    name = checked_name(args.name)
    cps = codepoints_from_ranges(RANGES)
    source_faces = sources(args)
    built_faces = [
        build_face(source, size_px, line_height, baseline, cps)
        for size_px, line_height, baseline in SIZES
        for source in source_faces
    ]

    face_table_offset = HEADER_LEN
    codepoints_offset = face_table_offset + len(built_faces) * FACE_RECORD_LEN
    name_offset = codepoints_offset + len(cps) * 2
    data_offset = name_offset + len(name)

    face_records = bytearray()
    data = bytearray()
    for face in built_faces:
        metrics_offset = data_offset + len(data)
        data.extend(face.metrics)
        bitmap_offset = data_offset + len(data)
        data.extend(face.bitmap)
        kerning_offset = data_offset + len(data)
        data.extend(face.kerning)
        flags = 1 if face.source.synthetic else 0
        record = struct.pack(
            "<BBBBI" + "IIIIII" + "32x",
            face.size_px,
            face.style,
            face.line_height,
            face.baseline,
            flags,
            metrics_offset,
            len(face.metrics) // METRIC_LEN,
            bitmap_offset,
            len(face.bitmap),
            kerning_offset,
            len(face.kerning) // KERNING_LEN,
        )
        if len(record) != FACE_RECORD_LEN:
            raise AssertionError(f"face record is {len(record)} bytes, expected {FACE_RECORD_LEN}")
        face_records.extend(record)

    total_len = data_offset + len(data)
    header = bytearray(HEADER_LEN)
    header[0:4] = MAGIC
    header[4:6] = struct.pack("<H", VERSION)
    header[6:8] = struct.pack("<H", HEADER_LEN)
    header[8:12] = struct.pack("<I", total_len)
    # offset 12 stores the hash after the full payload is assembled.
    header[20:22] = struct.pack("<H", len(built_faces))
    header[22:24] = struct.pack("<H", len(cps))
    header[24:26] = struct.pack("<H", len(name))
    header[26:28] = struct.pack("<H", sum(1 << source.style for source in source_faces if not source.synthetic))
    header[28:32] = struct.pack("<I", face_table_offset)
    header[32:36] = struct.pack("<I", codepoints_offset)
    header[36:40] = struct.pack("<I", name_offset)
    header[40:44] = struct.pack("<I", data_offset)

    out = header + face_records + bytearray(struct.pack(f"<{len(cps)}H", *cps)) + name + data
    identity = pack_hash(bytes(out[:12] + b"\0" * 8 + out[20:]))
    set_u64(out, 12, identity)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_bytes(out)
    return summarize_pack(args.out, out)


def read_exact(data: bytes, offset: int, size: int) -> bytes:
    end = offset + size
    if offset < 0 or end > len(data):
        raise ValueError("truncated X4FT pack")
    return data[offset:end]


def summarize_pack(path: Path, data: bytes | None = None) -> dict[str, object]:
    if data is None:
        data = path.read_bytes()
    if len(data) < HEADER_LEN:
        raise ValueError("file is too short for an X4FT header")
    if data[:4] != MAGIC:
        raise ValueError("not an X4FT font pack")

    (
        version,
        header_len,
        total_len,
        identity,
        face_count,
        codepoint_count,
        name_len,
        style_bits,
        face_table_offset,
        codepoints_offset,
        name_offset,
        data_offset,
    ) = struct.unpack_from("<HHIQHHHHIIII", data, 4)
    if version != VERSION:
        raise ValueError(f"unsupported X4FT version {version}")
    if header_len != HEADER_LEN:
        raise ValueError(f"unsupported header length {header_len}")
    if total_len != len(data):
        raise ValueError(f"header length {total_len} does not match file length {len(data)}")

    expected = pack_hash(data[:12] + b"\0" * 8 + data[20:])
    if expected != identity:
        raise ValueError("pack identity hash mismatch")

    name = read_exact(data, name_offset, name_len).decode("utf-8")
    cps_bytes = read_exact(data, codepoints_offset, codepoint_count * 2)
    cps = struct.unpack(f"<{codepoint_count}H", cps_bytes)
    faces = []
    for index in range(face_count):
        offset = face_table_offset + index * FACE_RECORD_LEN
        record = read_exact(data, offset, FACE_RECORD_LEN)
        (
            size_px,
            style,
            line_height,
            baseline,
            flags,
            metrics_offset,
            metrics_count,
            bitmap_offset,
            bitmap_len,
            kerning_offset,
            kerning_count,
        ) = struct.unpack_from("<BBBBIIIIIII", record, 0)
        if metrics_count != codepoint_count:
            raise ValueError(f"face {index} metric count does not match codepoint count")
        read_exact(data, metrics_offset, metrics_count * METRIC_LEN)
        read_exact(data, bitmap_offset, bitmap_len)
        read_exact(data, kerning_offset, kerning_count * KERNING_LEN)
        faces.append(
            {
                "size_px": size_px,
                "style": STYLE_LABELS.get(style, f"style-{style}"),
                "line_height": line_height,
                "baseline": baseline,
                "synthetic": bool(flags & 1),
                "bitmap_bytes": bitmap_len,
                "kerning_pairs": kerning_count,
            }
        )

    return {
        "path": str(path),
        "name": name,
        "version": version,
        "identity": f"{identity:016x}",
        "bytes": len(data),
        "codepoints": codepoint_count,
        "coverage": f"U+{min(cps):04X}..U+{max(cps):04X}" if cps else "",
        "styles": [
            label for bit, label in STYLE_LABELS.items() if style_bits & (1 << bit)
        ],
        "faces": faces,
    }


def print_summary(summary: dict[str, object], as_json: bool, verb: str) -> None:
    if as_json:
        print(json.dumps(summary, indent=2, sort_keys=True))
        return
    print(f"{verb} {summary['path']}")
    print(f"name: {summary['name']}")
    print(f"identity: {summary['identity']}")
    print(f"size: {summary['bytes']} bytes")
    print(f"codepoints: {summary['codepoints']} ({summary['coverage']})")
    print(f"source styles: {', '.join(summary['styles'])}")
    print("faces:")
    for face in summary["faces"]:
        suffix = " synthetic" if face["synthetic"] else ""
        print(
            "  "
            f"{face['size_px']}px {face['style']}: "
            f"{face['bitmap_bytes']} bitmap bytes, "
            f"{face['kerning_pairs']} kerning pairs{suffix}"
        )


def main() -> None:
    try:
        args = parse_args()
        if args.command == "build":
            summary = build_pack(args)
            verb = "wrote"
        elif args.command == "inspect":
            summary = summarize_pack(args.pack)
            verb = "pack"
        else:
            raise ValueError(f"unknown command: {args.command}")
        print_summary(summary, args.json, verb)
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
