# Bench program plan

Status: Draft

## Problem

The repo has good host-side behavior checks: reducer tests, parser tests,
preview renders, and emulator scenarios with golden frames. Those catch
deterministic reader state, framebuffer, and SSD1677 protocol regressions before
flashing.

They do not answer the hardware questions that still dominate the X4 reader:

- whether Embassy channel pressure drops or reorders user-visible work;
- whether long reading sessions keep the reader store, section cache, and
  progress writes healthy;
- whether page-turn latency stays inside the current budget on real SD cards,
  at real panel temperatures;
- whether SD session reuse, catalog snapshots, and cache artifacts stay fast
  across many open/extend/progress cycles;
- whether display sleep, panel deep sleep, and ESP32-C3 deep sleep hand off
  reliably after fast-refresh history.

The missing tool is a development bench harness that can run repeatable
on-device workflows and turn serial telemetry into pass/fail numbers.

## Recommendation

Add a first-class host CLI named `bench`, backed by a tiny `tools/bench` Rust or
Python package, plus opt-in firmware bench instrumentation behind a feature such
as `bench-mode`.

The initial `bench` should not require an interactive serial shell on the
device. The firmware does not currently have one, and adding a UART command
plane would be a larger architecture decision. Instead, the host CLI should:

1. build or flash a normal or bench-mode firmware image;
2. capture serial output using the same DTR/RTS behavior as
   `tools/serial_capture.py`;
3. parse structured `bench:` lines and important existing markers;
4. optionally tell the human exactly which button workflow to perform;
5. emit a JSONL run log plus a concise terminal summary with budgets.

That gives the repo useful benching without smuggling a second control channel
into the board I/O task.

## Command shape

```sh
bench list
bench page-turn --port /dev/cu.usbmodem101 --book 0 --turns 100
bench reader-soak --port /dev/cu.usbmodem101 --minutes 30
bench storage-cache --port /dev/cu.usbmodem101 --book 0 --cold --warm
bench sleep-sync --port /dev/cu.usbmodem101 --cycles 20
bench channel-stress --host
bench report target/bench/*.jsonl
```

`bench channel-stress --host` should run without hardware. The rest should be
hardware benches that can start as guided workflows and later become automated
if the project adds GPIO poking, a fixture board, or an explicit debug command
transport.

## Bench suites

### `channel-stress`

Goal: prove the app/display/storage/power message contracts keep the intended
coalescing semantics under pressure.

Start on the host, not the board. Build a small simulator around the pure
`ReaderState` reducer, `StorageCommand` transition mapping, `RefreshPlanner`,
and simplified channel capacities matching firmware:

- input events can arrive while a render is in flight;
- at most one render is in flight;
- many button events collapse to one pending render of the latest reader state;
- stale open/extend requests are skipped by request id;
- progress writes are coalesced;
- sync-session admission refuses scratch-using storage commands after the loan.

Useful checks:

- no stale render settles after a newer reader state is known;
- `LATEST_READER_REQUEST_ID`-style stale storage is observable in the model;
- `DisplayCommand::Sleep` clears render/pending state;
- a sync session never admits `OpenBook`, `ExtendSection`, `LoadChapters`, or
  `JumpChapter` after `LoanSyncMemory`.

This fills a gap between unit tests and emulator scenarios: randomized or
exhaustive interleavings around the concurrency rules.

### `page-turn`

Goal: track real reading latency and refresh behavior.

Input: a warmed SD book, target turn count, optional refresh policy.

Telemetry to parse or add:

- existing `bench: render ... layout=... flush=... prestage=... t=...`;
- input press marker with button and timestamp;
- storage open/extend hit/miss marker;
- display refresh mode and BUSY duration;
- optional battery millivolts at start/end.

Budgets to start with, based on `docs/IMPLEMENTATION_PLAN.md` measurements:

- median press-to-settled page turn: target under 550 ms plus debounce;
- fast refresh BUSY: warn above 500 ms;
- layout draw: warn above 60 ms for Reading;
- prestage: warn above 40 ms;
- no unexpected full refresh after the first settled reading frame unless the
  selected refresh policy requires it.

### `reader-soak`

Goal: find workflow reliability bugs that only appear after many state changes.

Workflow:

- open an SD EPUB;
- turn forward and backward through section-cache boundaries;
- visit Chapters and jump;
- visit Home/Library and return;
- periodically sleep and wake manually or by idle timeout;
- keep collecting serial logs for 30-120 minutes.

Pass criteria:

- no panic, watchdog, queue-full storm, or unexpected reset;
- page count and current chapter remain plausible;
- storage extends complete when requested;
- progress writes are coalesced but flushed before display sleep;
- after a reset/wake, restored position matches the most recent flushed state
  within the accepted coalescing window.

This is the bench I would expect to catch the most valuable bugs.

### `storage-cache`

Goal: measure SD session and reader cache behavior directly.

Scenarios:

- cold card with no `/XTEINK/CATALOG.BIN`;
- warm card with catalog snapshot;
- cold book cache miss;
- warm book cache hit;
- section extend near a boundary;
- progress write burst;
- optional upload-created `.epub` long-name path if wireless serving is in scope.

Telemetry to add:

- `bench: storage scan ...`;
- `bench: storage catalog load/write ...`;
- `bench: storage open request=... cache=hit|miss ram=hit|miss elapsed=...`;
- `bench: storage section extend ...`;
- `bench: storage progress write elapsed=...`.

Budgets should be warning thresholds at first, because SD cards vary. The
important regression signal is large movement from this repo's own baseline.

### `sleep-sync`

Goal: validate power/display handoff, especially after fast-refresh history.

Workflow:

- boot to restored Home or Reading;
- perform N fast page turns;
- press Power or wait for idle sleep;
- verify sleep screen full refresh, SSD1677 deep sleep, and ESP32-C3 deep sleep
  markers;
- wake and repeat.

Pass criteria:

- every sleep path logs a successful sleep-frame refresh before panel deep
  sleep;
- no `display: sleep framebuffer flush failed`;
- full-refresh BUSY for the sleep screen stays in the expected range;
- wake performs exactly one deferred boot/restored render;
- no visible artifact notes are recorded for the run.

This directly continues `.scratch/sleep-screen-artifacts`.

### `thermal-run`

Goal: make the analog e-paper and board-temperature part explicit rather than
accidental.

This can be a thin wrapper over `page-turn` and `sleep-sync` at first:

```sh
bench thermal-run --suite page-turn --minutes 45
```

It should sample whatever the firmware can cheaply expose: panel refresh BUSY
time, temperature-register decisions if exposed, battery voltage, and elapsed
time. A manual `--note hot-room`, `--note freezer-pack-nearby`, or
`--note enclosure-closed` flag is enough for early runs.

## Telemetry format

Keep serial human-readable but regular:

```text
bench: render view=Reading mode=Fast page=42 ch=3 layout_ms=24 flush_ms=421 prestage_ms=23 t_ms=123456
bench: storage_open request=17 book_id=2 index=0 cache=hit ram=miss elapsed_ms=73 pages=1001
bench: sleep phase=refresh mode=Full busy_ms=3532 ok=1 t_ms=130000
bench: input button=Next aux=... nav=... page=... t_ms=122990
```

The current render line is close, but key-value fields will be easier to parse
than debug-formatted enums and mixed `layout=...ms` strings.

The host CLI should write parsed records as JSONL:

```json
{"suite":"page-turn","event":"render","view":"Reading","mode":"Fast","page":42,"layout_ms":24,"flush_ms":421}
```

## Implementation path

1. Create `tools/bench` with serial capture, parser, summary reporting, and
   `bench report`.
2. Normalize current `bench: render` output to key-value fields.
3. Add low-risk telemetry around input events, storage open/extend/cache paths,
   progress writes, and sleep phases.
4. Implement `page-turn`, `storage-cache`, and `sleep-sync` as guided hardware
   benches.
5. Add `channel-stress --host` as a deterministic or property-style model test
   over the app/display/storage concurrency rules.
6. Add `reader-soak` as a suite runner that combines guided steps, long capture,
   and failure-pattern detection.
7. Promote stable budgets into a checked-in `tools/bench/benches.toml` once the
   project has several known-good X4 runs and at least one X3 run.

## Repo integration

Add these files when implementing:

```text
tools/bench/
  Cargo.toml or pyproject-free script layout
  src/main.rs or bench.py
  benches.toml
  README.md
target/bench/
  ignored JSONL and summaries
```

Do not put raw run logs in git. Commit the parser, suite definitions, and
budgets. Keep especially interesting hardware findings in `.scratch/` issues or
dated docs notes.

## When to run

Use the bench suite in tiers:

- Normal development: run `bench channel-stress --host` when changing
  `app_task`, `DisplayCommand`, `StorageCommand`, `SyncSession`,
  `RefreshPlanner`, reducer behavior, or queue/coalescing logic. This should be
  fast enough for local pre-commit or CI because it needs no hardware.
- Hardware confidence before trusting a flashed build: run a short
  `page-turn` plus `sleep-sync` pass after changes to display flush, input
  debounce, sleep/power, reader rendering, SD sessions, section cache behavior,
  or progress writes.
- Big merge or release readiness: run `reader-soak`, `storage-cache`, and a
  longer `sleep-sync` pass on a real board. This is the closest thing to a
  "does this reader stay healthy in the world?" gate.
- Targeted hardware investigation: run `thermal-run` when changing refresh
  mode/waveform behavior, chasing ghosting or sleep-screen artifacts, or
  testing enclosure, power, SD-card, or ambient-temperature changes.

In short: host bench in ordinary development loops, short hardware bench before
trusting a flashed firmware, long hardware bench before releases or risky
merges.

## First milestone

The first useful milestone is deliberately small:

```sh
bench page-turn --port /dev/cu.usbmodem101 --turns 50
bench sleep-sync --port /dev/cu.usbmodem101 --cycles 10
bench report target/bench/latest.jsonl
```

It should parse existing render timings plus a few new key-value telemetry
lines, then report median/p95 page-turn timing, refresh-mode counts,
unexpected full refreshes, sleep failures, and queue/storage warnings.

That would already cover the project gap better than a broad bench menu with no
budgets.
