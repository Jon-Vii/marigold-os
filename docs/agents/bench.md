# Bench workflow

Use `tools/bench/bench.py` for hardware-facing development benches. The
emulator and golden frames remain the fast behavior oracle; bench runs answer
board-specific timing, SD/cache, sleep, and soak questions.

## When to run

- Run `tools/bench/bench.py channel-stress --host` during normal development
  when changing reader state, display command, storage command, sync session,
  refresh plan, or queue/coalescing behavior. This needs no hardware.
- Run short hardware confidence checks before trusting a flashed firmware after
  display flush, input debounce, sleep/power, reader rendering, SD session,
  section cache, or progress-write changes:

```sh
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --turns 50
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 5
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --reset-before --seconds 20 --strict
```

- Run longer hardware checks before releases or risky merges:

```sh
tools/bench/bench.py reader-soak --port /dev/cu.usbmodem101 --minutes 30
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --cold --warm
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 20
```

- Run `thermal-run` only for targeted refresh, ghosting, sleep-screen,
  enclosure, power, SD-card, or ambient-temperature investigations.

## Automated button sequences

Development firmware built with `--features bench-serial` accepts USB serial
button injection through the normal input channel. The feature is off by
default and excluded from release artifacts. Commands are `o` (Confirm), `n`
(Next), `p` (Previous), `b` (Back), and `s` (Power). The injected input is marked
`source=serial` and uses the host-test battery sample, so do not use its displayed
battery percentage as a power measurement.

```sh
python3 tools/bench/send.py npnp --port /dev/cu.usbmodem101 --interval 1.5 --seconds 10 --out target/bench/turns.jsonl
python3 tools/bench/send.py sssss --port /dev/cu.usbmodem101 --interval 14 --seconds 80 --out target/bench/sleep-cycles.jsonl
```

Sequences wait for each queued acknowledgement and the requested interval
before sending the next byte. The interval defaults to 1.5 seconds; `--seconds`
is the total capture duration. USB reconnection never resends a byte whose
acknowledgement was lost. The command exits unsuccessfully if any sequence
item lacks a queued acknowledgement. Acknowledgement proves input was queued;
use the render/storage/sleep telemetry to verify the resulting behavior.

The same feature adds a two-second timer wake alongside the Power wake source,
allowing unattended deep-sleep cycles. It also runs an isolated custom-font
cache/read benchmark at boot using a temporary root-level `FNTBENCH.BIN`.
Creation refuses an existing file; successful runs verify and remove their
fixture. It does not change the selected font or reading settings. Inspect
`font_fixture`, `font_selftest`, and `serial_sleep` telemetry before interpreting
a run, and repeat ordinary boot/sleep checks with the feature disabled.

## Logs

Raw bench logs are written under `target/bench/` by default and should not be
committed. Summarize them with:

```sh
tools/bench/bench.py report target/bench/latest.jsonl
```

Keep notable hardware findings in `.scratch/` issues or dated docs notes.
