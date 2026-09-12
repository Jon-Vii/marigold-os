# Publish requested pages through durable resumable prefixes
Status: completed

Implemented in `87e24de` and follow-up validation commits.

Implemented a demand-driven prefix builder that commits one eligible spine,
yields the board I/O task, and continues only until the requested page/chapter
is available. Unknown totals and unresolved targets are explicit. Request
epochs cancel stale work; provisional positions cannot overwrite saved progress.
Dual checksummed index generations and separate section banks retain a readable
cache across interruptions. Legacy indexes still load.

Host coverage includes prefix boundaries, distant targets, stale events,
section carry, corrupt generations, read/write failures, and persistence.
Hardware covers distant resume, chapter extension, EOF completion, page turns,
and sleep/wake. See the dated performance report for measured results.

Cancellation occurs between spines; an individual XHTML stream is not
preemptible. No idle prefetch is performed. These are implementation limits,
not outstanding tasks in this pass.

Report: `docs/plans/2026-09-12-reader-performance.md`.
