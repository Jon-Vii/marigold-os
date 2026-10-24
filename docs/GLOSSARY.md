# MarigoldOS glossary

The terms the MarigoldOS code and docs use for its moving parts, and what each one means.

### SD session

A short-lived board I/O operation. It borrows the shared SPI bus, deselects the display, prepares the microSD card, opens the FAT root directory, runs one storage action, closes the session, and restores display-speed SPI before returning.

The firmware's shared storage access module is built around SD sessions, so catalog, cache and progress-write callers don't each repeat the low-level SPI and card setup.

### Reader store

The board I/O task's bounded read model for catalog entries, active book metadata, cover data, TOC records, section pages, text blocks, and load status.

Rendering and task coordination should go through `ReaderStore` query methods: catalog entries, active book labels, selected cover, advertised page count, source identity, and block records. Raw field access is still fine inside cache construction and file decoding code that maintains the bounded arrays.

### Reader cache files

The firmware module that owns the FAT paths and binary records for `/XTEINK/CATALOG.BIN`, `/XTEINK/CACHE2/E<hash>/BOOKA.BIN` and `BOOKB.BIN` (with legacy `BOOK.BIN` fallback), section files, cover sidecars, and persisted state generations.

It handles cache file I/O only. EPUB parsing, XHTML decoding and pagination are separate from cache file persistence.

### Reader cache artifacts

The shared cache contract behind catalog snapshots, book records, section records, cover sidecars, cache keys, file names, version bytes, and fixed binary layouts.

This is the portable cache format that firmware and host tools share. Adapter-specific details, such as FAT directory handles or host filesystem paths, sit outside it.

### Committed reader prefix

The durable part of a book cache whose index still holds a next-spine cursor. Its page count covers the prepared pages; the final total for the book is still unknown. Opening the book shows the reader's page as soon as it is ready and prepares the rest in the background while they read.

A committed prefix can be resumed. A terminal `partial` cache is a different state: it has hit a capacity or parsing limit and has no continuation cursor, and the active index still holds at most 1,024 sections.

### Reader cache generation

A committed book index plus the section files it references. `BOOKA.BIN` and `BOOKB.BIN` records alternate, each with a generation/checksum footer, and loading verifies the newest valid record. Fresh builds and full-book spacing reflows write to the other section bank and keep the current generation intact. Prefix continuation appends unused section IDs in the current bank.

Generations are what make recovery and replacement safe. The two 1,024-section banks leave room for an interrupted replacement; the active book index still only uses one of them. A storage step processes at most one eligible source spine, then closes its handles and yields before continuing. That gives a point where the work can be cancelled, but it doesn't bound wall-clock latency for a large spine.

### Reader page plan

The resolved page-level reading plan built from Reader store blocks, typography rules, page bounds, paragraph gaps, style markers, and TOC page targets.

It is the pagination and rendering layer that firmware reading views and host preview tooling should share, so wrapping, style-marker interpretation and page slicing aren't implemented twice in the preview and firmware render paths.

### Wireless session

The one-way Wi-Fi mode behind the Wireless screen. It joins the saved network and serves the browser shelf page (catalog listing, EPUB upload, book removal) until the done press, or runs the onboarding portal when no network is saved. Entering it lends the EPUB scratch to the radio as heap, so the reader pipeline is gone until the session ends in a software reset. During the session the display task keeps serving renders, progress writes and upload writes, and refuses every storage command that needs the scratch.

The session is the wifi task's lifecycle. The captive-portal codecs (`proto::captive`), the memory loan plumbing (`fw::sync_mem`) and the upload streaming plumbing (`fw::upload`) are separate from the radio and UI code.

The session's storage-admission rules (which storage commands may run before and after the loan) live in `app_core::SyncSession` beside the message contracts, not in display-task flags. Call sites ask the session (`admits`, `active`) instead of re-deriving the whitelist.

### Refresh plan

The display-policy decision that maps render history, view/book context, selection changes, library changes, sleep/wake state, and refresh policy to a panel refresh mode (`RefreshMode`). The X4 (SSD1677) and X3 (UC8253) drivers each carry out that mode in their own way.

Firmware and the emulator share this decision (full, fast-clean, or fast). Panel command streaming, framebuffer transforms, and adapter-specific panel state stay outside the refresh plan.

### Staged page

The next reading page drawn into the framebuffer and written to the panel's new-frame plane (BW RAM on the X4, DTM2 on the X3) while the reader is still on the page before, so that a Next press only has to start the waveform. The app task offers the render a Next press would send (`DisplayCommand::StageNext`), and the display task stages it when the page is resident and drawn from RAM.

A staged page stands in for a render only when the render's `ReadingPageKey` and the reader store's fingerprint both match what they were when it was staged. Any press after the offer abandons staging.
