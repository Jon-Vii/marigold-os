# PRD: PNG images in EPUB books

Status: implemented on host; hardware validation pending

## Summary

Render PNG images embedded in EPUB books on the reading screen. Today `<img>`
becomes an `alt`/`"[Image]"` text placeholder (`proto/src/epub.rs`), no PNG
decoder exists in `fw/` or `ui/`, and covers are host-pre-rendered 1-bit files
(`tools/preview`). This feature adds an on-device path: decode, dither, cache,
and draw inline images within the firmware's standing rules — `no_std`, zero
heap in the reading path, Copy-only messages, one SPI owner.

## Non-goals (v1)

- JPEG (planned follow-up; the sidecar/dither pipeline is built to accept it)
- Library covers decoded on-device (stay host-pre-rendered)
- SVG-wrapped images (inside the already-skipped `<svg>` scope)
- 16-bit samples, Adam7 interlacing, gAMA/sRGB color management

## Design

- **Streaming zero-alloc PNG decoder** in `proto/src/png.rs`, adapted from the
  permissively licensed filter/defilter logic of `png-decoder` (bschwind).
  Fed by byte chunks; IDAT inflation reuses the `miniz_oxide` streaming
  `InflateState` pattern already proven in the ZIP path. Accepted inputs:
  non-interlaced, 8-bit grayscale / RGB / palette (tRNS alpha composited
  onto white), source width ≤ 792 px (panel width), box-filter downscale to
  the layout rect. Everything else falls back to the existing `alt`
  placeholder.
- **RAM-driven revision:** the decoder's working buffers are carved from the
  idle EPUB XHTML scratch window during the decode pass, not statics — the
  X3 stack floor left only ~3.3 KB of slack, far below the ~10 KB a static
  decoder would need. Deflated PNG entries are spooled compressed to the
  card (`SPTMP.BIN`) so the ZIP and PNG inflate stages reuse the one
  43 KB `InflateState` sequentially.
- **1 bpp with Bayer ordered dither.** The panels can do 4 gray levels, but
  doubling both framebuffers does not fit the dram2/stack budget. Position-
  stable Bayer avoids fast-refresh flicker against the dram2 previous frame.
- **Decode + dither at section-build time**; the dithered 1-bpp result is
  cached to an SD sidecar next to the section file, keyed by the section
  header's viewport/font config. Page turns stream ~52 KB from SD instead of
  decoding. Failure to decode keeps the alt placeholder.

## Phases

1. `proto/src/png.rs` decoder + inline host tests
2. Parser/model: `XhtmlBlockSink::push_image`, `<img>` handling, image
   records, `CACHE_V2_VERSION` 27→28
3. Layout (`ui/src/reading.rs`) + section-build sidecar writer
4. Render (`ui::reading::draw_reading_page_body`), static decode buffers in
   `fw/src/tasks/display.rs`, refresh policy for image pages
5. Goldens, full verification gate, hardware bench
