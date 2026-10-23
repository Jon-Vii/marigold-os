# Verification

How to check a change before trusting it, plus dated hardware measurements
that are not recorded elsewhere. Bench workflow details live in
`docs/agents/bench.md`.

## The gate

CI (`.github/workflows/ci.yml`) runs the commands below on every pull request
and push to main, and the release workflow runs it on the tagged commit. CI
calls plain `cargo`; `rust-toolchain.toml` pins the same compiler there. Run
them locally before committing changes they cover.

`tools/cargo.sh` runs Cargo from the toolchain pinned in `rust-toolchain.toml`.
The root `.cargo/config.toml` sets `build.target = "riscv32imc-unknown-none-elf"`,
so every host command needs an explicit `--target <host>`, for example
`aarch64-apple-darwin` on the maintainer's Mac or `x86_64-unknown-linux-gnu`
on CI.

Host:

```sh
tools/cargo.sh fmt --all --check
tools/cargo.sh fmt --all --check --manifest-path tools/emulator/Cargo.toml
tools/cargo.sh fmt --all --check --manifest-path tools/web-emulator/Cargo.toml
tools/cargo.sh fmt --all --check --manifest-path tools/preview/Cargo.toml
tools/cargo.sh test -p app-core -p proto -p ui -p display --target <host>
tools/cargo.sh test -p ui --target <host> --features display/device-x3
tools/cargo.sh test --manifest-path tools/emulator/Cargo.toml --target <host> --no-default-features
tools/cargo.sh test --manifest-path tools/emulator/Cargo.toml --target <host> --no-default-features --features device-x3
tools/cargo.sh run --manifest-path tools/emulator/Cargo.toml --target <host> --no-default-features -- --scenario fixtures/scenarios --check fixtures/golden
tools/cargo.sh run --manifest-path tools/emulator/Cargo.toml --target <host> --no-default-features --features device-x3 -- --scenario fixtures/scenarios --check fixtures/golden
tools/cargo.sh test --manifest-path tools/web-emulator/Cargo.toml --target <host>
python3 -m unittest discover -s tools/bench -p 'test_*.py'
tools/verify_web_flasher.sh web --safety-only
```

Firmware (`riscv32imc-unknown-none-elf`, release):

```sh
tools/cargo.sh clippy --workspace --release -- -D warnings
tools/cargo.sh clippy -p fw --release --features device-x3 -- -D warnings
tools/cargo.sh build -p fw --release
tools/cargo.sh build -p fw --release --features device-x3
```

Linking `fw` runs the RAM-layout assertions emitted by `fw/build.rs`: the
previous-frame buffer must match the display crate's framebuffer size, and the
main stack region must be at least 27 KB (`27 * 1024` bytes). The X3 build has
less margin above that floor than the X4 (as of late October 2026, 36,080 bytes
linked on X3 versus 44,656 on X4, because both X3 framebuffers are larger);
re-check the EPUB-open path with `-Zemit-stack-sizes` before deepening it.
About 5 KB of each comes from `ESP_HAL_CONFIG_PLACE_SWITCH_TABLES_IN_RAM =
"false"` in `.cargo/config.toml`, which keeps esp-hal's switch tables and
constant pools in flash; exporting the variable as `true` brings back the
old layout for comparison.

## Golden frames

Scenarios live in `fixtures/scenarios`; their frames live in `fixtures/golden`
as `<scenario>.png` (X4) and `<scenario>-x3.png` (X3, built with
`--features device-x3`). Frames compare by decoded pixels. On a mismatch the
check writes `<name>.actual.png` and `<name>.diff.png` under
`target/golden-diff/` and reports the differing pixel count and bounding box;
the `.actual.png` is in golden format, so it can be copied over the golden to
accept an intentional change.

After an intentional rendering change, regenerate the scenario frames by
dumping into the golden directory (once per panel), then review the diff:

```sh
tools/cargo.sh run --manifest-path tools/emulator/Cargo.toml --target <host> --no-default-features -- --scenario fixtures/scenarios --dump fixtures/golden
tools/cargo.sh run --manifest-path tools/emulator/Cargo.toml --target <host> --no-default-features --features device-x3 -- --scenario fixtures/scenarios --dump fixtures/golden
```

The reading-surface goldens in `tools/emulator/tests/reading_golden.rs`
regenerate with `REGEN_READING_GOLDEN=1` (exactly `1`; other values are
ignored):

```sh
REGEN_READING_GOLDEN=1 tools/cargo.sh test --manifest-path tools/emulator/Cargo.toml --target <host> --no-default-features --test reading_golden
REGEN_READING_GOLDEN=1 tools/cargo.sh test --manifest-path tools/emulator/Cargo.toml --target <host> --no-default-features --features device-x3 --test reading_golden
```

## Development commands

Not part of the gate, but useful while iterating:

```sh
# Dump every scenario's final frame for inspection
tools/cargo.sh run --manifest-path tools/emulator/Cargo.toml --target <host> --no-default-features -- --scenario fixtures/scenarios --dump target/emulator
# Interactive desktop emulator
tools/cargo.sh run --manifest-path tools/emulator/Cargo.toml --target <host> --features gui -- --gui
# Static shell previews into target/previews (pass an EPUB path to preview a book)
tools/cargo.sh run --manifest-path tools/preview/Cargo.toml --target <host>
# Host model of the app/display/storage channel rules; no hardware
tools/bench/bench.py channel-stress --host
```

## Hardware confidence checks

After relevant firmware changes, on a flashed device (see
`docs/agents/bench.md` for when to run which suite):

```sh
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --turns 50
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --reset-before --seconds 20 --strict
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 5
tools/bench/bench.py report target/bench/latest.jsonl
```

For button calibration, raw ADC serial logging is behind `RAW_LOG_ENABLED` in
`fw/src/tasks/input.rs`.

## Hardware measurements (historical)

Point-in-time X4 measurements. Firmware has changed since; re-measure before
relying on a number. Later results are in
`docs/plans/2026-09-12-reader-performance.md` and `docs/releases/`. Until
October 15 2026 the bench X4 ran its flash at espflash's default 40 MHz; it
now runs at 80 MHz (see `docs/FLASHING.md`), which makes CPU-bound work such
as layout and book builds markedly faster than these numbers.

### June 10 2026: panel and page-turn baseline

On-device, room temperature, commit `14c7eba` plus the page-plan seam,
Unsong.epub (1,001 pages / 78 chapters), from the `bench:` serial lines via
`tools/serial_capture.py`:

- Fast-waveform refresh BUSY: 421 ms, constant across 44 refreshes.
- Full-waveform refresh BUSY: 3.53–3.61 s. It then dominated every view
  change, boot (~4.1 s to Home), and wake (~3.8 s to Home); software around it
  was noise, so improvements there are waveform/temperature configuration, not
  SPI or layout work. (Wake and view changes have since moved to FastClean.)
- Whole-frame RAM stream over band DMA: 22–24 ms per plane (BW or RED).
- Layout + framebuffer draw: Reading 20–36 ms, menu views 82–90 ms.
- EPUB page turn, button press to panel settled: ~470 ms median (~89% of it
  the fast BUSY wait); sustained held-button rate about 2 pages/s. RED
  prestaging held on every fast turn, and SD-backed section extends (~50–85 ms
  v2 cache hits) never stalled a render. Input polling and debounce at the
  time added ~40–80 ms ahead of the logged press.
- Book open with a v2 cache hit: cache ready in 50–85 ms; the perceived ~4 s
  was the full refresh that painted the first page.

### June 11 2026: wireless session

On the esp-hal 0.23 / esp-wifi 0.12 stack, since replaced by esp-hal 1.1.1,
esp-radio 1.0.0-beta, and esp-rtos 0.3.0:

- Station join took ~21 s from Start to a DHCP address; the join timeout in
  `fw/src/tasks/wifi.rs` has since been raised to 35 s.
- Status repaints rode the 438 ms fast refresh, USB serial survived radio
  init, Back reset the device, and the saved position came back intact
  after reboot.
- AP onboarding: the hotspot came up, a phone leased an address, and submitted
  credentials reached the card. The portal URL was typed manually that run;
  the DNS responder's silence on AAAA/HTTPS queries, which stalled captive
  detection, was fixed afterward.
- Browser upload: a 2.4 MB EPUB traveled browser → Wi-Fi → loaned buffers →
  `/BOOKS` and appeared on the shelf after the rescan.

### July 9 2026: cold cache build

117-page / 35-chapter EPUB, same book and restored position both runs, from
`bench: storage_build` via `tools/bench/bench.py storage-cache`:

- Before the build-path work: 7.9 s total (4.6 s writing section files; 3,903
  write + 1,267 read block transactions).
- After write staging, a held `SECTIONS` directory, dirty-gated pagination
  rebuilds, and the 8 KB read clamp: 3.9 s total (0.86 s writes; 537 write +
  723 read block transactions), about 34 ms per page and roughly 2× faster.
  About 70% of the remainder was CPU (inflate + parse + measure).
