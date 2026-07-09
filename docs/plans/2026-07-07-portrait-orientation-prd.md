# Portrait Orientation — PRD

Status: **implemented (2026-07-08).** All six build-sequence phases landed;
landscape goldens byte-identical, portrait goldens and scenarios added, both
boards build and flush portrait frames through the panel-model protocol.
On-hardware verification of the portrait rocker direction (phase 4) and the
X3 portrait panel (open question below) remain the only bench items.

## Summary

Let the reader hold the device upright. **Portrait** becomes one posture in a
persisted three-posture Settings cycle; **landscape-buttons-down stays the
default** and its reading behavior is unchanged. In portrait the device is
rotated so the front four-key ladder sits along the **bottom edge** — the
natural paperback grip — and the existing Previous/Next pair of that row pages
with the left key going back and the right key going forward. Every view (Home,
Library, Chapters, Settings, Wireless, Sleep, and the reader itself) renders a
true portrait layout in the same Imprint design language: nothing is squeezed,
mirrored, or letterboxed.

Much of the state plumbing already exists and is being *lit up*, not built:

- **`DisplayOrientation`** — all four variants at `app-core/src/lib.rs:95-100`,
  carried on `ReaderState.orientation` and `RenderRequest.orientation`, but
  never read by any renderer.
- **Persistence** — `shell_orientation` / `reading_orientation` bytes already
  in the NVM record (`hal-ext/src/nvm.rs:12-13`) and restored through the
  reducer (`app-core/src/lib.rs:962`); no record-format bump needed.
- **Scenario runner** — `parse_orientation` already accepts
  `portrait-left`/`portrait-right` (`tools/emulator/src/scenario.rs:301`), so
  portrait goldens slot into the existing regression pipeline.

What is genuinely new: portrait-native composition in the framebuffer, a
transposing flush transform, orientation-parameterized shell and reader
layouts, a horizontal bottom key strip, a summoned key sheet while reading,
the Settings row, and portrait support in both emulators.

Out of scope for v1: a summoned side-sheet for landscape reading (landscape
reading behavior is untouched); automatic rotation (there is no
accelerometer); per-book orientation.

## The posture

Landscape puts the four-key ladder on the left short edge and the page rocker
on the bottom long edge. Rotate the device **90° counter-clockwise**: the
ladder lands on the bottom edge and the rocker lands on the right edge. This
is `DisplayOrientation::PortraitButtonsRight` (the "buttons" in the variant
names are the page rocker's edge, matching `LandscapeButtonsBottom`).

The geometry is kind to us. The ladder reads top-to-bottom
Back, Confirm, Previous, Next in landscape (`KEY_YS`, `ui/src/render.rs:25`);
after the counter-clockwise turn it reads **left-to-right
Back · Confirm · Previous · Next** along the bottom. Previous is already left
of Next — the requested paging feel falls out of the existing logical mapping
with **no ladder remap**. The key grammar survives intact: Back still zooms
out, Confirm still affirms, and the browse pair still browses; only the
physical axis changes.

The page rocker, now vertical on the right edge, keeps paging: **upper half =
previous, lower half = next**. Whether the current `SideLayout` mapping
already produces that or reads inverted must be verified on hardware; if it
is inverted, the fix is an orientation-aware `map_hardware()`
(`fw/src/tasks/input.rs:384-405`) rather than new band tables.

Note for the confused: the preview tool's `write_portrait_left_png`
(`tools/preview/src/main.rs:1903`) is the *other* rotation — ladder at the
top. Its rotate-a-finished-landscape-frame approach is a mockup device, not
portrait layout, and it retires once portrait-native previews exist (below).

## Rendering strategy: compose portrait, transpose at flush

Portrait frames are **composed portrait-native**: the framebuffer's logical
size becomes HEIGHT×WIDTH (X4: 480×800, X3: 528×792) and every drawing
primitive — `draw_text`, `fill_rect`, the QR blitter, dot leaders — works in
portrait coordinates unchanged, because they only care about the logical row
pitch. The math is clean: both portrait widths are byte-aligned (480/8 = 60,
528/8 = 66 bytes per row) and `FB_BYTES` is identical either way, so a
portrait frame costs **zero extra RAM**.

The rotation happens once, at flush time. A new **transposing band
transform** joins `fill_transformed_band_impl`
(`display/src/epd/mod.rs:63-110`): when the render request carries a portrait
orientation, each panel band row is gathered by reading a column of the
portrait framebuffer (eight source-row loads and bit extraction per output
byte, against the existing `REVERSE_BITS_LUT` conventions). It writes into
the same `BAND_BYTES` scratch the mirroring transform uses today — **no
second framebuffer, no large stack temporaries**, which is non-negotiable
given the X3's squeezed main stack
(`docs/brainstorms/2026-07-07-stack-headroom-options.md`). The transpose is
pure CPU inside a flush that the e-ink waveform already dominates by seconds.

`mirror_framebuffer_long_axis` (`ui/src/render.rs:815`) — today's
unconditional end-of-render flip — folds into this orientation-selected final
transform instead of being hardcoded in every render path.

Rejected alternatives, one line each:

- **Hardware transpose** (SSD1677 data-entry mode): violates the repo's
  fixed-transform stance (`docs/ARCHITECTURE.md:237-259`) and the UC8253
  would diverge — two panels, two behaviors.
- **Rotated glyph blitting into the landscape framebuffer**: forks every
  drawing primitive and every caller; invasive and permanently error-prone.

## Shell layout and the bottom key strip

The file-scope landscape constants (`ui/src/render.rs:20-57` — `KEY_YS`,
`CONTENT_X = 210`, `HEADING_CX = 480`, `FOOTER_RIGHT`, …) become a
**`ShellMetrics`** struct with one value set per orientation, selected from
`UiShell.orientation` (which `render_shell` finally reads). Views keep their
structure; they draw from metrics instead of consts.

Portrait metrics, in the same Imprint vocabulary:

- **The key strip** is the left margin rail rotated onto the bottom edge:
  `KEY_XS` positions match `KEY_YS`, which are evenly spaced across the physical 
  outer bounds of the device's two front rocker switches (e.g. `[99, 209, 319, 429]`).
  Instead of staggered
  text labels and em-dashes, it uses a single baseline of icons centered
  over the buttons. The icons are hand-rolled 1bpp bitmaps authored in
  ASCII art and packed at compile time (`ui/src/icons.rs`) — a monochrome
  mask is exactly what the 1bpp panel draws, so this costs ~2 KB of rodata
  against the ~18 KB the `embedded-icon`/`embedded-graphics` crates added
  for richer-than-1bpp data the panel would only threshold away. Settings
  uses a sliders/"tune" glyph (a gear reads as a burst at 24px); unused
  keys simply show no icon.
- **Content** runs full-width between comfortable margins — no 210px rail
  offset to honor, so portrait trades column width for page height. The
  heading centers at width/2 with its hairline underline; list screens keep
  `ROW_STEP`, dot leaders, italic right-aligned values, and the `→` selection
  arrow, and show more rows than landscape's six (library 10, contents 16)
  where the taller page allows.
- **Apparatus** — the battery percent sits in a phone-style top-right status
  corner (`battery_y = 30`), lifted above the centered heading and its rule
  so it reads as its own marker rather than hanging off the rule; the icon
  strip owns the bottom. Sleep stays the centered ceremonial plate,
  unchanged in spirit, re-centered for the portrait canvas.

All five shell views (`render_home`, `render_library`, `render_chapters`,
`render_settings`, `render_wireless`) plus the sleep plate
(`ui/src/app_render.rs:92`) render from metrics. The three type voices, the
46/22/16px palette, en-dash folios, and the progress rule carry over
untouched — portrait is the same book, held upright.

## Reading in portrait

The reader's page box gets portrait constants beside the landscape ones
(`ui/src/reading.rs`): margins on the narrower measure, with the body baseline
limit and footer band kept above the summoned-sheet zone. Portrait wrap points
differ from landscape, so **the panel salt and orientation salt claim disjoint
cache bands**: landscape X4 keeps 0..127 so every existing cache stays valid,
portrait X4 uses 128..255, landscape X3 uses 256..383, and portrait X3 uses
384..511. Toggling orientation therefore re-paginates the book — the same cost
and UX as changing type size, and it must be just as unceremonious: the reader
lands on the equivalent position, with the rebuild delay on first open after a
switch. An SD card carries caches for each geometry it has been read under;
they coexist by salt.

The folio ("– 142 –" style counter) keeps its page-box corner above the zone
the summoned sheet occupies.

## The summoned menu

Reading stays chrome-free — the bezel is the margin. In portrait, pressing any
front-ladder key while reading first **summons the key strip as a bottom
sheet** directly above the physical buttons: the margin appears when called
for, exactly the "summoning creates the margin" behavior the design-language
brainstorm specified for landscape
(`docs/brainstorms/2026-06-11-ui-design-language-brainstorm.md`), rotated.
The sheet is a white band carrying the four key icons on a single row (with a
reduced footprint of `PORTRAIT_READING_SHEET_HEIGHT = 48`); while it is up, keys
act on their icons — Back dismisses to the page, Confirm opens Chapters, the
browse pair pages (paging auto-dismisses the sheet, since turning the page is
the answer, not a menu errand).

Mechanism: a summon flag on `ReaderState` handled in the `AppView::Reading`
arm of `apply_input` (`app-core/src/lib.rs:723`), rendered through the
existing `render_shell_overlay` hook (`ui/src/render.rs:70`, currently a
passthrough). The sheet region rides the fast differential-refresh path the
way Settings cursor moves already do.

Landscape reading keeps its current direct mappings in v1; its side-sheet is
deferred (below).

## Settings and persistence

A new **Orientation** row joins Settings as row 5 — after the type block and
the display row, set-and-forget territory — cycling three exposed postures in
`apply_setting`: `LandscapeButtonsBottom` (buttons down) →
`LandscapeButtonsTop` (buttons up, the 180° flip) →
`PortraitButtonsRight` (portrait, ladder along the bottom edge) → back to
`LandscapeButtonsBottom`. The row renders as a standard `index_row` with the
italic value. `PortraitButtonsLeft` stays unexposed in v1.

Persistence unifies to the one user-facing setting. Today the record carries
two bytes and the save path writes a vestigial constant into
`shell_orientation`; both bytes now carry the chosen posture, restored on boot
through the existing `display_orientation_from_u8` path. The record format
already has the bytes — **no NVM version bump**.

## Emulators, previews, CI

- **Web emulator** (`tools/web-emulator/src/lib.rs`): the present path sizes
  the canvas from the frame's orientation (480×800 portrait) — with
  portrait-native composition this is a straight row-major paint, no
  rotation. The CSS device shell in `web/index.html` gets a portrait stance:
  the four front keys move below the panel, the page rocker and power tab to
  the right edge, and ArrowLeft/ArrowRight keep meaning previous/next.
- **Desktop emulator** (`tools/emulator/src/render.rs`): portrait PNG
  dimensions on the same trigger.
- **Goldens**: portrait scenarios for every view (home, library, chapters,
  settings, wireless, reading, sleep, and the summoned sheet) join
  `fixtures/scenarios` → `fixtures/golden`, exercised by the same Pages CI
  check as the landscape set.
- **Preview tool**: portrait-native galleries replace the
  `write_portrait_left_png` rotate-a-landscape mockup.

## Build sequence

Each phase lands green on `main` with landscape goldens byte-identical.

1. **Display core.** Logical-orientation framebuffer, the transposing flush
   band transform for both panels, and the emulators' orientation-aware
   present path. Verified by a portrait test-pattern golden that proves
   pixel-exact addressing (corners, one-pixel borders, text baseline) on the
   emulated SSD1677 and UC8253 protocol models.
2. **Shell.** `ShellMetrics`, the horizontal `dash_key` strip, portrait
   layouts for all five shell views and sleep. Portrait goldens for each;
   landscape goldens unchanged.
3. **Reader.** Portrait page box, new pagination salts, orientation carried
   into `reader_layout_config`. Golden portrait reading pages; a scenario
   proving an orientation flip re-paginates and returns to position.
4. **Setting and input.** The Settings row, unified persistence,
   restore-on-boot. On-hardware verification of the bottom-row feel and the
   rocker direction (flip via orientation-aware `map_hardware` if needed).
   After this phase the feature is user-reachable end-to-end.
5. **The summoned sheet.** Reading summon flag, overlay render, dismiss
   rules, differential-refresh behavior. Goldens for page-with-sheet.
6. **Polish.** Web page chrome rotation, portrait preview galleries,
   `docs/ARCHITECTURE.md` updated to retire the "every surface renders in
   landscape" paragraph (`ARCHITECTURE.md:495-502`).

## Risks

- **Partial-refresh windows change axis.** A portrait dirty rect (say, the
  bottom sheet) transposes to a panel *column* stripe that touches every
  flush band. The fast differential path needs measuring in phase 5; the
  fallback is widening portrait partial updates to full-band stripes, which
  the waveform cost likely hides.
- **Transpose cost per band.** Byte-gather instead of row copy across ~96K
  transforms per full flush. Expected to vanish under the e-ink update;
  measure in phase 1 on the X3 (slowest core budget) before building on it.
- **Pagination rebuild on toggle.** Inherent — wrap points change with the
  page box. Mitigated by disjoint salts (caches coexist per geometry) and by
  framing the delay identically to a type-size change.
- **Rocker direction is a guess until hardware.** The ladder mapping is
  derived and safe; the rocker's physical order in portrait needs a bench
  check (phase 4).
- **Golden count roughly doubles.** CI time and fixture churn; acceptable,
  and the scenario runner already speaks portrait.

## Open questions / deferred

- **Landscape summoned side-sheet** — the brainstorm's original 160px sheet
  over the page's button edge. Same mechanism as the portrait sheet;
  deferred until portrait proves the summon interaction.
- **Chapters from the sheet** — v1 keeps Confirm-opens-Chapters through the
  sheet; whether a long-press or second grammar deserves to exist is a
  later taste question.
- **X3 portrait on hardware** — 528×792 shares all the code paths but the
  UC8253's three-flag transform composes differently with the transpose;
  bench-verify before calling phase 1 done.
