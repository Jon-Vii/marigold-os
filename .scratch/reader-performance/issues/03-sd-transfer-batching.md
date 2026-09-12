# Measure contiguous FAT reads after X4 transfer batching
Status: ready-for-agent

X4 full-sector transfers shipped in commit 77343dd: matched 35-book catalog
loads improved from 428/428ms to 386/387ms. X3 retains 64-byte chunks. See
`docs/plans/2026-09-12-reader-performance.md` for evidence and test limits.

Next measure contiguous FAT reads against this baseline, covering cold/warm
book loads, writes, and sleep/wake. Do not infer throughput from transaction
ratios alone; command overhead, card latency, and CPU work remain.

September 12 release links leave 37,160 bytes of stack for X4 and 28,488 for X3;
the minimum is 27,648. A larger RX buffer subtracts static stack headroom and a
larger bounce buffer also raises active stack demand. Validate both links and
actual deep-path behavior before enabling larger chunks, especially on X3.

The pinned SD driver ALREADY implements CMD18 for multi-block requests. The FAT
file reader feeds a one-sector BlockCache, so contiguous-sector batching belongs
in that read path. Preserve the proven driver initialization revision; do not
upgrade the SD driver to obtain a command it already supports.

Enable a change only after repeated matched-book measurements demonstrate a
material improvement without storage errors or stack-budget regressions.
