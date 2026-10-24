#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Convert an X4FT font pack into a compile-time Rust custom font module."""

from __future__ import annotations

import argparse
import struct
from pathlib import Path

from font_module import FontModule


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUT = ROOT / "display" / "src" / "custom_generated.rs"

MAGIC = b"X4FT"
VERSION = 1
HEADER_LEN = 64
FACE_RECORD_LEN = 64
METRIC_LEN = 12
KERNING_LEN = 6

STYLE_NAMES = {
    0: "REGULAR",
    1: "ITALIC",
    2: "BOLD",
    3: "BOLD_ITALIC",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Convert CUSTOM.FNT/X4FT into display/src/custom_generated.rs."
    )
    parser.add_argument("pack", type=Path, help="Input X4FT font pack.")
    parser.add_argument(
        "--out",
        type=Path,
        default=DEFAULT_OUT,
        help=f"Rust module path (default: {DEFAULT_OUT}).",
    )
    return parser.parse_args()


def read_exact(data: bytes, offset: int, size: int) -> bytes:
    end = offset + size
    if offset < 0 or end > len(data):
        raise ValueError("truncated X4FT pack")
    return data[offset:end]


def rust_str(text: str) -> str:
    return '"' + text.encode("unicode_escape").decode("ascii").replace('"', '\\"') + '"'


def parse_pack(path: Path) -> dict[str, object]:
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
        _style_bits,
        face_table_offset,
        codepoints_offset,
        name_offset,
        _data_offset,
    ) = struct.unpack_from("<HHIQHHHHIIII", data, 4)
    if version != VERSION:
        raise ValueError(f"unsupported X4FT version {version}")
    if header_len != HEADER_LEN:
        raise ValueError(f"unsupported header length {header_len}")
    if total_len != len(data):
        raise ValueError(f"header length {total_len} does not match file length {len(data)}")

    name = read_exact(data, name_offset, name_len).decode("utf-8")
    cps = list(struct.unpack(f"<{codepoint_count}H", read_exact(data, codepoints_offset, codepoint_count * 2)))
    faces = []
    for index in range(face_count):
        record = read_exact(data, face_table_offset + index * FACE_RECORD_LEN, FACE_RECORD_LEN)
        (
            size_px,
            style,
            line_height,
            baseline,
            _flags,
            metrics_offset,
            metric_count,
            bitmap_offset,
            bitmap_len,
            kerning_offset,
            kerning_count,
        ) = struct.unpack_from("<BBBBIIIIIII", record, 0)
        if style not in STYLE_NAMES:
            raise ValueError(f"unsupported face style {style}")
        if metric_count != codepoint_count:
            raise ValueError(f"{size_px}px {style} metric count does not match codepoints")
        metrics = []
        for metric_index in range(metric_count):
            metric = read_exact(data, metrics_offset + metric_index * METRIC_LEN, METRIC_LEN)
            metrics.append(struct.unpack("<IHBBbbH", metric))
        bitmap = list(read_exact(data, bitmap_offset, bitmap_len))
        kerning = []
        for kerning_index in range(kerning_count):
            item = read_exact(data, kerning_offset + kerning_index * KERNING_LEN, KERNING_LEN)
            kerning.append(struct.unpack("<HHh", item))
        faces.append(
            {
                "size_px": size_px,
                "style": style,
                "line_height": line_height,
                "baseline": baseline,
                "metrics": metrics,
                "bitmap": bitmap,
                "kerning": kerning,
            }
        )
    return {
        "path": path,
        "name": name,
        "identity": identity,
        "codepoints": cps,
        "faces": sorted(faces, key=lambda face: (face["size_px"], face["style"])),
    }


def emit_module(pack: dict[str, object], out_path: Path) -> None:
    cps = pack["codepoints"]
    module = FontModule(
        "tools/font_pack_to_rust.py",
        "CUSTOM",
        preamble=[
            f"pub const CUSTOM_FONT_NAME: &str = {rust_str(pack['name'])};\n",
            f"pub const CUSTOM_FONT_IDENTITY: u64 = 0x{pack['identity']:016X};\n\n",
        ],
    )
    for face in pack["faces"]:
        bitmap = face["bitmap"]
        glyphs = [
            (width, height, x_offset, y_offset, advance_fp, bitmap[offset : offset + length])
            for offset, length, width, height, x_offset, y_offset, advance_fp in face["metrics"]
        ]
        # A pack doesn't say which slots are the face's missing-glyph box, so
        # every slot keeps its own entry; identical bitmaps still share one.
        module.add_face(
            f"CUSTOM_{face['size_px']}_{STYLE_NAMES[face['style']]}",
            face["line_height"],
            face["baseline"],
            cps,
            glyphs,
            face["kerning"],
        )
    module.write(out_path)


def main() -> None:
    args = parse_args()
    pack = parse_pack(args.pack)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    emit_module(pack, args.out)


if __name__ == "__main__":
    main()
