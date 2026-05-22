# Implementation plan

The repo has moved from hardware proof into a minimal reader shell. The current
target is still reliability before feature breadth.

## Phase 1: panel and buttons

Exit criteria:

- Build succeeds for `riscv32imc-unknown-none-elf`.
- Boot draws a simple reader shell framebuffer.
- Display task initializes SSD1677 and performs a full refresh.
- Power task requests SSD1677 deep sleep through the display task before MCU sleep.
- GPIO3 and ADC ladder buttons produce `InputEvent`s.
- App task updates a page counter from input.
- Power task can enter timer deep sleep after display settle.

Current code status:

| Area | Status |
| --- | --- |
| Workspace and target setup | Done |
| Embassy executor | Done |
| Task boundaries | Done |
| Single 48 KB framebuffer | Done |
| SSD1677 init sequence | SDK-aligned, panel responds |
| Refresh path | Reader shell readable with `MIRROR_X=true`, `MIRROR_Y=false`, `REVERSE_BITS=true`; first update uses full refresh, normal page turns use deterministic fast differential refresh |
| Input backpressure | App accepts input while a render is in flight and coalesces display work to the latest state |
| Input polling | Measured calibrated ADC ladder bands plus CrossPoint-style layout mapping applied; screen shows reader-facing `PREV`/`NEXT`/`BACK`/`OK` labels |
| Reader app shell | Portrait Home/Library/Settings plus landscape Reading/Chapters present with catalog-backed book data |
| Battery display | GPIO0-derived rough battery mV/percent flows through input/app/render |
| Deep sleep | Idle/power sleep now renders a visible sleep screen before SSD1677 deep sleep; GPIO wake pending |
| Partial refresh | Deferred; full-screen fast refresh present |
| NVM progress | Deferred |
| Storage / EPUB / Wi-Fi | EPUB stream reader, FAT scan, `/books` then card-root discovery, and SD-backed hybrid-light section cache present; Wi-Fi still pending |
| Typography | Literata Latin-1 bitmap assets generated; Reading uses Literata for demo text |

## Phase 2: measured board support

- Current calibrated ADC bands on this unit: GPIO1 Back `2400..2700`, Confirm
  `1800..2150`, Previous-front `1000..1250`, Next-front `0..100`; GPIO2
  Previous-side `1500..1800`, Next-side `0..100`. Current layout mapping is
  direct front `BACK_CONFIRM_LEFT_RIGHT` and side `PREV_NEXT`. Raw
  GPIO0/GPIO1/GPIO2 serial logging is available behind `RAW_LOG_ENABLED`.
- GPIO0 battery sampling is present as a rough 2:1 divider estimate; calibrate
  against measured pack voltage.
- Add GPIO wake for the power/home button.
- Record measured BUSY timings in this document.
- Use the on-screen input calibration panel to record raw GPIO1/GPIO2 values for every button.

## Phase 3: reader core

- Add persistent page index.
- `AppStateRecord` exists as a versioned/checksummed storage record; flash-backed
  load/store implementation pending.
- Tiny in-flash/static book source is present as reader-shell pages.
- Home, library, active reading view, chapter navigation, and settings view are
  present as explicit app state. Home now uses the landscape Dock Clean layout
  with the four hardware-adjacent actions on the left and the current book on
  the right. Storage-backed EPUB entries fill the same model.
- `DisplayOrientation` exists with landscape buttons-bottom/top and portrait
  buttons-left/right modes; default is landscape buttons-bottom.
- Keep app state as flat structs and render requests as small `Copy` messages.

## Phase 4: storage and EPUB

- `proto::storage` defines a bounded `BookStorage` trait, `/books`/card-root
  candidates, and case-insensitive `.epub` filtering.
- `proto::book` defines shared `BookMeta`, `BookProgress`, `ChapterMeta`, and
  catalog primitives used by Home, Files, Reading, and Chapters.
- `proto::epub` can locate ZIP central directories, read stored/deflated entries
  into caller-owned buffers, parse `META-INF/container.xml`, parse OPF metadata,
  manifest, and spine, and map XHTML tags into styled text runs.
- `proto::epub::ZipStream` can locate and read ZIP entries through a bounded
  `ReadAt` interface, so EPUBs no longer need to fit in memory. The firmware
  path now streams compressed deflate input in chunks, and XHTML spine entries
  can decode a bounded prefix for the first section cache instead of failing
  just because the section is larger than the XHTML scratch buffer.
- `proto::text` defines Literata/Bookerly-ready font/style roles and a
  deterministic one-screen paginator over bounded styled runs.
- `proto::cache` defines bounded binary cache records for book, TOC, section,
  page, line, word, and block data.
- Firmware Files/Home/Reading now consume the shared catalog/cache model through
  the refactored `ReaderStore`. The current in-flash demo book remains a
  fallback source while SD EPUB loading is hardened.
- The selected-book preview path has been replaced by `build_or_load_book_cache`.
  First open writes `/XTEINK/CACHE/E<hash>/BOOK.BIN`, builds the requested
  section into `/XTEINK/CACHE/E<hash>/SECTIONS/SNNN.BIN`, and renders from those
  flat records. Near-end NEXT requests a larger cached page target before
  rendering, so partial section caches can extend on demand.
- Home can now draw a selected-book cover bitmap from
  `/XTEINK/CACHE/E<hash>/COVER.BIN`. The firmware format is a fixed 202x303,
  1-bit, row-packed DOD bitmap; if it is absent or invalid, the Dock Clean
  fallback artwork is used. Host preview tooling can generate this cache file
  from an EPUB cover image.
- Current limitation: partial caches for very large single-XHTML spine items
  can render the first decoded chunk, but true byte-accurate resume inside that
  same compressed member is still pending.
- `BOOK.BIN` stores book/spine/TOC records plus a shared string blob. Section
  files store a section header, page records, block records, paragraph flags,
  and UTF-8 text bytes. Line/word cache records remain defined in `proto::cache`
  for the next rendering refinement; the current firmware renderer still draws
  styled block text with Literata.
- `/XTEINK/STATE.BIN` writes the encoded `AppStateRecord` for SD reading
  progress. Version 2 stores the volatile book id plus stable SD source identity
  derived from path and file size; boot/Home restore scans the card, maps the
  record back to the matching EPUB, and keeps v1 decode fallback for older state
  files.
- Home `Read` now resumes the restored/last-selected SD EPUB. If no current EPUB
  exists, it opens Files when SD books exist and falls back to the built-in book
  when the card is empty or unavailable.
- X4 SD pins are configured on the shared SPI bus: SCK GPIO8, MOSI GPIO10, MISO
  GPIO7, SD CS GPIO12. `embedded-sdmmc` is present with default features
  disabled.
- The display task remains the single runtime coordinator for serialized EPD and
  SD transactions, while SD discovery, EPUB cache construction, reader layout,
  view drawing, and EPD flushing now live in deeper `fw` modules.

## Phase 4b: typography and preview

- `tools/generate_literata.py` downloads OFL Literata TTFs and generates Latin-1
  bitmap tables for Regular, Italic, Bold, and BoldItalic.
- `display::font` renders generated bitmap glyphs directly into the framebuffer.
- Reading mode and the chapter navigation screen use Literata; tiny 5x7 remains
  for debug/status chrome and non-reader utility views.
- `tools/preview` exports PBM/PNG snapshots for Home, Files, Reading, Chapters,
  and Settings into `target/previews`. It can also render EPUB parser previews
  from host-side files for layout inspection before flashing.

## Phase 5: Wi-Fi sync

- Enable `esp-wifi`.
- Keep transfer chunks caller-owned or borrowed, not embedded as large enum
  variants.
- Write book data directly to storage in bounded chunks.

## Verification commands

```sh
cargo check --target riscv32imc-unknown-none-elf --release
cargo test -p proto --target aarch64-apple-darwin
cargo clippy --workspace --target riscv32imc-unknown-none-elf --release -- -D warnings
cargo run --manifest-path tools/preview/Cargo.toml --target aarch64-apple-darwin
```
