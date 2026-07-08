# Portrait mode

Add one portrait orientation (`PortraitButtonsLeft`) to the existing landscape pair.
Landscape stays the default; the enum, persistence (u8 0–3), boot restore, and the
refresh planner's orientation-change FastClean already handle four variants — only
rendering, layout, input mapping, and the settings row are missing.
`PortraitButtonsRight` (front buttons above the screen) has no clear use case and is
not offered; the enum variant remains for the persistence format, and anything that
receives it renders like ButtonsLeft.

## Naming

`PortraitButtonsLeft` (persisted value 2, matching the stock firmware's portrait
shell) is the counter-clockwise device rotation: the front-button column lands below
the screen and the side (page-turn) pair stands on the right edge. The enum name is
historical; the value is what the persistence format cares about.
`PortraitButtonsRight` is the clockwise rotation with the front buttons above the
screen.

## 1. Logical frame in `Framebuffer` (display crate)

Today every renderer draws in an upright frame and then applies whole-buffer post-passes:
`mirror_framebuffer_long_axis`/`flip_vertical` (the panel scans with an inverted y), plus
`rotate_180` for `LandscapeButtonsTop`. Those compose into one per-orientation pixel
mapping, so `Framebuffer` gains a frame mode consumed by `set_pixel`/`pixel`
(everything — text, rects, QR, covers — funnels through them):

| frame              | logical dims | logical (x, y) → buffer (bx, by) |
|--------------------|--------------|----------------------------------|
| Native (default)   | 800×480      | identity (boot/sleep-blank, flush)|
| LandscapeButtonsBottom | 800×480  | (x, H−1−y)                        |
| LandscapeButtonsTop    | 800×480  | (W−1−x, y)                        |
| PortraitButtonsLeft    | 480×800  | (W−1−y, H−1−x)                    |
| PortraitButtonsRight   | 480×800  | (y, x)                            |

The logical frame is the screen exactly as held. Landscape mappings reproduce today's
bytes bit-for-bit (existing goldens must not change), which verifies the refactor. The
post-pass calls in `ui`, `fw/views.rs`, and `tools/emulator` are deleted. No second
framebuffer, no extra RAM; the mapping is one branch + subtraction per pixel.

## 2. Shell layout (ui crate)

`ShellLayout` grows portrait geometry: full-width content column (x 32–448, heading
centered at 240), and the margin-key rail becomes a horizontal row along the bottom
edge, adjacent to the front-button column that now sits below the screen. Slot centers
reuse the physical button positions (120/200/280/360 on the 480 axis). Each slot keeps
the em-dash mark facing the button with the letterspaced-caps label beside it. Reading
stays full bleed — no labels, as today.

Home and sleep title-page furniture scales its baselines proportionally into the taller
page; list screens keep their row rhythm and counts (`LIBRARY_VISIBLE_ROWS` 6 / TOC 9
stay, since fw window sizing is derived from them — more visible rows in portrait is a
possible follow-up, not this change).

## 3. Reading layout + repagination (ui + fw)

Wrap width and page height change in portrait, so pagination is orientation-dependent:

- `ReadingBlocks` gains `page_box()` (left/right/top/bottom), default = today's
  landscape constants; `ReaderStore` returns the box for its configured orientation.
  The `READER_*` const consumers switch to the source's box.
- `reader_layout_config` gains a portrait bit; `READER_LAYOUT_VERSION` bumps so old
  caches invalidate. Both portraits share one bit (same box).
- Orientation-change relayout reuses the type-settings flow: `fw/tasks/app.rs` sets
  `reader_relayout_pending` when `is_portrait` flips; storage open/extend commands carry
  orientation next to `TypeSettings`; the display task's "already paginated" check
  compares the box too.

## 4. Input (app-core)

Positional semantics, as `LandscapeButtonsTop` already established (the button in the
"back position" is Back):

- `PortraitButtonsLeft`: front row keeps natural order; side pair swaps
  (PagePrevious↔PageNext) so the upper button pages back.

Physical wiring direction is asserted from the emulator model; the hardware walk
confirms it and may flip the swap.

## 5. Settings

Orientation row cycles down → up → portrait; label "portrait".

## 6. Emulator, goldens, web

`tools/emulator/src/render.rs` mirrors the views.rs changes. New scenarios cover the
portrait shell screens, reading pages, and the settings cycle; portrait goldens land
beside the landscape set (which must stay byte-identical). Web emulator wasm rebuilds;
its canvas keeps the physical 800×480 panel (portrait content appears rotated, as the
real panel would — a rotated presentation is a follow-up).

Hardware validation (real panel + button walk) is deferred to a bench session; no
reflash in this change.
