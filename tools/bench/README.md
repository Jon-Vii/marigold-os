# MarigoldOS bench harness

`tools/bench/bench.py` captures hardware serial logs, parses structured
`bench:` telemetry, and summarizes development bench runs. It does not control
the reader over serial; current hardware suites are guided workflows.

## Common runs

```sh
tools/bench/bench.py channel-stress --host
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --turns 50
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --reset-before --seconds 20
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 10
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --reset-before --seconds 20 --strict
tools/bench/bench.py report target/bench/latest.jsonl
tools/bench/bench.py report --strict target/bench/latest.jsonl
```

Longer release/risky-merge runs:

```sh
tools/bench/bench.py reader-soak --port /dev/cu.usbmodem101 --minutes 30
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --cold --warm
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 20
```

## Output

Raw serial remains visible in the terminal. Parsed records are appended as
JSONL under `target/bench/` by default:

```json
{"event":"render","flush_ms":421,"layout_ms":24,"mode":"Fast","page":42,"suite":"page-turn"}
```

Do not commit run logs. Keep only parser code, suite docs, and stable budgets in
the repo.

`--reset-before` uses `espflash reset` before opening the raw serial capture.
This is useful for boot, catalog-cache, and sleep/wake smoke runs because it
does not rely on catching a manual button press at the right moment.

`report --strict` exits non-zero when checked-in warning budgets are exceeded.
Capture commands also accept `--strict`, applying the same budget gate to the
log they just wrote.

## When to use

- Use `channel-stress --host` during normal development when touching reader
  state, display command, storage command, sync session, refresh plan, or
  queue/coalescing behavior.
- Use short `page-turn` and `sleep-sync` runs before trusting a flashed firmware
  after display, input, sleep, reader rendering, SD session, section cache, or
  progress-write changes.
- Use `reader-soak`, `storage-cache`, and longer `sleep-sync` runs before
  releases or risky merges.
- Use `thermal-run` for targeted refresh, ghosting, sleep-screen, enclosure,
  power, SD-card, or ambient-temperature investigations.
