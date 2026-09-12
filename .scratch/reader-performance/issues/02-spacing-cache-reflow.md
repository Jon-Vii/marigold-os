# Repaginate cached text after spacing-only settings changes
Status: completed

Implemented in `2ac49e6` and follow-up validation commits.

Completed caches now reflow cached lines through the shared page plan across
old section boundaries, preserving paragraph flags and rebuilding every page,
section, and TOC mapping. The other section bank is published only after a
complete successful write. Incomplete or incompatible caches fall back to EPUB
parsing.

Both panel geometries pass all six spacing transitions against fresh layout.
A 100-cut write-interruption test remounts and validates the surviving layout.
On the X4, completed-cache Relaxed→Compact and Compact→Normal reflows took
4,845 ms and 4,941 ms respectively, without opening the EPUB. Original Normal
spacing was restored. See the dated performance report for full validation.

Report: `docs/plans/2026-09-12-reader-performance.md`.
