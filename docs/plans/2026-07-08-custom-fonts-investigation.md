# Custom fonts investigation

Question: can users bring their own custom fonts/typefaces?

Terminology: a typeface is the design family (for example Literata), while a
font is one concrete face/file within that family (for example Literata Regular
22 px). In the current codebase the user-facing setting is rightly called
`Typeface`, because it switches the reading body family. The implementation
still needs concrete Regular/Italic/Bold/BoldItalic font assets for every
reading size.

## Current state

Reading text is rendered from generated `BitmapFont` tables in the `display`
crate. Host-side Python tools use Pillow/FreeType to rasterize TTFs into 1 bpp
glyph bitmaps, metrics, and kerning entries. Firmware then blits those static
tables directly into the 1 bpp framebuffer; it does not parse or rasterize TTFs
on device.

The reader page plan already treats typeface as layout-affecting:

- `display::font::FontFamily` is a closed enum of `Literata` and
  `Merriweather`.
- `TypeSettings` carries size, spacing, weight, and family through app state,
  storage commands, cache building, and rendering.
- `ui::reading::reader_layout_config` stores the family bit in section cache
  headers. A family change forces rebuilt pagination because glyph advances and
  kerning change wrap points.
- Settings already exposes this as a `Typeface` row.

Generated font data is meaningful flash/compile-time weight. The committed
generated source is about 20 MB; estimated target-side read-only table data is
about 3.0 MB across the shipped faces:

```text
literata_extra_generated.rs     ~74 KB
literata_generated.rs          ~367 KB
literata_semibold_generated.rs ~556 KB
literata_sizes_generated.rs    ~744 KB
merriweather_generated.rs    ~1,297 KB
```

This estimate covers bitmaps, metrics, codepoint tables, and kerning entries,
assuming 32-bit target layout for `GlyphMetric` and 6-byte `KerningEntry`.

## Constraints

- The ordinary reading path is `#![no_std]`, no heap, one 48 KB 1 bpp
  framebuffer, and deterministic cached pagination.
- The firmware has enough structure to select among pre-known families, but not
  arbitrary runtime font files.
- Any change to glyph advances, kerning, codepoint coverage, line metrics, or
  fallback behavior can invalidate existing section caches.
- FAT firmware paths prefer bounded, short-name-friendly artifacts under
  `/XTEINK`.
- The Wireless session can upload files, but entering it loans the EPUB scratch
  to the radio and ends in a reset, so font installation should be modeled as
  an install/update operation rather than an in-session live preview.

## Implementation paths

### 1. More built-in typefaces

Add another generated family to the firmware, extend `FontFamily`, update the
Settings cycle, and bump the reader layout version if the enum encoding reuses
existing bits or cache semantics change.

Pros:

- Smallest code change.
- Same rendering path, same no-heap guarantees, same cache model.
- Good for a curated third face.

Cons:

- Every shipped face increases firmware image size and compile time.
- Not user-provided in the meaningful sense.
- The current `reader_layout_config` only reserves one family bit; more than two
  families needs a wider family field and a layout version bump.

This is useful as a proving step, but it does not solve bring-your-own fonts.

### 2. Runtime TTF/OTF parsing on firmware

Store `.ttf` or `.otf` files on the SD card and rasterize/cache glyphs on the
device.

Pros:

- Closest to the user mental model: drop a font file on the card.
- Could support wide Unicode coverage over time.

Cons:

- Poor fit for current architecture. TrueType parsing, hinting, rasterization,
  shaping, and glyph cache management want memory and complexity the reader
  path deliberately avoids.
- Pagination would need to block on glyph discovery or maintain a persistent
  glyph cache before cache building.
- Font licensing and malformed-font handling become firmware concerns.

This is not recommended for MarigoldOS in the current hardware envelope.

### 3. Host-side conversion to a font-pack artifact

Accept user font files through a host tool, the MarigoldOS site, or Wireless
shelf, then convert them into the same kind of 1 bpp bitmap data the firmware
already knows how to draw. Store the converted artifact on SD, and teach
firmware to load one bounded custom typeface slot from `/XTEINK/FONTS`.

Pros:

- Preserves the runtime model: firmware blits bitmaps and measures fixed
  metrics; no TTF rasterizer in the reading path.
- Lets conversion validate coverage, line metrics, bitmap sizes, and kerning
  limits before the device ever selects the face.
- Scales to user-provided typefaces without baking every face into firmware.

Cons:

- Requires a new portable font-pack binary format and loader.
- `BitmapFont` currently points at `'static` slices; dynamic SD-backed tables
  need a borrowed/loaded-font variant or a second draw/measure path.
- RAM is too tight to load multi-megabyte full-family packs wholesale. The pack
  likely needs per-face records and a small glyph window/cache, or a deliberately
  constrained codepoint set.
- Page turns must not do SD glyph lookups. Cache build or book-open should make
  the active face usable before reading renders.

This is the recommended direction.

## Recommended staged design

Start with a single custom typeface slot, not an arbitrary list.

1. Define a host-generated `X4FT` font-pack artifact under
   `/XTEINK/FONTS/CUSTOM.FNT`.
2. Keep the first version deliberately narrow: three reading sizes,
   Regular/Italic/Bold/BoldItalic, the same codepoint coverage as the built-in
   reading faces, fixed line heights/baselines matching the existing sizes, and
   bounded kerning.
3. Add a host tool such as `tools/build_font_pack.py` that reuses
   `tools/fontgen_common.py` but writes binary records instead of Rust source.
4. Extend firmware font selection with `FontFamily::Custom` only when a valid
   font-pack manifest is present. Settings can show `custom` after
   `Merriweather`; if the file disappears, fall back to Literata and mark
   custom caches stale.
5. Change the reader page plan font access from returning only
   `&'static BitmapFont` to an active-font provider that can serve either
   built-in static tables or a loaded custom face.
6. Store a stable custom font identity in layout config or cache headers. A
   hash/version from the font-pack manifest is safer than a plain enum value:
   replacing `CUSTOM.FNT` must invalidate stale section caches even though the
   selected slot is still `Custom`.

The key design pressure is whether a complete custom pack can be resident while
reading. The current built-in family footprint suggests a full Merriweather-like
custom pack would be around 1.3 MB of table data, so the firmware should not
plan to load the whole thing into RAM. Two viable sub-options:

- Keep custom packs SD-backed but build section caches by streaming glyph
  records through a small lookup cache, then render pages from already-resolved
  line/page records. This requires care to avoid SD access during render.
- Make custom packs much smaller in v1: one size only, no Heavy mode, limited
  Latin coverage, or no custom italic/bold fallback files. This is simpler but
  feels less like a real reader feature.

## Dev tool and X4FT v1 format

The first dev-side builder exists as `tools/build_font_pack.py`. It builds a
single custom slot artifact:

```sh
.venv-font/bin/python tools/build_font_pack.py build \
  --regular path/to/Regular.ttf \
  --italic path/to/Italic.ttf \
  --bold path/to/Bold.ttf \
  --bold-italic path/to/BoldItalic.ttf \
  --name "Atkinson Hyperlegible" \
  --out target/fonts/CUSTOM.FNT
```

`--regular` is required. Missing italic/bold faces fall back to the closest
available source and are marked synthetic in the face records. The shorthand
form also works:

```sh
.venv-font/bin/python tools/build_font_pack.py \
  --regular path/to/Regular.ttf \
  --name "Atkinson Hyperlegible" \
  --out target/fonts/CUSTOM.FNT
```

Inspect a pack with:

```sh
.venv-font/bin/python tools/build_font_pack.py inspect target/fonts/CUSTOM.FNT
```

The v1 artifact is intentionally simple:

```text
Header, 64 bytes, little endian
0x00  [4]  magic "X4FT"
0x04  u16  version = 1
0x06  u16  header_len = 64
0x08  u32  total file length
0x0c  u64  BLAKE2s-64 identity hash, computed with this field zeroed
0x14  u16  face count, currently 12
0x16  u16  codepoint count
0x18  u16  display-name byte length
0x1a  u16  source style bits, Regular=1, Italic=2, Bold=4, BoldItalic=8
0x1c  u32  face table offset
0x20  u32  codepoint table offset
0x24  u32  display-name offset
0x28  u32  data offset
0x2c  [20] reserved zeroes

Face record, 64 bytes each
0x00  u8   size_px: 19, 22, or 26
0x01  u8   style: Regular=0, Italic=1, Bold=2, BoldItalic=3
0x02  u8   line height
0x03  u8   baseline
0x04  u32  flags, bit 0 = synthetic/fallback source
0x08  u32  metrics offset
0x0c  u32  metric count
0x10  u32  bitmap offset
0x14  u32  bitmap byte length
0x18  u32  kerning offset
0x1c  u32  kerning pair count
0x20  [32] reserved zeroes

Metric record, 12 bytes per codepoint, same codepoint order as the table
u32 bitmap offset within this face bitmap
u16 glyph bitmap byte length
u8  width
u8  height
i8  x offset
i8  y offset
u16 advance in 12.4 fixed-point pixels

Kerning record, 6 bytes
u16 left codepoint
u16 right codepoint
i16 adjustment in 12.4 fixed-point pixels
```

The initial format stores the same codepoint coverage and three reading sizes
as the built-in reading faces. A full Literata test pack is about 1.0 MB.

Firmware support now includes the first device-side slice of the feature:

- `proto::font_pack` decodes the fixed `X4FT` header, face records, and display
  name without allocation.
- The display task probes `/XTEINK/FONTS/CUSTOM.FNT` during boot catalog load
  and catalog refresh.
- A valid pack manifest is stored in `ReaderStore` with its display name and
  identity hash.
- `LibraryEvent::CustomFont` tells the reducer whether a custom pack is
  present; Settings cycles into `Custom` only when that event says it is
  available, and falls back to Literata if the pack disappears.
- The Settings row labels the custom face with the pack display name.
- `FontFamily::Custom` has its own cache-layout value; the reader layout config
  widened the family field and bumped to v17 so Custom cannot collide with the
  version bits.
- When an SD book is open with `FontFamily::Custom`, the display task renders
  cached one-line reader blocks from `/XTEINK/FONTS/CUSTOM.FNT`. The drawer
  reads metrics and one bitmap row at a time, so it never loads a full face or
  whole pack into RAM. If the pack is missing, malformed, or lacks the requested
  face, the view falls back to the compiled renderer instead of failing.
- Cache building now uses the same SD-backed custom metrics for line wrapping
  when the active settings select `Custom`. The v2 cache header version bumped
  to 26 and stores the custom pack identity in both book and section headers,
  so replacing `CUSTOM.FNT` invalidates stale custom pagination even though the
  selected enum slot is unchanged.

The remaining deeper work is performance tuning and user-facing install UX.
The first pagination implementation is deliberately RAM-conservative: it reads
glyph metrics from the pack during cache build instead of loading a whole face
or metric table. That is correct and bounded, but bench runs should decide
whether it needs a small metric cache before this becomes the polished default.

## Compile-time custom font path

Runtime `CUSTOM.FNT` remains the user-facing path: no firmware rebuild, one
file on SD, and Settings exposes it only when the manifest is valid.

There is also an opt-in fast/dev/OEM path that compiles the same custom pack
into firmware as static `BitmapFont` tables:

```sh
.venv-font/bin/python tools/build_font_pack.py build \
  --regular path/to/Regular.ttf \
  --italic path/to/Italic.ttf \
  --bold path/to/Bold.ttf \
  --bold-italic path/to/BoldItalic.ttf \
  --name "Atkinson Hyperlegible" \
  --out target/fonts/CUSTOM.FNT

python3 tools/font_pack_to_rust.py \
  target/fonts/CUSTOM.FNT \
  --out display/src/custom_generated.rs

tools/cargo.sh build --release --features builtin-custom-font
```

The `builtin-custom-font` feature requires `display/src/custom_generated.rs`.
That generated file is intentionally not required for normal builds. When the
feature is enabled:

- `FontFamily::Custom` resolves through the normal static `display::font`
  selector, so pagination and rendering use compiled tables rather than SD
  metric/glyph reads.
- Firmware marks Custom available with the generated display name and identity.
- The generated identity is still stored in v2 cache headers, so rebuilding
  firmware with a different built-in custom face invalidates stale custom
  pagination.
- Built-in custom takes precedence over SD `/XTEINK/FONTS/CUSTOM.FNT`; the SD
  runtime path is for builds without the feature.

## Cache and state implications

The current `reader_layout_config` packs spacing, size, weight, family, and
layout version into `u16`. Custom fonts need identity, not just family.
Options:

- Short term: add `FontFamily::Custom` and bump `READER_LAYOUT_VERSION` whenever
  the custom font changes. Simple but global and awkward; every font replacement
  would retire all caches.
- Better: add a custom-font hash to `BookV2Header`/`SectionV2Header` or a nearby
  cache header field. Built-ins can use hash `0`; custom packs use a manifest
  hash. This changes reader cache artifacts and needs a cache version bump.

`STATE.BIN` currently persists `font_family` as `u8`. It can persist `Custom`
as another enum value, but restore should validate that the custom pack is still
available before applying it.

## UI and import flow

The existing public site is a strong candidate for the first user-facing import
tool. It is already a static browser-hosted firmware/emulator/flasher surface,
so a custom typeface lab could run entirely client-side: the user drops font
files into the page, the browser converts them to an `X4FT` pack, shows proof
images, and gives the user either a `CUSTOM.FNT` download for the SD card or a
Web Serial install path for unlocked/connected devices.

Client-side conversion has two important advantages:

- The site does not have to upload or retain user font files, which avoids a
  surprising privacy/licensing trap.
- The same page can preview the exact bitmap artifact it will install, rather
  than relying on the user's desktop font rendering.

There are two plausible browser implementation strategies:

- **Canvas/FontFace prototype:** load the user's font with the browser
  `FontFace` API, rasterize glyphs into a canvas, threshold the pixels, and
  build the pack in JavaScript. This is good for a fast proof, but text metrics,
  kerning, hinting, and browser differences may drift from the current
  Pillow/FreeType generator.
- **FreeType-in-WASM production path:** compile or bundle a deterministic
  rasterizer/parser to WebAssembly and use the same pack writer as the host
  tool. This is the better long-term route if the firmware cache identity and
  proof sheets need to be reproducible across browsers.

The existing Wireless shelf is also a natural later place to import a typeface
because it already moves files from browser to SD. A future shelf could accept
four files or a zip:

- Regular is required.
- Italic, Bold, and BoldItalic are optional with generated/fallback substitution
  called out during conversion.
- The converter produces a preview/proof sheet before install when running on a
  host; on device, the best it can do is install and reboot.

For an SD-card/manual flow, users could put the site-generated artifact directly
at `/XTEINK/FONTS/CUSTOM.FNT`. A lower-level inbox flow, where users drop raw
TTFs under `/XTEINK/FONT-INBOX/`, should wait until there is a converter outside
the normal reading firmware path.

## Open questions

- How much flash margin does the release firmware currently have after the
  existing built-in font tables?
- Should custom typefaces be allowed to change line metrics, or should v1 force
  existing size baselines so line spacing remains predictable?
- Is Latin/Greek/Cyrillic coverage enough, or should the feature be designed
  around per-book subset generation from the start?
- Do we want one custom slot or a small catalog of installed font packs? One
  slot fits the current Settings UI; a catalog would need a real font browser.
- Should the website use a quick Canvas/FontFace converter for v1, or should it
  wait for a FreeType-in-WASM converter so generated packs are deterministic?
- Can Web Serial write `CUSTOM.FNT` to the device through an existing firmware
  endpoint, or does the first install path need to be SD-card download only?
- Should custom font packs ever be generated on-device by temporarily using the
  radio heap after upload and then resetting? That is attractive but risky.

## Recommendation

Do not add runtime TTF parsing to firmware. Keep the firmware renderer as a
bitmap/metrics blitter and investigate a host/site-generated `X4FT` font-pack
format with one custom slot. The first proof should be a converter plus
preview/emulator support on the public site or host tooling; firmware selection
can follow once the artifact format and memory strategy are proven.
