# MarigoldOS Context

## Glossary

### SD session

A short-lived board I/O operation that borrows the shared SPI bus, deselects the display, prepares the microSD card, opens the FAT root directory, runs one storage action, closes the session, and restores display-speed SPI before returning.

Use this term for the shared storage access module in firmware. Avoid re-describing the low-level SPI/card setup ritual at each catalog, cache, or progress-write caller.

### Reader store

The board I/O task's bounded read model for catalog entries, active book metadata, cover data, TOC records, section pages, text blocks, and load status.

Rendering and task coordination should prefer ReaderStore query methods such as catalog entries, active book labels, selected cover, advertised page count, source identity, and block records. Raw field access is still acceptable inside cache construction and file decoding code that is actively maintaining the bounded arrays.

### Reader cache files

The firmware module that owns the FAT paths and binary records for `/XTEINK/CATALOG.BIN`, `/XTEINK/CACHE2/E<hash>/BOOKA.BIN` and `BOOKB.BIN` (with legacy `BOOK.BIN` fallback), section files, cover sidecars, and persisted state generations.

Use this term for cache file I/O. Keep EPUB parsing, XHTML decoding, and pagination language separate from cache file persistence.

### Reader cache artifacts

The shared cache contract behind catalog snapshots, book records, section records, cover sidecars, cache keys, file names, version bytes, and fixed binary layouts.

Use this term when discussing the portable cache format shared by firmware and host tools. Keep adapter-specific details, such as FAT directory handles or host filesystem paths, outside the artifact language.

### Committed reader prefix

The durable, readable part of a book cache whose index still holds a next-spine cursor. Its page count describes available pages; the final book total remains unknown. Reading extends it only when a requested page or chapter is not yet available, with no idle prefetch.

Use this term for resumable progress. A terminal `partial` cache has reached a capacity or parsing limit and has no continuation cursor; the active index still holds at most 320 sections. Do not treat the two states as interchangeable.

### Reader cache generation

A committed book index plus the section files it references. Alternating `BOOKA.BIN` and `BOOKB.BIN` records carry generation/checksum footers; loading verifies the newest valid record. Fresh builds and full-book spacing reflows use the other section bank while preserving the current generation. Prefix continuation appends unused section IDs in the current bank.

Use this term for recovery and replacement. The two 320-section banks provide room for interrupted replacement, not a larger active book index. A storage step processes at most one eligible source spine, then closes its handles and yields before continuing. This is a cancellation boundary, not a wall-clock latency guarantee for a large spine.

### Reader page plan

The resolved page-level reading plan produced from Reader store blocks, typography rules, page bounds, paragraph gaps, style markers, and TOC page targets.

Use this term for the deepened pagination/rendering seam that firmware reading views and host preview tooling should share. Avoid duplicating wrapping, style-marker interpretation, and page slicing separately in preview and firmware render paths.

### Wireless session

The one-way Wi-Fi mode behind the Wireless screen: join the saved
network and serve the browser shelf page (catalog listing, EPUB upload,
book removal) until the done press, or run the onboarding portal when no
network is saved. Entering it loans the EPUB scratch (plus the dram2
segment) to the radio as heap, so the reader pipeline is gone until the
session ends in a software reset. The display task keeps serving
renders, progress writes, and upload writes during the session; it
refuses every scratch-using storage command.

Use this term for the wifi task's lifecycle. Keep the captive-portal
codecs (`proto::captive`), the memory loan plumbing (`fw::sync_mem`), and
the upload streaming plumbing (`fw::upload`) out of radio and UI
language.

The session's storage-admission rules — which storage commands may run
before and after the loan — live in `app_core::SyncSession` beside the
message contracts, not in display-task flags. Ask the session
(`admits`, `active`) rather than re-deriving the whitelist at call sites.

### Refresh plan

The display-policy decision that maps render history, view/book context, selection changes, library changes, sleep/wake state, and refresh policy into an SSD1677 refresh mode.

Use this term for the shared full-vs-fast refresh decision used by firmware and emulator. Keep panel command streaming, framebuffer transforms, and adapter-specific panel state outside the refresh plan.
