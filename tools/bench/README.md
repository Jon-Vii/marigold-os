# MarigoldOS bench harness

`tools/bench/bench.py` captures hardware serial logs, parses structured
`bench:` telemetry, and summarizes development bench runs. It answers
board-specific questions about timing, SD and cache behavior, sleep, and long
soaks. The emulator and golden frames stay the fast check for behavior (see
[`docs/DEVELOPMENT.md`](../../docs/DEVELOPMENT.md)).

`bench.py` itself only listens: the hardware suites are guided workflows where
you work the reader. To drive the reader from the host, use a `bench-serial`
build and `send.py` (see [Automated button sequences](#automated-button-sequences)).

`bench.py` needs Python 3.11+ to read the warning budgets in
`tools/bench/benches.toml`, since it uses `tomllib`. Run it directly as shown
below; its `uv run --script` shebang picks a suitable Python.
`python3 tools/bench/...` also works when `python3` is 3.11 or newer. On older
interpreters, any command that reads budgets exits with an error instead of
skipping them. `send.py` and `check_forward_turns.py` need only the standard
library.

The examples use `/dev/cu.usbmodem101`, which is also the default `--port`.
Pass your own board's port if it differs.

## When to run what

Run `channel-stress --host` during normal development when you change reader
state, display command, storage command, sync session, refresh plan, or
queue/coalescing behavior. It needs no hardware:

```sh
tools/bench/bench.py channel-stress --host
```

Run the short hardware checks before trusting a flashed firmware after display
flush, input debounce, sleep/power, reader rendering, SD session, section
cache, or progress-write changes:

```sh
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --turns 50
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 5
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --reset-before --seconds 20 --strict
```

Run the longer checks before releases or risky merges:

```sh
tools/bench/bench.py reader-soak --port /dev/cu.usbmodem101 --minutes 30
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --cold --warm
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 20
```

Save `thermal-run` for targeted refresh, ghosting, sleep-screen, enclosure,
power, SD-card, or ambient-temperature investigations.

Other runs that come up often:

```sh
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --reset-before --seconds 20
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 10
tools/bench/bench.py report target/bench/latest.jsonl
tools/bench/bench.py report --strict target/bench/latest.jsonl
```

`--reset-before` uses `espflash reset` before opening the raw serial capture.
That helps with boot, catalog-cache, and sleep/wake smoke runs, because you
don't have to catch a manual button press at the right moment.

`report --strict` exits non-zero when checked-in warning budgets are exceeded or
when the selected suite did not capture its expected signal, such as storage
telemetry for `storage-cache` or input-to-Reading-render timing for
`page-turn`. Capture commands also accept `--strict`, applying the same gate to
the log they just wrote.

## Output and logs

Raw serial stays visible in the terminal. Parsed records are appended as JSONL
under `target/bench/` by default:

```json
{"event":"render","flush_ms":421,"layout_ms":24,"mode":"Fast","page":42,"suite":"page-turn"}
```

Don't commit run logs. Only parser code, suite docs, and stable budgets belong
in the repo. Summarize a log with:

```sh
tools/bench/bench.py report target/bench/latest.jsonl
```

Notable hardware findings worth keeping go in the dated measurements section of
[`docs/DEVELOPMENT.md`](../../docs/DEVELOPMENT.md#hardware-measurements-historical).

### Image telemetry

Books with images add two lines. `bench: image_decode` reports each image
decode: its index slot, the ZIP entry's size (`src_bytes`), the fitted size
(`out`), the whole decode and, for a deflated image, its spool to the card
(`ms`, `spool_ms`), and how it ended (`decoded`, `cancelled`, `failed`, or
`retry`).

`bench: image_session` reports each pass. `pass=window` and `pass=remaining`
are idle work (the pages around the one on screen, then the rest of the book).
`pass=request` is a page turn that checked, and perhaps decoded, the images on
its page before answering. A turn to a page whose images the store has noted as
settled checks nothing and logs no line, and once a `remaining` pass finds
nothing left, idle steps open no image session at all.

`report` lists decode outcomes and times, spool times, page turns that waited
for a decode (`image wait`), and page turns that only checked (`image check`).
To time page turns over pages whose images are already decoded, leave the book
idle on its first page until its `image_session` lines stop.

### Staged page telemetry

The next reading page is drawn and written to the panel's new-frame plane
while the reader reads (see "Staging the next page" in
[`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md)). Each `bench: render`
line says whether its page was staged (`staged=true stage=hit`) or why not
(`stage=none`, `other_page`, `store_changed`). A staged page's `layout_ms` is
only the check that it still holds, and its `flush_ms` has no BW write.
`bench: stage` reports each staging: `step=bw` with the draw, one store
fingerprint (`digest_us`) and the BW write when it got that far, `step=render`
when a press stopped the draw, and `step=check` when the page couldn't be
staged. Bench-serial builds draw every staged page again once it is shown and
log `bench: stage_check match=...`.

`report` counts staged pages, splits page-turn times into staged and drawn
turns, and treats any `stage_check` that didn't match as a warning, which
`--strict` turns into a failure. `check_forward_turns.py` prints the same split
and fails on a mismatch.

## Automated button sequences

Development firmware built with `--features bench-serial` accepts USB serial
button injection through the normal input channel. The feature is off by
default and excluded from release artifacts. Commands are `o` (Confirm), `n`
(Next), `p` (Previous), `b` (Back), and `s` (Power). Injected input is marked
`source=serial` and uses the host-test battery sample, so don't treat its
displayed battery percentage as a power measurement.

```sh
python3 tools/bench/send.py npnp --port /dev/cu.usbmodem101 --interval 1.5 --seconds 10 --out target/bench/turns.jsonl
python3 tools/bench/send.py sssss --port /dev/cu.usbmodem101 --interval 14 --seconds 80 --out target/bench/sleep-cycles.jsonl
```

`send.py` waits for each queued acknowledgement and the requested interval
before sending the next byte. The interval defaults to 1.5 seconds; `--seconds`
is the total capture duration. A USB reconnect never resends a byte whose
acknowledgement was lost. The command exits unsuccessfully if any item in the
sequence lacks a queued acknowledgement. An acknowledgement only proves the
input was queued, so check the render, storage and sleep telemetry to confirm
what happened next.

For page-turn regressions, start at page zero of a prepared book and send 50
consecutive `n` commands with `send.py` (one-second interval, 55-second capture).
Check the capture with `python3 tools/bench/check_forward_turns.py <capture>`.
It requires one settled render per destination page, no EPUB preparation, and
at most one second from each input to its settled page. Use `--first-page` for
another starting position and `--max-latency-ms` for a different panel budget.
Alternating `np` inside one RAM section does not exercise section boundaries.

The same feature adds a two-second timer wake alongside the Power wake source,
so deep-sleep cycles can run unattended. It also runs an isolated custom-font
cache/read benchmark at boot using a temporary root-level `FNTBENCH.BIN`.
Creation refuses an existing file; successful runs verify and remove their
fixture. It doesn't change the selected font or reading settings. Inspect the
`font_fixture`, `font_selftest`, and `serial_sleep` telemetry before
interpreting a run, and repeat ordinary boot/sleep checks with the feature
disabled.

## Screen captures

`bench-serial` builds also take `f`: the display task prints the framebuffer
after its next render. `tools/bench/fb_dump.py` sends `f` plus navigation keys,
collects the rows, and writes a PNG turned to the reading orientation:

```sh
python3 tools/bench/fb_dump.py --port /dev/cu.usbmodem101 --keys n --frame landscape --out target/bench/page.png
```

A capture shows exactly what reached the panel's buffer (image placement,
dithering, clipping) without anyone looking at the device. It can't show panel
effects such as ghosting.
