# Reader performance work — September 12, 2026

## Initial pass

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

## Completed follow-up

### Durable progressive opening (`87e24de`)

The board I/O task builds and commits at most one eligible spine per step.
It yields between steps and stops once the requested page or chapter is
available. An unfinished prefix records its next spine; its page count is not
advertised as the final book total. Future pages and chapters remain pending,
and repeated Next presses coalesce. Request epochs reject stale results after
Back, sleep, layout changes, or another book selection. Unresolved saved
positions are not overwritten with provisional page zero.

BOOKA.BIN and BOOKB.BIN carry checksummed generation footers. Replacement
builds use the other section bank, and retire obsolete indexes before reusing
that bank. A verified previous generation survives interrupted replacement.
Legacy BOOK.BIN remains readable. Read failures do not authorize retirement,
and orphan cleanup skips uncertain cache identities.

Cancellation is between source spines, not within an XHTML stream. A large
spine can still take several seconds. There is no idle prefetch.

A cold open of a different 102-spine book built only its first eligible spine:
7 pages in 724 ms, 793 ms for the storage-open command. The first readable
page finished refreshing 3.34 seconds after Confirm, including the loading
plate and panel refreshes. No further spines were built during the remaining
capture. This measures first-page behavior directly, without claiming an
unmeasured before/after speedup for that book.

The connected X4 resumed page 302 by extending its prefix from 18 to 407 pages
and then stopped, leaving later source spines unbuilt. A later chapter jump
extended that same cache to 579 known pages. Subsequent cache opens took
70–75 ms of storage work. These are observations on this book, not a matched
whole-book speedup comparison.

Hardware testing also exposed a chapter cursor briefly reverting to zero and
a second refresh after an already-rendered RAM page turn. Both are fixed:
unresolved TOC targets are skipped, and a RAM reply for a page already on the
panel sends only the chapter cursor.

### Spacing-only reflow (`2ac49e6`)

Completed, non-partial caches can change line spacing without reopening or
inflating the EPUB. Reflow joins cached lines across old section boundaries,
uses the shared cut/carry policy, and rebuilds all section, global-page, and
TOC mappings before committing the other bank. It borrows the existing XHTML
scratch buffer and adds no static text arena. Incompatible layouts, incomplete
prefixes, missing sections, or malformed records fall back to fresh parsing.

Host tests compare all six spacing transitions against fresh pagination on
both panel geometries, including paragraphs crossing old section boundaries.
Fault tests exercise 100 write-failure cut points in a reflow run and remount
the card: the selected generation always loads intact and matches its layout.
Separate generation tests cover 45 write cuts, 32 read cuts, corruption
fallback, legacy migration, source identity, and unknown prefix totals.

### Contiguous FAT reads (`7ec416b`)

The exact pinned SD fork is vendored with an optional `sd-multiblock` feature.
Aligned full sectors are read in batches of up to 16, stopping at FAT cluster
boundaries. Partial sectors retain the existing cache, failed batches do not
advance file position, and CMD18 errors always attempt CMD12 cleanup. SD
initialization is unchanged. The feature remains off by default.

A matched X4 microbench read and verified the same 2,048 bytes 64 times:

| Mode | Read calls | Sectors | Time |
| --- | ---: | ---: | ---: |
| Single-sector | 256 | 256 | 132,705 µs |
| Contiguous | 64 | 256 | 114,019 µs |

This workload takes about 14.1% less time with batching. The matched cached
page-302 open was 70 ms in both builds, so this is not evidence of faster warm
page turns. Cold inflate reads can request up to 8 KiB and can use batching;
small metadata reads and per-sector transfer/CRC costs remain. The opt-in
implementation is available for further workload-specific measurements.

### Automated hardware validation (`1e1f02d`)

Default-off `bench-serial` firmware accepts normal navigation events over USB
and adds a two-second timer wake alongside GPIO wake. The host driver waits
for each acknowledgement and never retries uncertain delivery after a USB
disconnect. Normal release firmware excludes both behaviors.

The isolated font fixture exercised the actual SD metric, bitmap, and drawing
path: four cold reads, zero reads for 1,196 repeated warm glyphs, and exact
pixel matches. The fixture was removed and no custom font pack or typography
settings were installed. Both SD modes passed this test.

The final 50-turn alternating page run produced exactly 50 renders, retained
chapter 11 throughout, and ended at its original page 302. Median layout was
23.5 ms and median panel flush was 439 ms. RAM cache replies took 0 ms and no
longer triggered a duplicate refresh. These timings include the panel's
approximately 421 ms busy interval.

Completed-cache hardware reflows took 4,845 ms (Relaxed→Compact, 701→547
pages) and 4,941 ms (Compact→Normal, 547→579 pages). Neither opened the EPUB.
The original Normal setting was restored. EOF extension also changed a
701-page prefix to complete without adding or duplicating pages.

Raw captures remain under `target/bench/`.

### Directory-end lookup (`45d238e`)

Per-label telemetry found that missing files consumed 79 SD reads per catalog
entry. The pinned FAT lookup stopped only its inner sector loop at the 0x00
end marker; its caller continued scanning every allocated directory sector.
The fix distinguishes the end marker from a full-sector miss and stops the
entire lookup. It applies to FAT16 roots/subdirectories and FAT32. Deleted
entries still continue, and full sectors/clusters still follow the directory.

Eight sparse filesystem tests pass with batching both enabled and disabled,
including exact one-sector misses and stale entries beyond the end marker.
The change adds no buffer, allocation, or SD initialization changes. Hardware catalog loads settled at 160–163 ms over six development-build
boots, versus 615–629 ms before the fix. The instrumented window dropped
from 1,115 to 235 reads. This is the same 35-book card, but the first book
acquired a title cache during intervening cold-open testing, so these are
observed end-to-end timings rather than a byte-identical card-image comparison.
The isolated FAT tests prove the missing-file scan reduction independently.

### Cancelled chapter persistence (`3b05401`)

An unresolved chapter jump now retains the actual page/chapter and carries its
requested chapter in the existing selection field. Progress writes wait for a
resolved jump just as they do for an initial book open. Back and Power cannot
save the future chapter as the current reading position. This adds no state
fields or static RAM. The reducer regression covers cancellation, sleep,
backing out of the chapter list, and successful resolution.

## Final validation and device state

- The combined app-core/proto/UI host run passed 196 tests. UI tests also pass
  with X3 geometry, and X4/X3 reading golden suites pass (8 tests each).
- Web emulator `wasm32-unknown-unknown` check passed. Host channel-stress and
  USB sequence PTY tests passed, including rejected acknowledgements and
  disconnects without retransmission.
- Five consecutive hardware sleep/wake cycles completed successfully. Every
  wake restored chapter 0/page 0 after cancellation of an unavailable chapter
  77 request. The Back event arrived during a spine build; the task finished
  that spine, returned Home, and did not continue building afterward.
- Each font fixture run verified pixels, reported zero warm SD reads, and
  removed its temporary file. No custom font pack was installed.
- The complete vendored filesystem suite passed 55 tests with batching on;
  the 8 sparse read/lookup tests also passed with batching off.
- Normal release links passed for both panels. X4 has 36,720 bytes of stack
  (9,072 above the 27,648-byte floor); X3 has 28,048 (400 above the floor).
  X3's optional multiblock build also linked (28,064 bytes before the final
  allocation-free lookup/cancellation edits). These are linked regions, not
  measured runtime high-water marks. Hardware testing used the connected X4.
- Original book index 34, chapter 11, page 302 and Normal spacing were restored
  and persisted. The normal X4 image is
  `target/bench/reader-performance-final-x4.bin`; it excludes USB injection,
  fixture self-tests, and timer wake. Only the existing app slot at 0x10000
  was flashed; bootloader and partition table were preserved.
- The final normal-firmware boot passed strict storage validation with zero
  warnings. It loaded the 35-book catalog in 146 ms and restored book index
  34, chapter 11, page 302. This final image has no bench serial input or
  automatic timer wake. Capture: `target/bench/final-production-boot.jsonl`.

The three tracked performance action items are completed. Contiguous reads
remain opt-in based on the measured workload tradeoff; spine-level cancellation
and synchronous completed-cache reflow are explicit limits of this implementation.
