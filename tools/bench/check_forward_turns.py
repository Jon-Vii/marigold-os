#!/usr/bin/env python3
"""Check a pure Next sequence captured by send.py against page-turn latency."""
import argparse
import json
from pathlib import Path
import statistics


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path)
    parser.add_argument("--turns", type=int, default=50)
    parser.add_argument("--first-page", type=int, default=1, help="zero-based first destination")
    parser.add_argument("--max-latency-ms", type=int, default=1000)
    args = parser.parse_args()
    rows = [json.loads(line) for line in args.capture.read_text().splitlines() if line.strip()]
    inputs = [r for r in rows if r.get("event") == "input" and r.get("button") == "Next"]
    renders = [r for r in rows if r.get("event") == "render" and r.get("view") == "Reading"]
    builds = [r for r in rows if r.get("event") == "storage_build"]
    differed = [
        r for r in rows
        if r.get("event") == "stage_check" and r.get("done") is True and r.get("match") is not True
    ]
    pages = [r["page"] for r in renders]
    expected = list(range(args.first_page, args.first_page + args.turns))
    errors = []
    if len(inputs) != args.turns:
        errors.append(f"expected {args.turns} Next inputs, got {len(inputs)}")
    if pages != expected:
        errors.append(f"expected exactly one render per forward page; got {pages}")
    if builds:
        errors.append(f"{len(builds)} EPUB preparation steps occurred during reading")
    if differed:
        errors.append(f"{len(differed)} staged pages differed from a fresh render: {[r.get('page') for r in differed]}")
    latencies = []
    if not errors:
        latencies = [frame["t_ms"] - button["t_ms"] for button, frame in zip(inputs, renders)]
        if min(latencies) < 0 or max(latencies) > args.max_latency_ms:
            errors.append(f"turn latency outside 0..{args.max_latency_ms} ms: {latencies}")
    staged = [r.get("staged") is True for r in renders]
    print(
        f"Next inputs={len(inputs)} reading renders={len(renders)} "
        f"staged={sum(staged)} EPUB builds={len(builds)}"
    )
    if latencies:
        print(f"input-to-settled median={statistics.median(latencies):g} ms max={max(latencies)} ms")
        for label, keep in (("staged", True), ("drawn", False)):
            subset = [ms for ms, was in zip(latencies, staged) if was == keep]
            if subset:
                print(f"  {label}: {len(subset)} turns, median={statistics.median(subset):g} ms max={max(subset)} ms")
    if errors:
        raise SystemExit("\n".join(errors))


if __name__ == "__main__":
    main()
