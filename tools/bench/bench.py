#!/usr/bin/env python3
"""Development bench harness for MarigoldOS hardware runs.

The harness deliberately starts as a serial log collector and parser. The
firmware has no interactive command channel, so hardware suites are guided:
the host tells the operator what workflow to perform, captures structured
`bench:` lines, and writes JSONL for repeatable reporting.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import select
import signal
import statistics
import struct
import subprocess
import sys
import termios
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable

try:
    import tomllib
except ImportError:  # pragma: no cover - Python < 3.11 fallback.
    tomllib = None  # type: ignore[assignment]

try:
    import fcntl
except ImportError:  # pragma: no cover - non-POSIX hosts cannot capture serial.
    fcntl = None  # type: ignore[assignment]


DEFAULT_PORT = "/dev/cu.usbmodem101"
DEFAULT_OUT = Path("target/bench/latest.jsonl")
DEFAULT_BUDGETS = Path(__file__).with_name("benches.toml")

LEGACY_RENDER_RE = re.compile(
    r"bench: render (?P<view>\w+) (?P<mode>\w+) page=(?P<page>\d+) "
    r"ch=(?P<ch>\d+) layout=(?P<layout>\d+)ms flush=(?P<flush>\d+)ms "
    r"prestage=(?P<prestage>\d+)ms t=(?P<t>\d+)"
)
LEGACY_INPUT_RE = re.compile(
    r"input: (?P<button>Some\((?P<some>\w+)\)|None) gpio0=(?P<aux>\d+) "
    r"gpio1=(?P<nav>\d+) gpio2=(?P<page>\d+) t=(?P<t>\d+)"
)
REFRESH_BUSY_RE = re.compile(r"display: refresh busy (?P<busy>\d+) ms")
STORAGE_OPEN_RE = re.compile(
    r"storage: open complete status=(?P<status>\w+) pages=(?P<pages>\d+) "
    r"chapters=(?P<chapters>\d+)"
)


@dataclass(frozen=True)
class Suite:
    name: str
    guidance: str
    stop_event: str | None = None
    stop_count_arg: str | None = None


SUITES = {
    "page-turn": Suite(
        "page-turn",
        "Open a warmed SD book, then press Next for the requested turn count.",
        stop_event="reading_render",
        stop_count_arg="turns",
    ),
    "reader-soak": Suite(
        "reader-soak",
        "Run a normal reading workflow: page turns, chapter jumps, Home/Library returns, and a sleep/wake cycle.",
    ),
    "storage-cache": Suite(
        "storage-cache",
        "Exercise cold/warm catalog, book open, section extend, and progress-write paths.",
    ),
    "sleep-sync": Suite(
        "sleep-sync",
        "After several fast page turns, press Power or wait for idle sleep, then wake and repeat.",
        stop_event="sleep_complete",
        stop_count_arg="cycles",
    ),
    "thermal-run": Suite(
        "thermal-run",
        "Run the named underlying workflow while recording temperature/ambient notes in the run metadata.",
    ),
}


def main() -> int:
    parser = argparse.ArgumentParser(prog="bench")
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("list", help="list available suites")

    for name in SUITES:
        add_capture_parser(sub, name)

    stress = sub.add_parser("channel-stress", help="run host concurrency checks")
    stress.add_argument("--host", action="store_true", help="required; no hardware is used")
    stress.set_defaults(func=run_channel_stress)

    report = sub.add_parser("report", help="summarize one or more bench JSONL logs")
    report.add_argument("paths", nargs="+", type=Path)
    report.add_argument("--budgets", type=Path, default=DEFAULT_BUDGETS)
    report.add_argument("--strict", action="store_true", help="exit non-zero on budget warnings")
    report.set_defaults(func=run_report)

    args = parser.parse_args()
    if args.command == "list":
        print_suites()
        return 0
    return args.func(args)


def add_capture_parser(sub: argparse._SubParsersAction[argparse.ArgumentParser], name: str) -> None:
    suite = SUITES[name]
    p = sub.add_parser(name, help=suite.guidance)
    p.add_argument("--port", default=DEFAULT_PORT)
    p.add_argument("--out", type=Path, default=DEFAULT_OUT)
    p.add_argument("--seconds", type=int, default=None, help="stop after this many seconds")
    p.add_argument(
        "--reset-before",
        action="store_true",
        help="hard-reset the ESP32-C3 with espflash before capture",
    )
    p.add_argument("--espflash", default="espflash", help="espflash executable")
    p.add_argument("--strict", action="store_true", help="exit non-zero on budget warnings")
    p.add_argument("--note", action="append", default=[], help="free-form note stored in metadata")
    p.add_argument("--book", default=None, help="operator label for the book under test")
    if name == "page-turn":
        p.add_argument("--turns", type=int, default=50)
    if name == "reader-soak":
        p.add_argument("--minutes", type=int, default=30)
    if name == "sleep-sync":
        p.add_argument("--cycles", type=int, default=10)
    if name == "storage-cache":
        p.add_argument("--cold", action="store_true")
        p.add_argument("--warm", action="store_true")
    if name == "thermal-run":
        p.add_argument("--suite", choices=["page-turn", "sleep-sync"], default="page-turn")
        p.add_argument("--minutes", type=int, default=45)
    p.set_defaults(func=run_capture)


def print_suites() -> None:
    for name, suite in sorted(SUITES.items()):
        print(f"{name:14} {suite.guidance}")
    print("channel-stress  Host-only interleaving checks for queue/coalescing rules.")
    print("report          Summarize captured JSONL logs.")


def run_capture(args: argparse.Namespace) -> int:
    suite = SUITES[args.command]
    seconds = capture_seconds(args)
    stop_target = stop_target_for(args, suite)
    args.out.parent.mkdir(parents=True, exist_ok=True)

    print(f"bench {suite.name}: {suite.guidance}")
    print(f"port: {args.port}")
    print(f"out:  {args.out}")
    if seconds:
        print(f"stop: after {seconds}s")
    elif stop_target:
        print(f"stop: after {stop_target[1]} parsed {stop_target[0]} events")
    else:
        print("stop: Ctrl-C")
    if args.note:
        print("notes:", "; ".join(args.note))
    if args.reset_before:
        print("reset: hard-reset before capture")

    metadata = {
        "suite": suite.name,
        "event": "run_start",
        "host_time": time.time(),
        "port": args.port,
        "notes": args.note,
        "book": getattr(args, "book", None),
        "reset_before": bool(args.reset_before),
    }
    counts: dict[str, int] = {}
    started = time.monotonic()
    stop_at = started + seconds if seconds else None
    stop = False

    with args.out.open("a", encoding="utf-8") as out:
        write_event(out, metadata)
        try:
            if args.reset_before:
                reset_device(args.espflash, args.port)
            for line in serial_lines(args.port, stop_at=stop_at):
                sys.stdout.write(line)
                sys.stdout.flush()
                for event in parse_line(line, suite.name):
                    write_event(out, event)
                    for counter in event_counters(event):
                        counts[counter] = counts.get(counter, 0) + 1
                if stop_target and counts.get(stop_target[0], 0) >= stop_target[1]:
                    stop = True
                if stop:
                    break
        except KeyboardInterrupt:
            print("\nbench: capture stopped")
        finally:
            write_event(
                out,
                {
                    "suite": suite.name,
                    "event": "run_end",
                    "host_time": time.time(),
                    "elapsed_s": round(time.monotonic() - started, 3),
                    "counts": counts,
                },
            )
    budget_warnings = summarize_paths([args.out], DEFAULT_BUDGETS)
    return 1 if args.strict and budget_warnings else 0


def reset_device(espflash: str, port: str) -> None:
    command = [
        espflash,
        "reset",
        "--chip",
        "esp32c3",
        "--port",
        port,
        "--non-interactive",
        "--after",
        "hard-reset",
    ]
    subprocess.run(command, check=True)


def event_counters(event: dict[str, Any]) -> list[str]:
    event_name = str(event.get("event", ""))
    counters = [event_name]
    if event_name == "render" and event.get("view") == "Reading":
        counters.append("reading_render")
    if event_name == "sleep" and event.get("phase") == "complete":
        counters.append("sleep_complete")
    if event_name == "input" and event.get("button") in {"Next", "Previous"}:
        counters.append("page_input")
    return counters


def capture_seconds(args: argparse.Namespace) -> int | None:
    if args.seconds is not None:
        return args.seconds
    if args.command in {"reader-soak", "thermal-run"}:
        return int(args.minutes) * 60
    return None


def stop_target_for(args: argparse.Namespace, suite: Suite) -> tuple[str, int] | None:
    if suite.stop_event is None or suite.stop_count_arg is None:
        return None
    return suite.stop_event, int(getattr(args, suite.stop_count_arg))


def serial_lines(port: str, stop_at: float | None = None) -> Iterable[str]:
    if fcntl is None:
        raise RuntimeError("serial capture requires POSIX fcntl support")
    fd = os.open(port, os.O_RDONLY | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        attrs = termios.tcgetattr(fd)
        attrs[0] = 0
        attrs[1] = 0
        attrs[2] = termios.CREAD | termios.CLOCAL | termios.CS8
        attrs[3] = 0
        termios.tcsetattr(fd, termios.TCSANOW, attrs)
        fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack("i", termios.TIOCM_DTR))
        fcntl.ioctl(fd, termios.TIOCMBIC, struct.pack("i", termios.TIOCM_RTS))
        buf = b""
        while True:
            timeout = 0.2
            if stop_at is not None:
                remaining = stop_at - time.monotonic()
                if remaining <= 0:
                    return
                timeout = min(timeout, remaining)
            ready, _, _ = select.select([fd], [], [], timeout)
            if not ready:
                continue
            chunk = os.read(fd, 4096)
            if not chunk:
                continue
            buf += chunk
            while b"\n" in buf:
                raw, buf = buf.split(b"\n", 1)
                yield raw.decode("utf-8", errors="replace") + "\n"
    finally:
        os.close(fd)


def parse_line(line: str, suite: str = "unknown") -> list[dict[str, Any]]:
    text = line.strip()

    match = LEGACY_RENDER_RE.match(text)
    if match:
        data = match.groupdict()
        return [
            {
                "suite": suite,
                "event": "render",
                "view": data["view"],
                "mode": data["mode"],
                "page": int(data["page"]),
                "chapter": int(data["ch"]),
                "layout_ms": int(data["layout"]),
                "flush_ms": int(data["flush"]),
                "prestage_ms": int(data["prestage"]),
                "t_ms": int(data["t"]),
                "legacy": True,
            }
        ]

    if text.startswith("bench: "):
        return [parse_bench_line(text, suite)]

    match = LEGACY_INPUT_RE.match(text)
    if match:
        button = match.group("some") or "None"
        return [
            {
                "suite": suite,
                "event": "input",
                "button": button,
                "aux": int(match.group("aux")),
                "nav": int(match.group("nav")),
                "page_raw": int(match.group("page")),
                "t_ms": int(match.group("t")),
                "legacy": True,
            }
        ]

    match = REFRESH_BUSY_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "refresh",
                "busy_ms": int(match.group("busy")),
                "legacy": True,
            }
        ]

    if text == "display: sleep deep":
        return [{"suite": suite, "event": "sleep", "phase": "deep", "legacy": True}]
    if text == "display: sleep framebuffer flush failed":
        return [{"suite": suite, "event": "sleep", "phase": "refresh", "ok": False}]
    if "queue full" in text or "panicked at" in text or "watchdog" in text.lower():
        return [{"suite": suite, "event": "warning", "line": text}]

    match = STORAGE_OPEN_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "storage_open",
                "status": match.group("status"),
                "pages": int(match.group("pages")),
                "chapters": int(match.group("chapters")),
                "legacy": True,
            }
        ]
    return []


def parse_bench_line(text: str, suite: str) -> dict[str, Any]:
    body = text.removeprefix("bench: ").strip()
    if not body:
        return {"suite": suite, "event": "unknown", "line": text}
    parts = body.split()
    event = parts[0].replace("-", "_")
    result: dict[str, Any] = {"suite": suite, "event": event}
    for part in parts[1:]:
        if "=" not in part:
            result.setdefault("tokens", []).append(part)
            continue
        key, raw = part.split("=", 1)
        result[key] = parse_value(raw)
    return result


def parse_value(raw: str) -> Any:
    value = raw.strip().rstrip(",")
    if value.startswith("Some(") and value.endswith(")"):
        return value[5:-1]
    if value.endswith("ms") and value[:-2].isdigit():
        return int(value[:-2])
    if value in {"true", "false"}:
        return value == "true"
    if value in {"ok", "fail"}:
        return value == "ok"
    if re.fullmatch(r"-?\d+", value):
        return int(value)
    try:
        return float(value)
    except ValueError:
        return value


def write_event(out: Any, event: dict[str, Any]) -> None:
    out.write(json.dumps(event, sort_keys=True, separators=(",", ":")) + "\n")
    out.flush()


def run_report(args: argparse.Namespace) -> int:
    budget_warnings = summarize_paths(args.paths, args.budgets)
    return 1 if args.strict and budget_warnings else 0


def summarize_paths(paths: list[Path], budgets_path: Path | None = None) -> list[str]:
    events: list[dict[str, Any]] = []
    for path in paths:
        events.extend(read_events(path))
    if not events:
        print("bench report: no events")
        return []

    renders = [event for event in events if event.get("event") == "render"]
    reading_renders = [event for event in renders if event.get("view") == "Reading"]
    refreshes = [event for event in events if event.get("event") == "refresh"]
    sleeps = [event for event in events if event.get("event") == "sleep"]
    warnings = [event for event in events if event.get("event") == "warning"]
    storage = [event for event in events if str(event.get("event", "")).startswith("storage")]

    print("\nbench report")
    print(f"events:        {len(events)}")
    print(f"renders:       {len(renders)}")
    print(f"storage:       {len(storage)}")
    print(f"sleeps:        {len(sleeps)}")
    print(f"warnings:      {len(warnings)}")
    print_duration("reading layout", values(reading_renders, "layout_ms"))
    print_duration("page turn", page_turn_durations(events))
    print_duration("render flush", values(renders, "flush_ms"))
    print_duration("prestage", values(renders, "prestage_ms"))
    print_duration("refresh busy", values(refreshes, "busy_ms"))
    print_duration(
        "storage open",
        values([event for event in events if event.get("event") == "storage_open"], "elapsed_ms"),
    )
    print_duration(
        "catalog load",
        values(
            [
                event
                for event in events
                if event.get("event") == "storage_catalog" and event.get("action") == "load"
            ],
            "elapsed_ms",
        ),
    )
    print_duration(
        "catalog scan",
        values(
            [
                event
                for event in events
                if event.get("event") == "storage_catalog" and event.get("action") == "scan"
            ],
            "elapsed_ms",
        ),
    )
    print_duration(
        "progress write",
        values(
            [
                event
                for event in events
                if event.get("event") == "storage_progress" and event.get("elapsed_ms") is not None
            ],
            "elapsed_ms",
        ),
    )

    modes: dict[str, int] = {}
    for event in renders:
        mode = str(event.get("mode", "unknown"))
        modes[mode] = modes.get(mode, 0) + 1
    if modes:
        print("refresh modes: " + ", ".join(f"{k}={v}" for k, v in sorted(modes.items())))
    if warnings:
        print("warning lines:")
        for event in warnings[:10]:
            print(f"  {event.get('line', event)}")
    budget_warnings = evaluate_budgets(events, load_budgets(budgets_path))
    if budget_warnings:
        print("budget warnings:")
        for warning in budget_warnings:
            print(f"  {warning}")
    return budget_warnings


def read_events(path: Path) -> list[dict[str, Any]]:
    result = []
    with path.open(encoding="utf-8") as handle:
        for line_no, line in enumerate(handle, 1):
            line = line.strip()
            if not line:
                continue
            try:
                result.append(json.loads(line))
            except json.JSONDecodeError as err:
                raise SystemExit(f"{path}:{line_no}: invalid JSONL: {err}") from err
    return result


def values(events: list[dict[str, Any]], key: str) -> list[int]:
    return [int(event[key]) for event in events if isinstance(event.get(key), int)]


def print_duration(label: str, data: list[int]) -> None:
    if not data:
        return
    median = statistics.median(data)
    p95 = percentile(data, 95)
    print(f"{label:14} median={median:.0f}ms p95={p95:.0f}ms min={min(data)}ms max={max(data)}ms")


def page_turn_durations(events: list[dict[str, Any]]) -> list[int]:
    pending_inputs: list[int] = []
    durations: list[int] = []
    for event in sorted(events, key=event_sort_key):
        if event.get("event") == "input" and event.get("button") in {"Next", "Previous"}:
            t_ms = event.get("t_ms")
            if isinstance(t_ms, int):
                pending_inputs.append(t_ms)
        elif event.get("event") == "render" and event.get("view") == "Reading":
            t_ms = event.get("t_ms")
            if isinstance(t_ms, int) and pending_inputs:
                input_t = pending_inputs.pop(0)
                if t_ms >= input_t:
                    durations.append(t_ms - input_t)
    return durations


def event_sort_key(event: dict[str, Any]) -> tuple[int, float]:
    t_ms = event.get("t_ms")
    if isinstance(t_ms, int):
        return (0, float(t_ms))
    host_time = event.get("host_time")
    if isinstance(host_time, (int, float)):
        return (1, float(host_time) * 1000)
    return (2, 0)


def load_budgets(path: Path | None) -> dict[str, Any]:
    if path is None or tomllib is None or not path.exists():
        return {}
    with path.open("rb") as handle:
        return tomllib.load(handle)


def evaluate_budgets(events: list[dict[str, Any]], budgets: dict[str, Any]) -> list[str]:
    warnings: list[str] = []
    page_turn = budgets.get("page-turn", {})
    if page_turn:
        render_events = [event for event in events if event.get("event") == "render"]
        turn_durations = page_turn_durations(events)
        warn_if_above(
            warnings,
            "page-turn median",
            statistics.median(turn_durations) if turn_durations else None,
            page_turn.get("median_press_to_settled_ms"),
        )
        reading_layout = values(
            [event for event in render_events if event.get("view") == "Reading"],
            "layout_ms",
        )
        warn_if_above(
            warnings,
            "Reading layout p95",
            percentile(reading_layout, 95) if reading_layout else None,
            page_turn.get("reading_layout_warn_ms"),
        )
        prestage_values = values(render_events, "prestage_ms")
        warn_if_above(
            warnings,
            "prestage p95",
            percentile(prestage_values, 95) if prestage_values else None,
            page_turn.get("prestage_warn_ms"),
        )
        fast_busy = [
            int(event["busy_ms"])
            for event in events
            if event.get("event") == "refresh"
            and event.get("mode") == "Fast"
            and isinstance(event.get("busy_ms"), int)
        ]
        warn_if_above(
            warnings,
            "Fast refresh busy p95",
            percentile(fast_busy, 95) if fast_busy else None,
            page_turn.get("fast_refresh_busy_warn_ms"),
        )

    sleep_sync = budgets.get("sleep-sync", {})
    if sleep_sync:
        full_busy = [
            int(event["busy_ms"])
            for event in events
            if event.get("event") == "refresh"
            and event.get("mode") == "Full"
            and isinstance(event.get("busy_ms"), int)
        ]
        min_ms = sleep_sync.get("full_refresh_busy_min_ms")
        max_ms = sleep_sync.get("full_refresh_busy_max_ms")
        for busy in full_busy:
            if isinstance(min_ms, int) and busy < min_ms:
                warnings.append(f"Full refresh busy {busy}ms below budget floor {min_ms}ms")
            if isinstance(max_ms, int) and busy > max_ms:
                warnings.append(f"Full refresh busy {busy}ms above budget ceiling {max_ms}ms")
        failed_sleeps = [
            event
            for event in events
            if event.get("event") == "sleep" and event.get("ok") is False
        ]
        if failed_sleeps:
            warnings.append(f"{len(failed_sleeps)} failed sleep phase(s)")
    storage_cache = budgets.get("storage-cache", {})
    if storage_cache:
        storage_open = values(
            [event for event in events if event.get("event") == "storage_open"],
            "elapsed_ms",
        )
        warn_if_above(
            warnings,
            "storage open p95",
            percentile(storage_open, 95) if storage_open else None,
            storage_cache.get("warm_book_open_warn_ms"),
        )
        catalog_load = values(
            [
                event
                for event in events
                if event.get("event") == "storage_catalog" and event.get("action") == "load"
            ],
            "elapsed_ms",
        )
        warn_if_above(
            warnings,
            "catalog load p95",
            percentile(catalog_load, 95) if catalog_load else None,
            storage_cache.get("catalog_load_warn_ms"),
        )
    return warnings


def warn_if_above(
    warnings: list[str],
    label: str,
    actual: float | None,
    threshold: Any,
) -> None:
    if actual is None or not isinstance(threshold, int):
        return
    if actual > threshold:
        warnings.append(f"{label} {actual:.0f}ms above warning budget {threshold}ms")


def percentile(data: list[int], pct: int) -> float:
    if len(data) == 1:
        return float(data[0])
    ordered = sorted(data)
    index = (len(ordered) - 1) * pct / 100
    lower = int(index)
    upper = min(lower + 1, len(ordered) - 1)
    fraction = index - lower
    return ordered[lower] * (1 - fraction) + ordered[upper] * fraction


def run_channel_stress(args: argparse.Namespace) -> int:
    if not args.host:
        raise SystemExit("channel-stress currently requires --host")
    model = ChannelStressModel()
    model.run()
    print("channel-stress: ok")
    for check in model.checks:
        print(f"  {check}")
    return 0


class ChannelStressModel:
    """Tiny host model for the firmware's coalescing contract."""

    def __init__(self) -> None:
        self.rendering = False
        self.render_pending = False
        self.state_page = 0
        self.rendered_pages: list[int] = []
        self.pending_storage: int | None = None
        self.latest_request_id = 0
        self.checks: list[str] = []

    def run(self) -> None:
        self.input_page_turn()
        self.input_page_turn()
        self.input_page_turn()
        assert self.rendering
        assert self.render_pending
        self.display_settled()
        assert self.rendering
        assert not self.render_pending
        self.display_settled()
        assert self.rendered_pages == [1, 3]
        self.checks.append("input during render coalesces to latest reader state")

        stale = self.open_request()
        fresh = self.open_request()
        assert stale < fresh == self.latest_request_id
        assert not self.storage_request_is_current(stale)
        assert self.storage_request_is_current(fresh)
        self.checks.append("stale open/extend requests are rejected by request id")

        self.pending_storage = 1
        self.sleep()
        assert not self.rendering
        assert not self.render_pending
        self.checks.append("sleep clears render in-flight and pending-render state")

        refused = {"OpenBook", "ExtendSection", "LoadChapters", "JumpChapter", "LoadCatalogCache"}
        admitted = {"StoreProgress", "StoreWifiCredentials", "ReceiveUpload"}
        assert all(not sync_loaned_admits(command) for command in refused)
        assert all(sync_loaned_admits(command) for command in admitted)
        self.checks.append("sync session admits only progress, credentials, and upload after loan")

    def input_page_turn(self) -> None:
        self.state_page += 1
        if self.rendering:
            self.render_pending = True
        else:
            self.rendering = True
            self.rendered_pages.append(self.state_page)

    def display_settled(self) -> None:
        self.rendering = False
        if self.render_pending:
            self.render_pending = False
            self.rendering = True
            self.rendered_pages.append(self.state_page)

    def open_request(self) -> int:
        self.latest_request_id += 1
        return self.latest_request_id

    def storage_request_is_current(self, request_id: int) -> bool:
        return request_id == self.latest_request_id

    def sleep(self) -> None:
        self.rendering = False
        self.render_pending = False


def sync_loaned_admits(command: str) -> bool:
    return command in {"StoreProgress", "StoreWifiCredentials", "ReceiveUpload"}


if __name__ == "__main__":
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    raise SystemExit(main())
