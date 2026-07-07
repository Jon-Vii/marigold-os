# Xteink X3 Support — Implementation Plan

Status: **hardware-verified.** The `device-x3` build has working UC8253 panel and
BQ27220 battery support, and the release path now publishes X3 app/SD images beside the
X4 images. The 4-pin magnetic pogo cable is still required for USB serial/flashing; the
2-pin cable is charge-only and cannot flash.

## Validation update (2026-07-07)

The X3 path is no longer speculative: panel bring-up and battery reads have been validated
on real hardware. The remaining release-site work treats X3 as first-class: `firmware-x3.bin`
for app-only flashing and `FWUPDX3.BIN` for SD updates.

## Progress (2026-07-06)

Done and committed — the `device-x3` build compiles, both variants pass clippy, the X4
build is byte-identical (119 host tests + 6 reading goldens green), and the X3 shell was
eyeballed in the preview tool (apparatus back in the corner, nothing clipped):

- **Phase 1** — `device-x3` feature on the `display` crate selects 792×528; `fw` forwards
  it; band/DMA size coincidence is now an explicit assert.
- **Phase 2 seam only** — `display::epd` and `fw::display_flush` split into per-controller
  modules. SSD1677 moved verbatim; `RefreshMode`/`SpiOp`/band-transform are the shared
  surface; the X3 build compiles against `todo!()` UC8253 skeletons. Trimmed the radio's
  dram2 heap 16→13 KB so the larger X3 framebuffer fits the segment.
- **Layout geometry** — the reader page box and shell footer were pinned to 800×480
  literals (`READER_RIGHT_X=792` *equals* the X3 width → edge-touching ink). Re-expressed
  as panel-relative edge insets: identity on X4, correct margins on X3.
- **Phase 4** — reader layout version salted with panel geometry (section caches rebuild
  across panels); `POS.BIN` checksum salted so a cross-panel saved position resets to book
  start (salt is 0 on X4, compile-asserted, so no existing position is wiped on upgrade).
- **Phase 5 (partial)** — `device-x3` pass-through in the emulator/web-emulator/preview
  manifests; the SD firmware trigger filename is per-panel (`FWUPDATE.BIN` vs
  `FWUPDX3.BIN`) so a card is safe to move between devices; FLASHING.md updated.

### UC8253 driver — DONE (2026-07-06), hardware-verified (2026-07-07)

Phase 2 bodies are ported from CrossPoint's `Uc8253X3Driver` (MIT), BW path only:
`display/src/epd/uc8253.rs` (command set, 4 BW LUT banks, init/resolution bytes,
vertical-flip orientation, 16 MHz SPI) and `fw/src/display_flush/uc8253.rs` (the refresh
flow — Fast=DTM2-vs-DTM1 turbo, Full=white-DTM1 baseline + settle, FastClean=absolute
half scrub, prestage→DTM1). Added `EpdBus::wait_two_phase` for the X3's low-then-high
BUSY. The port started from the CrossPoint reference and has since been exercised on the
X3 panel.

### Phase 3 — BQ27220 battery — DONE (2026-07-06), hardware-verified (2026-07-07)

`hal-ext/src/bq27220.rs` is a small generic async gauge driver (SOC / voltage / charging
sign). `main.rs` cfg-swaps GPIO0 from the aux ADC to I2C0 SCL (SDA=GPIO20, 400 kHz) on
the X3 and builds the gauge; `tasks/input.rs` cfg-splits the power read — X4 aux ADC path
byte-identical, X3 reads the gauge. Button ladders (GPIO1/2) untouched. Not yet wired:
a charging indicator (the gauge exposes `charging()`, but the UI has no such element on
either device — add together if wanted). Phase-6 note: confirm the I2C pins park safely
before deep sleep (µA leak against the always-powered gauge).

### Still open

- **Release smoke.** Build the tagged X3 release asset and run one app-only flash plus one
  `FWUPDX3.BIN` SD update before calling the public release fully exercised.

### Key-hint positions — DONE (2026-07-06)

The X3 has the same button arrangement as the X4, on a physically smaller/different-sized
panel (confirmed by the device owner; an earlier research pass over CrossPoint's hint
renderer wrongly suggested a bottom+sides topology — disregarded). So `KEY_YS` needs no
redesign, just the same panel-relative treatment as the other geometry constants:
positioned as a fraction of panel height (`key_y` in `ui/src/render.rs`), identity on the
X4, so the left-margin key column stays balanced on the X3's taller panel instead of
bunching toward the top. Verified in the preview render.

### Deferred

No build/test CI exists (only a Pages deploy workflow), so a "build both feature sets"
job needs a CI-pipeline decision rather than a mechanical edit.

---

## Original plan

Everything up to the on-device phases can be built and code-reviewed without the device.

## Background

The firmware today is X4-only. The X3 turns out to be a sibling board: same ESP32-C3,
same 16 MB flash, same partition table, and the same GPIO wiring for the display SPI,
SD card, button ladders, and power button. Four things differ, and they define the work:

| | X4 (current) | X3 |
|---|---|---|
| Panel | GDEQ0426T82 4.26", 800×480, **SSD1677** | 3.68", **792×528**, **UC8253** |
| Battery | ADC divider on GPIO0 (2.0×) | **BQ27220 fuel gauge**, I2C SDA=GPIO20 / SCL=GPIO0 @400 kHz |
| USB | USB-C, native USB-Serial-JTAG | 4-pin pogo connector (same USB-Serial-JTAG behind it) |
| Extras | — | QMI8658 IMU (0x6B), DS3231 RTC (0x68) — both optional for us |

Shared wiring (identical on both): EPD SCK 8 / MOSI 10 / CS 21 / DC 4 / RST 5 / BUSY 6;
SD MISO 7 / CS 12; button ADC ladders GPIO1 (nav) + GPIO2 (page); power button GPIO3
(active-low, deep-sleep wake). Note GPIO0 and GPIO20 are the *repurposed* pins: X4 uses
them for battery ADC and USB detect, X3 uses them as the I2C bus for the fuel gauge.

Primary porting reference: **CrossPoint Reader** (MIT), `github.com/crosspoint-reader/crosspoint-reader`
— clone it with `--recurse-submodules` (the `freeink-sdk` submodule has the drivers). Key files:

- `freeink-sdk/libs/display/FreeInkDisplay/src/driver/Uc8253X3Driver.{h,cpp}` — the production X3 panel driver
- `freeink-sdk/libs/display/FreeInkDisplay/src/lut/Uc8253X3Luts.h` — the LUT banks
- `freeink-sdk/libs/hardware/BoardConfig/include/BoardConfig.h` — both boards' pin profiles
- `freeink-sdk/libs/hardware/XteinkDetect/` — the X3-vs-X4 I2C fingerprint probe

(papyrix-reader's docs claim the X3 panel is SSD1677; CrossPoint's shipping driver says
UC8253 and is the production lineage — trust CrossPoint, but keep the doubt in mind until
the first on-device init succeeds.)

## Decision: two feature-flagged builds, not runtime detection

CrossPoint compiles both panels into one binary, sizes the framebuffer to the max, and
picks a board profile at boot by probing the X3-only I2C chips. We deliberately do **not**
copy that. This codebase is statically sized everywhere (fixed framebuffer, 43 KB stack
budget, the sync-mode buffer loan pool carved out by linker section), and const generics
or runtime dimensions would ripple through the page plan and UI for no user benefit —
nobody hot-swaps firmware between devices; the SD-update path can ship per-device images.

So: a `device-x4` / `device-x3` cargo feature pair (X4 default, features mutually
exclusive, enforced by `compile_error!`), selecting constants and driver modules at
compile time. Release tooling produces two image sets. The cache geometry tag (Phase 5)
protects against flashing the wrong image or moving an SD card between devices.

## Phase 1 — Board constants behind features (buildable without hardware)

1. `display/Cargo.toml` and `fw/Cargo.toml`: add `device-x4` (default) and `device-x3`
   features; `fw`'s features forward to `display`'s. Add the mutual-exclusion
   `compile_error!` in `display/src/lib.rs`.
2. `display/src/lib.rs`: make the panel constants cfg-selected:
   - X4: `WIDTH=800, HEIGHT=480` → `ROW_BYTES=100`, `FB_BYTES=48_000`
   - X3: `WIDTH=792, HEIGHT=528` → `ROW_BYTES=99`, `FB_BYTES=52_272`
   - Keep `BAND_ROWS=80`. 528 is not a multiple of 80; `fill_transformed_band` already
     handles the short last band (`rows = BAND_ROWS.min(HEIGHT - band_y)`), so only the
     `const _: () = assert!(...)` lines need per-device values. `BAND_BYTES` becomes
     7 920 on X3 — still under the `dma_buffers!(8000)` allocation in `fw`, but make that
     literal reference `display::BAND_BYTES` (or assert `BAND_BYTES <= 8000`) instead of
     trusting the coincidence.
3. Fix the one literal: `HEADING_CX: i16 = 480` in `ui/src/render.rs:32` →
   derive from `display::HEIGHT` (it's the vertical center of the portrait-oriented
   heading axis; confirm intent from surrounding code rather than assuming).
4. Gate: `cargo build` for both feature sets, `cargo clippy` clean (the repo treats
   clippy as a gate), and the existing host-side tests pass under `device-x4`.

Memory note: the framebuffers grow +4.3 KB each on X3. `fw` keeps two (`fb`, `prev_fb`),
so budget ~+8.6 KB static RAM. Check the linker map / heap watermark after Phase 2;
the sync-mode loan pool boundaries in `sync_mem.rs` may need a corresponding trim.

## Phase 2 — Panel driver seam (buildable without hardware)

Today `display/src/epd.rs` (227 lines) is pure SSD1677: command constants, `RefreshMode`,
`INIT_SEQUENCE`, RAM-window math, `update_control_1/2`, and the framebuffer→panel band
transform. `fw/src/display_flush.rs` (158 lines) drives it: `init_panel`, `flush`
(BW RAM + RED RAM previous-frame differential + activation), `prestage_red`,
`sleep_panel`. `fw/src/tasks/display.rs` consumes only `RefreshMode` and those four
functions — that is the seam, and it's already narrow.

Restructure:

1. Split `display/src/epd.rs` into `display/src/epd/mod.rs` (shared: `RefreshMode`,
   `SpiOp`, `Rect` helpers, `fill_transformed_band`, `REVERSE_BITS_LUT`) plus
   `epd/ssd1677.rs` (everything else, moved verbatim) and a new `epd/uc8253.rs`.
   `mod.rs` does a cfg-gated `pub use` of the active panel module so `fw` imports stay
   `display::epd::…`. Keep `RefreshMode { Full, Fast, FastClean, PowerDown }` shared —
   it is the contract with `app_core::RefreshPlanner` and the web emulator; do not fork it.
2. Split `fw/src/display_flush.rs` the same way: the four `pub(crate)` entry points keep
   their signatures; per-panel bodies live in cfg-gated submodules.
3. Write the UC8253 backend by porting `Uc8253X3Driver.cpp`. The controller model is
   genuinely different from SSD1677 — don't translate line by line, map concepts:
   - **RAM planes:** UC8253 has DTM1 (old frame) / DTM2 (new frame) instead of BW/RED RAM.
     Our existing previous-frame differential (`prev_fb` → RED RAM) and the
     `prestage_red` optimization map directly onto DTM1: prestage the just-shown frame
     into DTM1 after a refresh settles, stream only DTM2 on a fast turn.
   - **Waveforms are uploaded, not OTP:** each mode is a LUT bank (VCOM + WW/BW/WB/BB,
     42 bytes each) written to the controller, plus the CDI register (cmd 0x50)
     selecting differential (0x29) vs absolute (0xA9) mode. Port the bank tables from
     `Uc8253X3Luts.h` as `static` byte arrays (they're data, a few hundred lines; put
     them in `epd/uc8253_luts.rs` and exempt it from rustfmt churn the same way the
     generated font files are handled).
   - **Mode mapping:** `Fast` → the `fast` turbo bank (CDI 0x29, differential);
     `Full` → the `full` OEM bank from a white-DTM1 baseline plus the post-full settle
     pass CrossPoint does; `FastClean` → the `half` scrub bank (WW==BW, WB==BB — drives
     to target ignoring DTM1), which is the X3's natural "one-flicker clean". The
     SSD1677 90 °C temperature-override trick does not exist here; delete that branch
     in the X3 path rather than emulating it. `PowerDown` → the UC8253 power-off/deep-
     sleep command sequence from the reference driver.
   - **Skip grayscale:** CrossPoint's 4-level `gc` bank and strip-grayscale plumbing
     support features we don't have. Leave it out; note it as a follow-on.
   - **BUSY polarity:** the X3 panel uses what CrossPoint calls `X3TwoPhase` BUSY —
     `wait_ready` in `hal_ext::spi_dma::EpdBus` currently assumes the SSD1677's single
     busy-high phase. Read CrossPoint's busy-wait implementation for `X3TwoPhase` and
     add a cfg-gated (or parameterized) wait strategy to `EpdBus`. Getting this wrong
     is the classic "first refresh hangs forever / returns instantly" failure.
   - **SPI clock:** driver default is 16 MHz on X3 (papyrix runs 10 MHz and reports
     pixel corruption at 20 MHz). Our X4 bus speed constant lives in the fw SPI setup —
     make it a board constant; start X3 at 10 MHz, raise on the bench later.
   - **Orientation:** `MIRROR_X=true / MIRROR_Y=false / REVERSE_BITS=true` are GDEQ0426T82
     truths. The X3 values must come from the reference driver's addressing setup —
     make all three per-panel constants and expect to fix them on first boot (mirrored
     or bit-reversed text is the symptom).
4. Gate: both feature builds compile; X4 build is byte-identical in behavior (pure
   refactor — flash and spot-check a page turn before proceeding, per the
   commit-at-each-milestone convention).

## Phase 3 — Battery and bring-up (buildable; validation needs hardware)

1. New `hal-ext/src/bq27220.rs`: minimal async I2C driver — SOC from register 0x2C,
   voltage from 0x08, charging inferred from the signed current register (see papyrix's
   X3 doc for register semantics). I2C0 on SDA=GPIO20/SCL=GPIO0 @400 kHz.
2. `fw/src/main.rs` bring-up (~lines 150–200): cfg-gate the GPIO0/1/2 ADC block.
   On X3, GPIO1/2 stay ADC (buttons), GPIO0/20 become I2C, and the battery task reads
   the gauge instead of `battery_percent(sample.aux)` in `fw/src/tasks/input.rs`.
   Restructure so `input.rs` owns buttons only and battery becomes a small separate
   source feeding the same app message — that keeps the X4 path equivalent and the
   diff reviewable.
3. USB/charging detect: X4 senses GPIO20; X3 gets it from the gauge. Route both through
   one `power` message so `tasks/power.rs` and the UI stay device-agnostic.
4. Deep sleep: wake pin (GPIO3) is unchanged. Verify the I2C pins are parked safely
   before `esp_deep_sleep` (floating SDA/SCL against a powered gauge can leak µA —
   check what CrossPoint does before sleep).
5. IMU and RTC: out of scope. Don't probe them; don't add features for them.

## Phase 4 — Cache geometry tag

Cached page plans are geometry-dependent: a `BOOK.BIN`/section file paginated for
800×480 renders garbage at 792×528. Fold the panel geometry into the existing cache
versioning in `proto` (the artifact version byte has been bumped several times — same
mechanism): either a device byte in the header or, simpler, mix `WIDTH`/`HEIGHT` into
the layout-version constant so a mismatched cache reads as stale and rebuilds. Same for
`CATALOG.BIN` only if it stores geometry-dependent fields (check; cover thumbnails are).
`STATE.BIN` page indices are also plan-relative — geometry mismatch should invalidate
saved positions per book, falling back to the synced/percent progress where available.

## Phase 5 — Tools, images, release plumbing

1. Web emulator + preview (`tools/web-emulator`, `tools/preview`): they link the
   `display` crate, so they inherit the cfg constants — add an x3 feature pass-through
   and (for the web emulator) an X3 canvas size. Panel timing/waveform behavior is
   outside the emulator's parity boundary already; nothing to emulate there.
2. Release images: the locked-device SD/OTA path (partition table, patched-image +
   otadata trick, `proto::ota` validator, recovery combo) is **identical on the X3** —
   CrossPoint added their SD recovery combo specifically for USB-locked X3s. The only
   change is producing and naming a second image (`…-x3.bin`) and making the SD-update
   flow refuse a mismatched image: extend the `proto::ota` header/validator with a
   device tag so an X4 image can't be flashed onto an X3 from SD.
3. CI: build both feature sets so the X3 path can't rot.

## Phase 6 — On-device validation (needs the X3 + 4-pin pogo cable)

Order of first-boot debugging, cheapest signal first:
1. Serial alive over the pogo USB-Serial-JTAG (DTR-safe capture workflow applies as on X4).
2. Panel init completes: BUSY two-phase wait returns in sane time (not 0 ms, not ∞).
3. Full refresh shows the home screen — expect to iterate on MIRROR_X/Y/REVERSE_BITS here.
4. Fast page turn with DTM1 differential; then prestage-into-DTM1 off the critical path.
5. FastClean-equivalent (half-scrub bank) ghosting check after ~20 fast turns.
6. Battery percent sane vs charger on/off (gauge current sign).
7. SD catalog + reading end-to-end; sleep/wake; SD firmware update with the device-tag
   check (try flashing the X4 image — must be refused).
8. Re-derive timing baselines (X4 numbers — 421 ms fast, 1.5 s clean, 3.5 s full — do
   not transfer) and record them in `docs/` alongside the X4 ones.

## Estimate and risk

Roughly 800–1,200 new lines (UC8253 driver + LUT tables + BQ27220 + board config),
~200–300 lines refactored across ~a dozen files. Phases 1–5 are desk work; the schedule
risk is concentrated in Phase 2's waveform/BUSY behavior and Phase 6 bench time. Biggest
unknowns, in order: the X3TwoPhase BUSY handling in `EpdBus`, panel orientation flags,
whether the ported LUT banks behave at our SPI clock, and the (low) chance papyrix is
right that some X3 units carry an SSD1677 — if init fails inexplicably, test for that
before debugging the UC8253 path deeper.
