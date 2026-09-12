# Reader performance work — September 12, 2026

## Implemented

### Incremental section pagination (`98b474d`)

The EPUB sink now retains completed page records and re-walks only the trailing
page after appending lines or ending the final paragraph. The shared PageCursor
still owns page-breaking policy. Layout changes and section carry use a full
rebuild. No cache format change or extra resident memory is needed.

Regression tests compare every incremental result with the complete page walk
across both panels, spacing changes, forced page breaks, paragraph-end mutations,
section carry, and index capacity.

Reproduce the host timing probe using the actual firmware store and page plan:

```sh
tools/cargo.sh test -p ui --release --target aarch64-apple-darwin \
  --test firmware_pagination benchmark_cache_pagination -- --ignored --nocapture
```

Representative host run: 320 cached one-line body blocks per section, 2,000
sections per sample, median of five samples; release workspace settings.

| Lines appended per update | Full rebuild, µs/section | Incremental, µs/section |
| --- | ---: | ---: |
| 1 | 918.594 | 74.264 |
| 4 | 225.660 | 20.894 |
| 16 | 52.767 | 7.984 |
| 320 | 3.908 | 4.260 |

The four-line case spends about 10.8 times less time indexing. The single-update
case has no repeated work to eliminate. These are synthetic host measurements,
not whole-book or device speedups; parsing, decompression, storage, and panel
refresh are outside the probe.

### Custom-font bitmap working set (`862b8d9`)

A bounded 2,048-byte arena and 32 bitmap records reuse repeated glyphs within one
page render. Missing glyphs load directly into the arena, replacing the previous
512-byte per-glyph temporary. Failed reads invalidate any overwritten records
and never publish partial data. Zero-byte glyphs do not read from the card.

The cache is temporary, inside the non-inlined custom-font drawing function.
It does not persist across pages. This choice avoids consuming static RAM needed
by the deeper EPUB stack. Linked stack regions are unchanged on both panels:

| Panel | Stack region | Margin above 27,648-byte link floor |
| --- | ---: | ---: |
| X4 | 37,608 bytes | 9,960 bytes |
| X3 | 28,488 bytes | 840 bytes |

The X3 custom-font drawing function's compiled frame is 2,560 bytes, separate
from the EPUB construction path. This is a static inspection, not a dynamic
stack-watermark measurement. Tests verify byte preservation, identity/offset/
length matching, eviction, arena wrap, partial read failures, and empty glyphs.
The synthetic repeated-text test uses 1,000 glyphs with a 16-glyph working set;
all cached bytes match their source. It is not a representative device workload.

### Full-sector SD transfers on X4 (`77343dd`)

X4 now transfers each 512-byte SD payload in one DMA operation instead of eight
64-byte operations. X3 retains 64-byte chunks. No SD command, clock rate, or
pinned driver revision changed. The larger X4 RX buffer costs 448 static bytes;
its linked stack region is now 37,160 bytes. X3 remains at 28,488 bytes.

On the connected X4, matched boot/catalog runs using the same card and 35-book
catalog measured:

| SD chunk size | Catalog load, run 1 | Catalog load, run 2 |
| --- | ---: | ---: |
| 64 bytes | 428 ms | 428 ms |
| 512 bytes | 386 ms | 387 ms |

This is a repeatable approximately 9.7% reduction in catalog-load time. It is
not a measured improvement to cold EPUB construction or page-turn latency.
All four strict storage-cache captures completed without warnings. Initial full
panel refresh remained 3,612 ms. Reading/custom-font/sleep interactions require
separate observations; boot checks do not cover those paths.

Logs (untracked): `target/bench/performance-after.jsonl`,
`target/bench/sd64-repeat.jsonl`, `target/bench/sd512-first.jsonl`, and
`target/bench/sd512-repeat.jsonl`. An earlier capture of the previously installed
firmware measured 433 ms; that is not used as the matched SD baseline.

The X4 app image was written at `0x10000` using `espflash write-bin`. Bootloader,
partition table, and SD files were not replaced. The final image is
`target/bench/performance-x4-sd512.bin`.

## Validation

- UI tests, X4/X3 firmware-pagination and glyph-cache regressions passed.
- X4/X3 reading golden tests passed (built-in-font rendering).
- Both firmware release links passed; static stack regions checked with llvm-nm.
- Host channel-stress passed.
- Before/after 60-second page-turn captures contained no input or render events;
  they are not page-turn or sleep/wake passes. Those hardware paths remain
  unverified in this pass. The connected card has no custom font pack, so no
  on-device custom-font timing or pixel comparison was possible.

## Remaining candidates

Local issues under `.scratch/reader-performance/issues/` describe the next work:

1. Progressive opening needs a resumable storage job, an incomplete-generation
   contract, stable location handling, and sleep/Wireless cancellation. Sending
   Loaded while the synchronous builder keeps running cannot unblock rendering
   in the same board I/O task.
2. Spacing-only reuse needs repagination across old section boundaries and a
   complete replacement of page/TOC mappings. Relaxing the book-header check
   alone would publish stale page counts; independently reflowing old sections
   would introduce short pages at their former boundaries.
3. X4 chunk sizing is implemented; contiguous-sector FAT reads remain a
   separate candidate. The pinned SD driver already supports CMD18; its FAT
   reader's one-sector cache prevents batching. Updating the driver is not
   necessary to obtain CMD18 and would risk proven cold initialization.
