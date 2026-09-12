# Publish a requested reading section before full EPUB cache construction
Status: needs-triage

The synchronous builder currently walks the complete spine before publishing
Loaded. Historical cold-open timing was 3.9 seconds for 117 pages / 35 chapters.

A useful implementation must return control to the board I/O task after a
renderable section exists, then perform bounded resumable cache work while idle.
Merely sending Loaded from inside the synchronous builder cannot render sooner:
the same task owns both storage and display SPI.

Requirements:
- Preserve the single SPI owner and no-heap reading path.
- Persist a distinguishable incomplete generation; interrupted builds must resume
  or restart without advertising missing sections as ready.
- Represent unknown total pages honestly; chapter navigation and restored global
  pages must not map through an unfinished index.
- Preserve a stable reading location while total pagination grows.
- Yield to sleep and cancel before the Wireless session loans EPUB scratch.
- Test power interruption, mid-spine section carry, distant resume, and queued
  open/extend commands; benchmark time to first readable page separately from
  total build time.

Not implemented in the September 12 performance pass: this changes the reader
state/storage command contract and durable cache lifecycle, not just loop order.
