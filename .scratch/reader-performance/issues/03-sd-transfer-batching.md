# Measure contiguous FAT reads after X4 transfer batching
Status: completed

Implemented in `7ec416b` and follow-up validation commits.

The exact pinned SD fork is vendored with bounded contiguous FAT reads and
CMD18 error cleanup. Initialization is unchanged. Host tests cover both FAT
variants, fragmentation, EOF, unaligned reads, coherence, failed transfers,
and retry. Both firmware variants link.

Matched X4 reads of 2,048 bytes × 64 took 132,705 µs with single-sector reads
and 114,019 µs with contiguous reads (about 14.1% less time). Cached page-302
opens took 70 ms in both modes. The experiment is complete; sd-multiblock stays
opt-in because a material warm-open improvement was not measured. X4's earlier
512-byte DMA change remains enabled; X3 retains 64-byte DMA chunks.

The same validation found a separate directory-end lookup bug, fixed and
measured in the follow-up documented in the performance report.

Report: `docs/plans/2026-09-12-reader-performance.md`.
