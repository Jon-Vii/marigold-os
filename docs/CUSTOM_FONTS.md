# Custom fonts

MarigoldOS supports one optional custom reader typeface. Users do not copy a raw
TTF or OTF file to the device; the font first has to be converted into
MarigoldOS' bounded `CUSTOM.FNT` format.

When a valid custom font pack is installed, Settings shows it as another
Typeface choice using the display name stored in the pack. If no valid pack is
installed, Settings stays unchanged.

## Install a custom font

1. Pick the font files you want to use. A regular TTF/OTF face is required.
   Italic, bold, and bold italic faces are optional.
2. Build `CUSTOM.FNT` from a checkout of this repo:

   ```sh
   uv run --script tools/build_font_pack.py build \
     --regular path/to/Regular.ttf \
     --italic path/to/Italic.ttf \
     --bold path/to/Bold.ttf \
     --bold-italic path/to/BoldItalic.ttf \
     --name "My Typeface" \
     --out CUSTOM.FNT
   ```

   For a regular-only font:

   ```sh
   uv run --script tools/build_font_pack.py build \
     --regular path/to/Regular.ttf \
     --name "My Typeface" \
     --out CUSTOM.FNT
   ```

   The converter runs through [uv](https://docs.astral.sh/uv/). The script
   declares its own pinned Pillow, so `uv run --script` fetches it on first use;
   no virtualenv or `pip install` is needed. On macOS and Linux you can also
   run `tools/build_font_pack.py` directly.
3. Copy the generated file to the SD card:

   ```text
   /XTEINK/FONTS/CUSTOM.FNT
   ```

   Create the `XTEINK/FONTS` directories if they do not exist.
4. Eject the SD card, insert it into the reader, and reboot.
5. Open Settings and choose the custom typeface by name.

Replacing `CUSTOM.FNT` with a different font can make books rebuild their page
cache the next time they are opened. That is expected: font metrics affect line
breaks and page counts.

## Current limits

- There is one custom slot: `/XTEINK/FONTS/CUSTOM.FNT`.
- Raw `.ttf` and `.otf` files on the SD card are ignored.
- Missing styles fall back to the closest supplied face while building the pack.
- Keep font licensing in mind. Only install fonts you have the right to use.
- The website does not build or upload custom fonts yet; the local converter is
  the current user path.

## Developer compile-in path

Firmware developers can also compile the same custom pack into a firmware build.
This is useful for experiments, demos, or hardware batches that should ship with
a specific custom typeface without relying on an SD card file.

```sh
uv run --script tools/build_font_pack.py build \
  --regular path/to/Regular.ttf \
  --italic path/to/Italic.ttf \
  --bold path/to/Bold.ttf \
  --bold-italic path/to/BoldItalic.ttf \
  --name "My Typeface" \
  --out target/fonts/CUSTOM.FNT

uv run --script tools/font_pack_to_rust.py \
  target/fonts/CUSTOM.FNT \
  --out display/src/custom_generated.rs

tools/cargo.sh build --release --features builtin-custom-font
```

`display/src/custom_generated.rs` is a tracked placeholder in normal builds so
workspace tooling can resolve the optional module. Replace it with the generated
file before enabling `builtin-custom-font`. When enabled, the compiled-in custom
font takes precedence over the SD card `CUSTOM.FNT`.
