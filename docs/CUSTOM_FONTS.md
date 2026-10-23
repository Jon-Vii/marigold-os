# Custom fonts

MarigoldOS supports one optional custom reader typeface. You can't copy a raw
TTF or OTF file to the device. The font has to be converted into MarigoldOS'
bounded `CUSTOM.FNT` format first.

Once a valid custom font pack is installed, Settings lists it as another
Typeface choice, under the display name stored in the pack. Without a valid
pack, Settings looks the same as before.

## Install a custom font

1. Pick the font files you want to use. You need a regular TTF/OTF face;
   italic, bold, and bold italic faces are optional.
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
   declares its own pinned Pillow, which `uv run --script` fetches the first
   time you run it, so you don't need a virtualenv or `pip install`. On macOS
   and Linux you can also run `tools/build_font_pack.py` directly.
3. Copy the generated file to the SD card:

   ```text
   /XTEINK/FONTS/CUSTOM.FNT
   ```

   Create the `XTEINK/FONTS` directories if they do not exist.
4. Eject the SD card, insert it into the reader, and reboot.
5. Open Settings and choose the custom typeface by name.

If you replace `CUSTOM.FNT` with a different font, books may rebuild their
page cache the next time you open them. Font metrics affect line breaks and
page counts, so this is expected.

## Current limits

- There is one custom slot: `/XTEINK/FONTS/CUSTOM.FNT`.
- Raw `.ttf` and `.otf` files on the SD card are ignored.
- Missing styles fall back to the closest supplied face while building the pack.
- Only install fonts you have the right to use.
- The website doesn't build or upload custom fonts yet. For now, use the local
  converter.

## Developer compile-in path

Firmware developers can also compile the same custom pack into a firmware
build. That's useful for experiments, demos, or hardware batches that should
ship with a specific custom typeface without depending on a file on the SD
card.

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

In normal builds `display/src/custom_generated.rs` is a tracked placeholder,
there so workspace tooling can resolve the optional module. Replace it with the
generated file before you enable `builtin-custom-font`. With the feature on, the
compiled-in custom font takes precedence over `CUSTOM.FNT` on the SD card.
