#!/usr/bin/env python3
"""Send timed navigation commands to a bench-serial build and capture bench JSONL.

Commands: o=Confirm, n=Next, p=Previous, b=Back, s=Power. The development
firmware wakes two seconds after deep sleep. This tool reconnects after USB
disconnects but never resends a command whose delivery could be uncertain.
Only one command is pending at a time; the next requires a queued acknowledgement
and the requested interval. Examples: npnp --interval 1.5; sssss --interval 14.
"""
from __future__ import annotations

import argparse
import errno
import fcntl
import math
import os
from pathlib import Path
import select
import struct
import sys
import termios
import time

import bench


def open_port(port: str) -> int:
    fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        attrs = termios.tcgetattr(fd)
        attrs[0] = attrs[1] = attrs[3] = 0
        attrs[2] = termios.CREAD | termios.CLOCAL | termios.CS8
        attrs[4] = attrs[5] = termios.B115200
        termios.tcsetattr(fd, termios.TCSANOW, attrs)
        fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack("i", termios.TIOCM_DTR))
        fcntl.ioctl(fd, termios.TIOCMBIC, struct.pack("i", termios.TIOCM_RTS))
        return fd
    except BaseException:
        os.close(fd)
        raise


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", help="one command or a sequence using only o/n/p/b/s")
    parser.add_argument("--interval", type=float, default=1.5, help="minimum seconds between commands")
    parser.add_argument("--port", default=bench.DEFAULT_PORT)
    parser.add_argument("--seconds", type=float, default=12)
    parser.add_argument("--out", type=Path, default=Path("target/bench/serial.jsonl"))
    args = parser.parse_args()
    if not args.command or any(command not in "onpbs" for command in args.command):
        parser.error("command sequence must contain only o/n/p/b/s")
    if not math.isfinite(args.seconds) or args.seconds <= 0:
        parser.error("--seconds must be finite and positive")
    if not math.isfinite(args.interval) or args.interval <= 0:
        parser.error("--interval must be finite and positive")
    args.out.parent.mkdir(parents=True, exist_ok=True)
    stop_at = time.monotonic() + args.seconds
    fd = None
    sent = 0
    acked = 0
    pending_command = None
    rejected = False
    next_send_at = 0.0
    pending = b""
    try:
        with args.out.open("a", encoding="utf-8") as out:
            bench.write_event(out, {"event": "run_start", "suite": "serial", "command": args.command,
                                    "port": args.port, "interval": args.interval,
                                    "expected_commands": len(args.command), "host_time": time.time()})
            while time.monotonic() < stop_at:
                try:
                    if fd is None:
                        fd = open_port(args.port)
                    can_send = pending_command is None and not rejected and sent < len(args.command)
                    if can_send and time.monotonic() >= next_send_at:
                        # Mark before writing: exceptions leave this command
                        # pending and must never cause automatic retransmission.
                        pending_command = args.command[sent]
                        sent += 1
                        next_send_at = time.monotonic() + args.interval
                        if os.write(fd, pending_command.encode("ascii")) != 1:
                            rejected = True
                    timeout = min(0.2, max(0, stop_at - time.monotonic()))
                    if pending_command is None and not rejected and sent < len(args.command):
                        timeout = min(timeout, max(0, next_send_at - time.monotonic()))
                    ready, _, _ = select.select([fd], [], [], timeout)
                    if not ready:
                        continue
                    chunk = os.read(fd, 4096)
                    if not chunk:
                        raise OSError(errno.EIO, "serial disconnected")
                    pending += chunk
                    while b"\n" in pending:
                        raw, pending = pending.split(b"\n", 1)
                        line = raw.decode("utf-8", errors="replace").rstrip("\r")
                        print(line, flush=True)
                        events = bench.parse_line(line, "serial")
                        # Preserve unstructured boot/storage logs alongside
                        # parsed metrics, so reconnects remain reviewable.
                        if not events:
                            events = [{"event": "serial_line", "suite": "serial", "line": line}]
                        for event in events:
                            event["host_time"] = time.time()
                            if (event.get("event") == "serial_command"
                                    and pending_command is not None
                                    and event.get("command") == pending_command):
                                event["sequence_index"] = sent
                                if event.get("status") == "queued":
                                    acked += 1
                                else:
                                    rejected = True
                                pending_command = None
                            bench.write_event(out, event)
                except OSError as error:
                    if error.errno not in (errno.ENOENT, errno.EIO, errno.ENXIO, errno.ENODEV):
                        raise
                    if fd is not None:
                        os.close(fd)
                        fd = None
                    pending = b""
                    time.sleep(min(0.2, max(0, stop_at-time.monotonic())))
            bench.write_event(out, {"event": "run_end", "suite": "serial", "command": args.command,
                                    "acknowledged": acked == len(args.command),
                                    "sent_commands": sent, "acked_commands": acked,
                                    "expected_commands": len(args.command), "rejected": rejected,
                                    "pending_command": pending_command, "host_time": time.time()})
    finally:
        if fd is not None:
            os.close(fd)
    if acked != len(args.command) or rejected:
        print(f"Only {acked}/{len(args.command)} commands acknowledged ({sent} sent); "
              "no uncertain commands were resent.", file=sys.stderr)
        return 2
    return 0


def self_test() -> None:
    """Exercise sequence/ack/reconnect behavior over PTYs, without hardware.

    Run: PYTHONPATH=tools/bench python3 -c 'import send; send.self_test()'
    """
    import contextlib
    import io
    import json
    import pty
    import tempfile
    import threading
    import tty
    from unittest.mock import patch

    original_read = os.read
    for sequence, statuses, disconnect in [
        ("nnpnp", ["queued"] * 5, False),
        ("nnn", ["queued", "queue_full"], False),
        ("nn", [None], True),
    ]:
        master, slave = pty.openpty()
        tty.setraw(slave)
        seen = []
        opens = []
        errors = []
        injected = False

        def device():
            try:
                for status in statuses:
                    ready, _, _ = select.select([master], [], [], 2)
                    assert ready, "missing host command"
                    command = original_read(master, 1)
                    seen.append(command)
                    line = (f"bench: serial_command command={command.decode()} status={status}\n"
                            if status is not None else "bench: serial_ready commands=onpbs\n")
                    os.write(master, line.encode())
            except BaseException as error:
                errors.append(error)

        def fake_open(_):
            descriptor = os.dup(slave)
            opens.append(descriptor)
            return descriptor

        def flaky_read(descriptor, size):
            nonlocal injected
            if disconnect and descriptor != master and not injected:
                injected = True
                raise OSError(errno.EIO, "simulated USB disconnect")
            return original_read(descriptor, size)

        worker = threading.Thread(target=device)
        worker.start()
        try:
            with tempfile.TemporaryDirectory(prefix="bench-send-test-") as directory:
                output = Path(directory) / "capture.jsonl"
                argv = ["send.py", sequence, "--interval", "0.02", "--seconds", "0.45", "--out", str(output)]
                with patch.object(sys, "argv", argv), patch("send.open_port", fake_open), patch("send.os.read", flaky_read), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    result = main()
                events = [json.loads(line) for line in output.read_text().splitlines()]
            worker.join(timeout=2)
            assert not worker.is_alive() and not errors, errors
            expected_acks = statuses.count("queued")
            assert seen == [command.encode() for command in sequence[:len(statuses)]]
            assert events[-1]["sent_commands"] == len(statuses)
            assert events[-1]["acked_commands"] == expected_acks
            assert result == (0 if expected_acks == len(sequence) else 2)
            ack_indices = [event["sequence_index"] for event in events if "sequence_index" in event]
            assert ack_indices == list(range(1, 1 + sum(status is not None for status in statuses)))
            if disconnect:
                assert injected and len(opens) >= 2, "reconnect was not exercised"
            ready, _, _ = select.select([master], [], [], 0)
            assert not ready, "a rejected or uncertain command allowed another send"
        finally:
            os.close(master)
            os.close(slave)
    print("PTY tests passed: repeated commands, rejected acknowledgement, disconnect without resend")


if __name__ == "__main__":
    raise SystemExit(main())
