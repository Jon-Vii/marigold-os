---
date: 2026-07-07
topic: stack-headroom-optimization
---

# Further main-stack headroom options (not implemented)

Context: the esp-hal 1.x migration squeezed the main stack to 14.5 KB and the
reader's deep path corrupted .bss (BorrowMutError panic, then the cache-build
lockup). The dram2 rebalance in `fw/build.rs` plus the RX DMA buffer shrink
restored the stack to 36.5 KB (X3) / 45.7 KB (X4), inside the 30-43 KB
EPUB-chain budget noted in the workspace Cargo.toml. A link-time ASSERT now
fails any build whose stack drops under 27 KB.

Because `_stack_start` is pinned to the prev-fb slot at the top of dram2,
**every byte shaved from .data/.bss becomes stack headroom directly**. These
are the two known remaining levers, parked here with their tradeoffs since
current headroom makes them optional.

## Option A: move switch tables back to flash (~2 KB)

esp-hal's `place-switch-tables-in-ram` esp-config option defaults to `true`;
the current image carries ~2 KB of `.Lswitch.table.*` entries in `.data`
(observed: 960 + 832 + 216 bytes in the post-migration ELF). Opting out is one
line in `.cargo/config.toml`:

```toml
[env]
ESP_HAL_CONFIG_PLACE_SWITCH_TABLES_IN_RAM = "false"
```

**Gain:** ~2 KB of DRAM → stack.

**Tradeoff:** the tables cover switch-heavy dispatch, including paths related
to interrupt handling (per the option's own description). From flash they run
through the 16 KB icache; a cache miss during interrupt dispatch adds
XIP-fetch latency, and flash-cache-suspended windows (esp-storage NVM writes)
are exactly when latency spikes hurt most. For an e-reader workload this is
probably unmeasurable, but it should be validated on hardware with the bench
lines (`bench: render ... flush=... t=...`) before shipping - refresh timing
regressions would show up there first.

**Verdict:** cheap and reversible; do it if the budget ever tightens again,
measure the render bench before/after.

## Option B: slim the DISPLAY_EVENTS channel (~2-4 KB)

`DISPLAY_EVENTS` is `Channel<CriticalSectionRawMutex, DisplayEvent, 16>` and
weighs 4,320 bytes of .bss - roughly 270 bytes per slot, because the
`DisplayEvent::Library(LibraryEvent)` variant carries its payload inline and
every slot is sized for the largest variant.

Two independent shrinks:

1. **Halve the capacity (16 → 8): ~2.1 KB.** The queue depth exists to ride
   out bursts while the app task is busy; `send_required_display_event`
   already has a drop-with-retry policy for overflow, and library events are
   the droppable class. Risk: more frequent fallback into the retry path
   during cache builds; the "required display event queue full" log line
   would surface it.

2. **Take the fat payload out of line: ~3 KB at capacity 16.** Move the
   `LibraryEvent` body into a small static slab (or shrink `LibraryEvent`
   itself - its largest member is likely the catalog/label data) and pass an
   index or a compact form through the channel. Risk: real refactor touching
   every sender/receiver of library events, plus lifetime/ownership questions
   for the slab slots; the kind of churn that wants golden-frame coverage
   before and after.

**Verdict:** option 1 is a one-line experiment worth trying next time RAM is
needed; option 2 only pays if the event type grows further - reassess when
the AP file-upload phase adds new event variants.

## Explicitly rejected

- **4-byte stack alignment (ilp32e ABI):** the RISC-V ilp32 psABI mandates
  16-byte stack alignment; the relaxed ilp32e variant needs a custom target
  JSON with `-Zbuild-std` (nightly-only, conflicts with the stable-toolchain
  migration) and would run against esp-hal/esp-rtos naked-asm trap and
  context-switch code written for the standard ABI. Expected gain is only
  0-12 bytes per frame (~1 KB over a deep chain). Not worth the soundness
  risk.
- **Frame-pointer removal:** already off for this target; esp-backtrace's
  raw stack dump does not use frame pointers. Nothing to reclaim.
