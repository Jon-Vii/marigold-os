# xteink-x4-os architecture

This firmware is a bare-metal Rust bring-up for the Xteink X4 e-ink reader:
ESP32-C3, SSD1677, 800x480 monochrome panel, no PSRAM.

The design goal is not to imitate a desktop OS. It is a small data pipeline:

```text
buttons -> app state -> display command -> framebuffer -> SSD1677 RAM -> refresh -> sleep
```

## Current architecture diagram

![Current architecture diagram](architecture.png)

```mermaid
flowchart TD
    buttons["GPIO3 power button<br/>ADC button ladders"]
    input_task["input_task<br/>debounce + classify buttons"]
    app_task["app_task<br/>owns ReaderState reducer shell"]
    display_task["board I/O + display task<br/>single owner of EPD bus, SD CS,<br/>ReaderStore, framebuffer"]
    power_task["power_task<br/>idle timer + deep sleep"]
    wifi_task["wifi_task<br/>kosync sync session"]

    app_core["app-core<br/>Copy message contracts<br/>ReaderState reducer<br/>RefreshPlanner"]
    display_crate["display<br/>1 bpp framebuffer<br/>drawing + fonts<br/>SSD1677 transforms"]
    proto["proto<br/>bounded book/storage/text/cache models<br/>ZIP/EPUB/XHTML parser pieces"]
    hal_ext["hal-ext<br/>SPI DMA, RTC, NVM helpers"]
    ui["ui<br/>bounded layout/render helpers"]

    epd["SSD1677 e-ink panel<br/>800x480 BW/RED RAM"]
    sd["microSD FAT<br/>/BOOKS + card root EPUBs<br/>/XTEINK cache + state"]
    sleep["ESP32-C3 deep sleep"]

    emulator["tools/emulator<br/>host reducer + panel protocol model<br/>scenario/golden-frame runner"]
    preview["tools/preview<br/>host render/export inspection"]
    fixtures["fixtures<br/>TOML scenarios + golden PNGs"]

    buttons -->|"raw samples"| input_task
    input_task -->|"InputEvent"| app_task
    app_task -->|"DisplayCommand::Render / Sleep"| display_task
    app_task -->|"StorageCommand"| display_task
    display_task -->|"DisplayEvent::Settled / Asleep<br/>LibraryEvent"| app_task
    app_task -->|"PowerEvent::Activity"| power_task
    display_task -->|"PowerEvent::DisplaySettled / DisplayAsleep"| power_task
    power_task -->|"DisplayCommand::Sleep"| display_task
    power_task --> sleep

    app_task -.-> app_core
    display_task -.-> app_core
    display_task -.-> display_crate
    display_task -.-> proto
    display_task -.-> hal_ext
    display_task -.-> ui

    display_task -->|"framebuffer flush<br/>full/fast refresh"| epd
    display_task -->|"SD session<br/>catalog, cache, progress"| sd

    emulator -.-> app_core
    emulator -.-> display_crate
    emulator -.-> proto
    emulator --> fixtures
    preview -.-> display_crate
    preview -.-> proto
```

## Rules

- `#![no_std]`, no heap allocation in firmware paths.
- One 48 KB 1 bpp framebuffer.
- Display ownership is single-writer: only `display_task` touches the EPD bus.
- Reader state ownership is single-writer: only `app_task` mutates page/menu state.
- Messages are small `Copy` values. Bulk bytes stay in caller-owned buffers.
- Power requests display sleep through `display_task`; it never touches SPI.
- Hardware assumptions live in one of two places:
  - X4 board wiring in `fw/src/main.rs` and `fw/src/tasks/input.rs`.
  - SSD1677 protocol in `display/src/epd.rs`.

## Workspace

```text
app-core/ app state reducer and Copy message contracts shared by firmware/tools
display/   framebuffer, drawing primitives, SSD1677 constants and address math
hal-ext/   thin async wrappers over ESP HAL peripherals
fw/        boot, Embassy executor, task wiring, board-owned peripherals
ui/        shared shell rendering plus ui::reading, the reader page-plan seam
           (page bounds, ink measurement, wrapping) used by fw and host tools
proto/     bounded book/storage/text/cache models plus ZIP/EPUB/XHTML parser pieces
tools/emulator/ host-side development emulator and scenario runner
```

## Embassy tasks

```text
app_task
  owns ReaderState
  InputEvent -> DisplayCommand::Render
  modes: Library, Reading, Chapters, Settings

board_io/display task
  owns EpdBus, SD CS, ReaderStore, and Framebuffer
  DisplayCommand::Render -> pure framebuffer render from the current ReaderStore snapshot
  StorageCommand::* -> serialized SD/FAT/catalog/cache work on the shared SPI bus
  DisplayCommand::Sleep -> sleep screen full refresh -> SSD1677 deep sleep -> PowerEvent::DisplayAsleep
  sends DisplayEvent::Settled to app_task when render completes

input_task
  polls GPIO3 and ADC ladders
  debounced ADC/power edges -> reader Button actions -> InputEvent

power_task
  observes activity and display-settled events
  asks display_task to sleep the SSD1677, then enters ESP32-C3 deep sleep

wifi_task
  parked until SyncCommand::Start arrives from the Sync screen
  requests StorageCommand::LoanSyncMemory, receives the dismantled EPUB
  scratch as radio heap, joins Wi-Fi in STA mode, exchanges the active
  book's position with a kosync server, reports SyncEvents to app_task
  then serves the browser shelf page at the device's LAN address
  SyncCommand::Exit (the done press) ends the session with a software reset
```

## Wi-Fi sync session

Sync is a one-way modal session because the radio blob needs ~80 KB of
heap this firmware does not have while reading. `fw::sync_mem` owns the
plumbing: the display task dismantles the EPUB scratch into raw regions
(`reader_cache::dismantle_scratch`), and the wifi task donates them plus a
16 KB claim on the otherwise unused dram2 boot-loader shadow segment to
esp-alloc. The previous-frame framebuffer also lives in dram2 now so
esp-wifi's static demand fits in main DRAM with the ~41 KB stack region
intact. The smaller scratch buffers are reused directly as TCP socket and
HTTP buffers. Once loaned, the reader pipeline cannot come back: leaving
the Sync screen after the radio ran maps to `SyncCommand::Exit`, which is
a software reset; boot restore then reloads the saved position.

Progress flows both ways: before the loan, the display task flushes
pending progress, loads the saved book through the ordinary cache path if
needed, and ships the kosync identity (KOReader partial-MD5 of the EPUB
file), position permille, and chapter map with the loan. The wifi task
pulls the server position first and pushes ours only if it is ahead; a
pulled position lands through the still-working `StoreProgress` path so it
survives the session-ending reset. `proto::kosync` holds the sans-IO
protocol pieces (MD5, partial digest, HTTP request building and response
parsing) with host tests.

The session does not reset right after the kosync exchange: the wifi task
keeps serving a shelf page at the device's LAN address (the Sync screen's
`Serving` status hands out the URL, with Confirm as the done key). The
page lists the catalog, shows real upload progress, and offers per-book
removal. Routes: `GET /` serves the page, `GET /list` returns the catalog
snapshot shipped with the loan, `POST /upload?name=` streams raw EPUB
bytes, and `POST /delete?name=` removes a book (card-root entries carry
`root=1`; uploads always land in /BOOKS). Upload bytes reach the display
task — still the single SD owner — through `fw::upload`'s two-buffer
ping-pong: 4 KB chunks carry loaned buffers one way and the buffers come
back on a return channel once written. The display task holds one SD
session for the whole upload phase and writes `/BOOKS/<8.3>.EPU` (the
catalog scan accepts `.epu` alongside `.epub`). The done press waits for
any in-flight upload before the session-ending reset; the boot rescan
then surfaces the new books.

Station credentials come from `/XTEINK/WIFI.BIN` (written by the
onboarding portal below), falling back to compile-time `option_env!`
values (`XTEINK_WIFI_SSID`/`XTEINK_WIFI_PASS`) for dev builds. The kosync
account stays compile-time for now (`XTEINK_KOSYNC_HOST`/`_USER`/`_PASS`).

With no credentials anywhere, starting sync raises the onboarding portal
instead: an open `XTEINK-X4` hotspot at 192.168.4.1 with a captive DHCP
server, a DNS catch-all (every name resolves to the portal, which makes
phones raise their sign-in sheet unprompted), and a credential form on
port 80. The Sync screen shows a baked-at-build-time join QR
(`tools/generate_qr.py`). Submitted credentials travel to the display
task as a `StoreWifiCredentials` Copy message, land in WIFI.BIN, and the
next session joins as a station. `proto::captive` holds the sans-IO
DHCP/DNS/HTTP codecs under host tests; the wifi task only owns sockets.

Embassy is used for cooperative waits: ADC retry delays, button polling, SPI DMA
transfers, BUSY waits, and sleep windows all yield instead of spinning. The real
battery win comes after display settle: the power task asks the display task to
draw a visible sleep screen, power down the SSD1677, then move the ESP32-C3 into
deep sleep. The power button also requests this same sleep path instead of being
treated as ordinary navigation input.

Input/render backpressure is intentionally coalesced. The app keeps at most one
display render in flight. While the display is refreshing, new button events
still update `ReaderState`, but they set a single pending-render flag instead of
queuing stale framebuffer renders. When `DisplayEvent::Settled` arrives, the app
renders the latest state once.

Storage is also explicit. Files/Home/Reading transitions enqueue
`StorageCommand`s after the visible render settles; render commands never scan
FAT, open EPUBs, build caches, or write progress. Open/extend requests whose
page already sits inside the loaded section window are answered from RAM
without an SD session, and reading-progress writes are coalesced (at most one
STATE.BIN write per 15 s, flushed before display sleep). The board I/O task is still
the single SPI owner, so display refresh and SD transactions cannot overlap, but
the user-facing view is always drawn from the latest already-owned snapshot.
SD/FAT access goes through an SD session: the board I/O task deselects the
display, clocks the bus down for the card (400 kHz identification with wake
clocks, then 20 MHz data), opens the FAT root, performs one storage action, and
restores 40 MHz display SPI before returning to EPD work. The card stays powered
between sessions while the device is awake, so only the first session runs the
full CMD0/ACMD41 init; later ones reuse the remembered card type and skip the
handshake, falling back to a cold init if a reused session cannot open the
volume. Deep sleep resets the chip and clears that state.

## Display model

`display::fb::Framebuffer` is the source of truth. White is bit `1`, black is
bit `0`, row-major, 100 bytes per row.

The SSD1677 path writes the current framebuffer to BW RAM (`0x24`). The first
refresh after boot/display sleep also writes the current framebuffer to RED RAM
(`0x26`) and runs a full waveform. Normal page turns use a second retained
framebuffer as the RED RAM previous-frame source, then trigger the SSD1677 fast
waveform. This avoids the multi-flash full-update behavior during ordinary
reader navigation. Periodic full-refresh cleanup is available behind a constant
but currently disabled so page-turn behavior is deterministic during bring-up.

`display::epd` contains three transform constants currently validated during bring-up:
`MIRROR_X = true`, `MIRROR_Y = false`, and `REVERSE_BITS = true`. The logical
framebuffer API stays upright while firmware and host tools remap bytes/bits
before panel-RAM writes. This fixes the X4 panel's observed horizontal byte
order and bit order without leaking hardware orientation into app rendering.
`MIRROR_Y=true` was tested and rejected because it made glyphs vertically
mirrored/upside down.

Physical orientation is an app/layout concern, not an SSD1677 streaming concern.
The current readable build places logical top on the physical button side. The
reader state already carries a complete orientation enum:

```rust
enum DisplayOrientation {
    LandscapeButtonsBottom,
    LandscapeButtonsTop,
    PortraitButtonsLeft,
    PortraitButtonsRight,
}
```

Default reader mode is `LandscapeButtonsBottom`, but the low-level display
transform above should stay fixed unless corruption returns.

Addressing follows the OpenX4 community SDK behavior:

- SPI mode 0, 40 MHz.
- BUSY is active high.
- X window is pixel-addressed, `0..799`.
- Y gate scan is reversed, so the full Y window is `479..0`.

## Data-oriented design

State is plain data, not object graphs:

```text
InputEvent        Copy enum
ReaderState       view/book/page/chapter/settings/battery fields
RenderRequest     view/book/page/orientation/refresh/battery/dirty rect
Layout<N>         parallel arrays of kind/rect/parent/text span
Framebuffer       single flat byte array
```

`app-core` owns the reader reducer and the shared message contracts. The
firmware `app_task` is an Embassy shell around this pure reducer, and host tools
use the same reducer for deterministic navigation tests. This keeps button flow,
library events, restore events, orientation, refresh policy, and render requests
from drifting between device and emulator.

EPUB work keeps the same shape:

```text
SD file -> ZIP entry -> inflate window -> XML token -> flat cache record -> glyph blit
```

No DOM, no heap object graph, and no entire-book-in-RAM reader model. Parsers
are allowed to be state machines, but their output is immediately flattened into
bounded records.

`proto` owns the reader data contracts shared by Home, Files, Reading, Chapters,
and the host preview tool:

- `BookMeta`, `BookProgress`, and `ChapterMeta` for catalog and progress data.
- `BookStorage` and `FileCandidate` for microSD-backed `.epub` discovery.
- `ZipArchive` for host-side central-directory lookup and stored/deflated entry
  reads into caller-owned buffers.
- `ZipStream` for central-directory lookup and entry reads through a bounded
  `ReadAt` interface, which is the path storage-backed EPUBs use. Firmware ZIP
  reads stream deflate input through a reusable inflater scratch state, so large
  compressed members do not have to fit in the compressed scratch buffer.
- `EpubZipOps` as the narrow zip-entry interface cache loaders program
  against. Both zip front-ends implement it, and one shared streaming inflate
  engine sits behind them, so entry reads behave identically regardless of
  whether compressed bytes come from random-access or forward-only storage.
- `EpubPackage` for container/OPF metadata, manifest, and spine.
- `xhtml_blocks_to_sink` with `TextRole`, `FontStyle`, and `TextAlign` as the
  single XHTML extraction path feeding bounded block records.
- `BookV2Header` with `BookV2SectionRecord`, and `SectionV2Header` with
  `PageRecord`, `BlockRecord`, and `TocRecord`, for the bounded binary cache
  records the firmware reads and writes. The earlier `BookCacheHeader`,
  `SectionHeader`, `PageCacheHeader`, `LineRecord`, and `WordRecord` remain in
  `proto::cache` only for the disabled V1 migration path.

The firmware still ships one built-in catalog entry as a fallback, but the
board I/O task owns the shared SPI bus while it scans FAT16/FAT32
microSD cards for EPUBs under `/books` and then the card root. X4 SD pins are
configured on the shared SPI bus (SCK GPIO8, MOSI GPIO10, MISO GPIO7, SD CS
GPIO12). SD transactions and display refreshes remain serialized by that single
board-I/O owner.

## SD-backed reader cache

The SD reader uses a V2 whole-book cache. Opening an EPUB parses OPF/TOC/spine,
then builds the whole book up front: every spine item paginates into one or more
fixed-size sections, each section is written to its own file, and a book index
records where each section sits. After that the book reopens from cache in tens
of milliseconds; only the first build of a large book is slow (minutes for
something HPMOR-sized).

A chapter is a spine item, and a long one paginates into several sections. The
builder closes the current section and opens the next when its in-RAM arena
fills, where the text budget (16 KB) is the binding limit for prose, well ahead
of the block (384) and page (96) caps. Sections are invisible while reading: the
reader walks across them seamlessly, and the footer page-in-chapter counter
aggregates every section sharing a spine. The book index holds up to
`MAX_BOOK_SECTIONS` (320, on the order of 4,500 pages); a longer book caches
`partial`.

Each section header carries a `font_config` that packs `READER_LAYOUT_VERSION`
with the type size and spacing it was paginated under. A loaded section whose
version or size no longer matches is invalid and forces a rebuild, so bumping
`READER_LAYOUT_VERSION` retires every stale cache after a layout or
cache-encoding change; a spacing-only change re-walks line heights without a
reparse.

Cache paths use FAT 8.3-safe names because `embedded-sdmmc` operates on short
file names in the firmware path. The library list is a separate flat catalog
snapshot at `/XTEINK/CATALOG.BIN`. On boot/refresh, firmware first tries to load
this cached snapshot, then refreshes `/BOOKS` and card-root discovery in a
storage command. Files renders the current snapshot immediately. It may show
“Library unavailable” before any successful cache/scan, and “No books found”
only after a completed scan proves the card has no EPUBs.

```text
/XTEINK/CACHE2/E<hash>/BOOK.BIN
/XTEINK/CACHE2/E<hash>/COVER.BIN
/XTEINK/CACHE2/E<hash>/SECTIONS/S000.BIN
/XTEINK/CACHE2/E<hash>/SECTIONS/S001.BIN
/XTEINK/STATE.BIN
```

`BOOK.BIN` holds a `BookV2Header`, one `BookV2SectionRecord` per section (spine,
start page, page count, partial), TOC records, and a string blob for title,
author, and TOC titles. Section files hold a `SectionV2Header`, page records,
block records, per-block paragraph flags, and the UTF-8 text blob of that
section's pre-wrapped lines. The active firmware state keeps only loaded book
metadata, the full section index, the active section's page/block records and
text bytes, and small ZIP/XML scratch buffers. Spine XHTML members of any size
stream completely through the resumable block parser in bounded inflate
windows, so chapter content is never truncated by scratch-buffer limits. `STATE.BIN`
stores the encoded `AppStateRecord`; version 2 records include the SD source
size and path-derived hash so boot restore can map saved progress back onto the
scanned SD catalog instead of trusting a volatile list index.

`COVER.BIN` is an optional Home-cover sidecar for the same cache key. It stores
a tiny header followed by a 202x303, 1-bit, row-packed bitmap matching the Dock
Clean cover slot. Firmware treats it as flat DOD data: valid records are drawn
directly, while missing or invalid records fall back to generated cover art. The
host preview tool can generate the sidecar from EPUB JPEG/PNG covers with
`--cover-bin` or write it directly to a mounted SD cache path with `--sd-root`.

Reading and chapter navigation typography use generated Literata bitmap assets.
The host generator downloads OFL Literata TTFs and emits Latin-1 glyph
metrics/bitmaps for Regular, Italic, Bold, and BoldItalic. Firmware does not
rasterize TTFs on-device.

## Development emulator

`tools/emulator` is a host-side parity tool for fast development loops. It has a
headless scenario runner for agents/CI and an optional egui frontend for manual
interactive testing. The default build is headless; the desktop window is built
with `--features gui` to keep routine checks lightweight.

The emulator intentionally models the behavior that is useful during ordinary
development:

- app reducer state transitions from button and library events
- 800x480, 1 bpp framebuffer rendering
- shared panel byte/bit transform from `display::epd`
- SSD1677-style BW/RED RAM, address counters/ranges, refresh mode history, and
  deep-sleep command validation
- scripted scenarios that can assert final view/book/page/selection/panel state,
  dump PNG frames, and compare against golden frames

It does not model ESP32-C3 CPU timing, ADC noise, SPI DMA edge cases, BUSY
timings, voltage/temperature behavior, or true e-paper waveform physics. Those
remain hardware-validation concerns.

Typical development loop:

```sh
cargo test -p app-core -p proto --target aarch64-apple-darwin
cargo test --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features -- --scenario fixtures/scenarios --check fixtures/golden
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features -- --scenario fixtures/scenarios --dump target/emulator
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --features gui -- --gui
```

## Reader app model

The firmware now has the e-reader surfaces as explicit app state:

- `Home`: current book cover/metadata plus Continue, Library, Sync, and Settings.
- `Library`: selects a book or opens settings.
- `Reading`: owns the active book/page position.
- `Chapters`: selects a chapter within the current book.
- `Settings`: cycles orientation and refresh policy.

Every surface renders in landscape: the X4 is held that way for its side page
buttons, so `Home`, `Library`, and `Settings` share the reading posture rather
than rotating into portrait. Home is cover-led: the current book is the visual
anchor, with a restrained menu down the side for Continue, Library, Sync, and
Settings.
Reading mode keeps the page quiet: tiny book title, rendered-screen count within
the chapter, symbolic battery, and a thin whole-book progress bar. Home shows a
small battery percentage because it is a status surface. GPIO0 is sampled as the
current rough battery source using a 2:1 divider assumption and a simple
3300-4200 mV LiPo percentage curve. The current book may be the built-in
fallback or the restored/last-selected microSD EPUB. Home triggers SD scan and
state restore on first render, then `Read` resumes the current EPUB through the
same cache-loading path as Files. If there is no current SD EPUB, `Read` opens
Files when EPUBs are present and falls back to the built-in reader when the card
is empty or unavailable. SD EPUBs use the same flat book/chapter/page fields as
built-in content, but page bodies come from the SD-backed cache instead of
static text arrays.

## Current module map

`fw/src/tasks/display.rs` is intentionally the only task touching the EPD bus and
coordinating SD access. It is now the orchestration layer:

```text
display task orchestration
  receives DisplayCommand
  triggers SD scan and EPUB cache loading when needed
  calls view rendering into the framebuffer
  selects refresh mode
  flushes or sleeps the panel
  publishes display/power/library events
```

The deeper modules keep implementation complexity behind narrow data-oriented
interfaces:

```text
fw::display_flush  SSD1677 init, RAM streaming, sleep, and byte transforms
fw::library_sd     FAT scan, SD chip-select handling, and file discovery
fw::reader_cache   EPUB-to-cache loading into bounded proto::cache records
fw::reader_layout  page indexing, line wrapping, style markers, measurements
fw::reader_store   bounded loaded-book/library state shared by cache and views
fw::views          Home/Files/Reading/Chapters/Settings drawing
fw::tasks::display task loop, refresh policy, and event publishing
```

Do not split this by moving bus access into a second task unless there is also a
proper request/response protocol for the shared SPI bus. The current invariant
that display refresh and SD reads cannot overlap is more important than file
size.

Persistent app state is represented by `hal_ext::nvm::AppStateRecord`, a compact
versioned/checksummed record for book id, chapter, rendered screen, shell
orientation, reading orientation, refresh policy, source hash, and source file
size. The firmware stores it at `/XTEINK/STATE.BIN` for SD reading progress;
flash/NVM fallback remains separate from the record format.

## Bring-up checklist

1. Flash firmware and confirm the reader shell appears.
2. Measure BUSY on GPIO6 during reset and refresh.
3. Confirm full refresh timing.
4. Confirm `TL`, `TR`, `BL`, and `BR` are readable and map consistently.
   Current readable transform: `MIRROR_X=true`, `MIRROR_Y=false`,
   `REVERSE_BITS=true`. Logical top currently appears on the physical button
   side; handle this later through `DisplayOrientation`.
5. Validate the Adafruit-scaled ADC ladder bands against this physical unit.
   Current calibrated bands are GPIO1 Back `2400..2700`, Confirm `1800..2150`,
   Left `1000..1250`, Right `0..100`; GPIO2 Up `1500..1800`, Down `0..100`. Raw
   hardware buttons then pass through a CrossPoint-style mapping layer into
   reader actions: front `BACK_CONFIRM_LEFT_RIGHT`, side `PREV_NEXT`. Both
   previous-page buttons emit `Previous`; both next-page buttons emit `Next`.
   Raw ADC serial logging and on-screen GPIO values are now behind debug
   constants so normal firmware only refreshes on debounced button edges.
6. Measure deep-sleep current.
7. Only then add partial refresh, NVM progress, storage, and Wi-Fi sync.
