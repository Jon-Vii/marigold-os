# Design: images in EPUB books

Status: draft for review (2026-10-22). Supersedes `PRD.md` in this folder,
which is the plan behind the prototype in #17 (Jan Steinke). This design keeps
that prototype's core shape and its test assets, and changes most of the rest.

## Summary

Show the images in a book on the reading page. Each image is decoded once
during the book build, dithered to 1 bpp, and stored on the card. A page draw
copies the stored rows into the framebuffer. The work runs inside the existing
build sessions, gives way to input, and adds no static RAM.

## What books actually contain

A small survey (8 Project Gutenberg EPUB3 editions, 2026-10-22). It is biased
to one producer, so the design does not depend on these proportions, but they
set the priorities:

- Photos and engravings are baseline JPEG (all 77 JPEGs; none progressive).
- PNGs include RGBA (`ct6`), 1-bit palette (`ct3 bd1`), and 4-bit palette
  (`ct3 bd4`). The prototype rejected all three.
- Byrne's *Euclid* (#21076) holds 639 images, median 138 px wide and
  516 bytes, most of them inline in running text.
- In all 8 books, the cover page wraps its image in `<svg><image xlink:href>`, which the
  parser skips today.
- Gutenberg stores image entries uncompressed. Other producers deflate them,
  so deflated entries must still work.
- Widths reach 1,600 px; the prototype's 792 px cap rejected 26 of 737.

## Goals

1. PNG: every non-interlaced variant — gray, RGB, palette, gray+alpha, RGBA;
   bit depths 1, 2, 4, 8, 16; tRNS. Alpha composites onto the white page.
2. Baseline JPEG (phase 4), decoded with DCT-domain downscaling (1/2, 1/4, 1/8)
   so a large photo costs far less than its full-size pixel count.
3. `<img>` and the single `<image>` inside an `<svg>` wrapper.
4. Floyd–Steinberg error diffusion at build time. It is computed once and
   stored, so every draw shows the same bits and fast refresh cannot flicker.
5. Books without images behave exactly as today: same pagination, same cache
   files (not even a one-time rebuild after the update), no extra card access,
   no extra RAM. The only added work is a branch per block and per page.

Out of scope for v1: interlaced PNG (needs the whole image in RAM),
progressive JPEG (needs every coefficient in RAM), GIF/WebP/SVG content,
gray levels, inline images inside a text line (see Open questions), covers
(still host-rendered).

## Budgets (acceptance criteria)

| | Budget |
|---|---|
| Static RAM | +0 bytes on X4 and X3 (X3 stack is 3,336 B above its floor) |
| Stack | decode and draw paths add ≤ 1 KB over today's deepest build frame, checked with `-Zemit-stack-sizes` |
| Flash code | PNG ≤ 40 KB; text + rodata leave only ~200 KB of the 4 MB mapped window |
| Page turn to an image page | within the existing 1 s `check_forward_turns` budget on X4 |
| Input | no stretch longer than one image decode without checking for a press |
| Books without images | no change in `page-turn` or `storage-cache` bench results |

## Design

### 1. Image index (once per book)

`IMAGES.BIN` in the book's cache directory, built at the first build session
of a book whose ZIP central directory has image entries (by extension, then
confirmed by magic bytes). Each entry, sorted by path hash for binary search:

- path hash (u64), ZIP local-header offset (u32), method (stored/deflated)
- source width and height, format and the decoder facts layout needs
- per-orientation pack offset and status (not decoded / decoded / failed)

The OPF parse drops non-spine manifest items, so the central directory is the
source. The index does not depend on type settings, so it survives every
rebuild and is deleted with the book cache.

Nothing is read up front beyond the central directory. Opening a book records
where each image lives, which costs no extra card reads, and leaves every image
unprobed. Probing an image (reading its header with the idle inflater: ≤ 33
bytes for PNG, up to the SOF marker for JPEG; roughly 3 ms per image) happens
only when a chapter needs that image, or during idle sessions. Reading every
header up front would cost *Euclid* about 2 s before a first page that has no
images.

A chapter whose layout meets an unprobed image notes it in the index, finishes
its layout, probes the noted images while the inflater is idle, and lays
itself out again with the real sizes. Only chapters with newly seen images pay
for that second pass, and text layout is cheap next to the decode that follows.

### 2. Layout

On `<img src>` (or `<svg><image href>`), the parser resolves the path against
the chapter, hashes it, and looks it up in `IMAGES.BIN` (a few small card
reads; card access mid-spine is already normal, since sections are written
from inside the inflate callback). A hit with a supported format becomes an
image block; anything else keeps today's alt-text placeholder, with entity
decoding and whitespace collapsing preserved.

The fitted size depends only on the orientation's page box (X4 landscape
784×451, portrait 464×763), because there is no margin setting. Images are
never scaled up.

An image block reuses the existing 12-byte record:

- `image_height: u16` occupies the struct's 2 padding bytes in RAM and the
  unused on-disk bytes 10–11. Zero means a text block, so v27 caches read as
  they always did.
- The text blob holds the image's index slot as a short token, which keeps
  the spacing-reflow rules (non-empty, no edge whitespace) satisfied.
- `line_count` stays 1. `block_height` returns the image height plus a top
  gap of about the body descent, which fixes the overlap seen in #17, plus the
  usual paragraph gap.

`CACHE_V2_VERSION` goes to 28, but a version bump must not rebuild books that
have no images. A v27 cache is accepted as before. The first build session that
uses it checks the book's ZIP central directory for image entries, which it
already reads, so this costs no extra card reads. With none, the index is
stamped v28 and nothing is rebuilt. With some, the book is rebuilt so its
images replace their alt-text placeholders. The section loader must not
mistake a v27 section for one with image blocks; bytes 10–11 are zero in v27,
so `image_height` reads as 0. The spacing reflow carries `image_height`
through, since it does not depend on spacing.

### 3. Decode

Images decode in the background like text is laid out, so a page's pictures are
normally ready before it is turned to. Image decoding is its own idle work, not
a step tied to text layout, because the two want different orders: idle text
layout extends the book from its start in spine order (`session_target`), while
images should be decoded nearest to the reader first.

An image becomes decodable once its chapter is laid out, since only then is its
place in the book known. Its index entry records the spine and a page hint.
Each idle session then works in this order:

1. **Prefetch window**: undecoded images on the next few pages after the one on
   screen, then the previous page. This is what makes a page turn find its
   pictures ready.
2. **Text layout**, as today. It is fast, and it is what makes images further
   on decodable.
3. **The rest of the book's images**, in reading order from the reader's
   position forward, then from the start. Eventually every image is decoded,
   once per orientation; a heavily illustrated book costs a few minutes of
   background work, once.

The inflater and the 24 KB XHTML window are idle between spines and between
sessions, so a decode never overlaps text layout. Idle sessions start after 2 s
without input, and a large figure takes about 2 s (see Measurements). At a
normal reading pace (tens of seconds per page) the next pages' images are done
long before the turn. The decoder checks for a press every few dozen source
rows; a press abandons that image, which is decoded again later from the start.

The only wait is when the reader outruns the background work: flipping quickly
through an illustrated part of a book that was just opened, or jumping into one
from the table of contents. Then the request decodes that page's images before
answering, about 2 s per large image, rather than drawing a box that later
changes into the picture. A press during that wait abandons it and is handled at
once, so flipping past an image page is never held up.

- Stored entries decode straight from ZIP reads. A deflated entry is inflated
  to a spool file first and then decoded from it with the same inflater; one
  43 KB `InflateState` cannot serve both layers at once. Spooling costs about
  3.6 s per MB of PNG (write plus read-back), so it is the slow path. A later
  optimization could lend the 48 KB previous-frame buffer as a second
  `InflateState` during a decode and re-render the previous frame afterwards,
  removing the spool entirely.
- The per-pixel loops avoid division: column mapping and box counts are
  stepped incrementally, and averages use precomputed reciprocals. Sub and Up
  defilter four bytes at a time.
- The decoder carves every buffer from the borrowed window and clears it
  before use. On the host it runs on a dirty (`0xA5`-filled) buffer.
- Output goes to one append-only pack per orientation (`IMGL.BIN`, `IMGP.BIN`)
  instead of one file per image. Opening a file in a FAT directory is a linear
  scan, and a book like *Euclid* would add 639 files. The index stores each
  image's offset; an interrupted decode leaves an orphan tail and no index
  update.
- Rows are stored in panel-native order for the orientation (portrait
  transposed), so a draw is a row copy.
- Font, size, spacing and alignment changes never trigger a re-decode. An
  orientation change decodes into the other pack and keeps the first.

### 4. Draw

A page whose plan contains an image block opens an SD session (as the
custom-font path already does), reads each image's rows from the pack into the
idle XHTML window, and copies them into the framebuffer with a new row-copy
API that shifts bits into place. There is no pop-in: a page is only shown once
its images are decoded (see Decode). An image that cannot be shown (unsupported,
corrupt, or a card read error during the draw) draws an outlined box with its
alt text, and the page is not redrawn while it is on screen; the next visit
tries again. An SD error falls back to the RAM-only page draw, never to the
previous frame.

### 5. Refresh

Measure first: if dithered images ghost under the fast refresh, add a
`needs_clean` bit to the render request so the planner can choose FastClean for
image pages (and for the page after one). Do not add it speculatively.

## Phases

1. **Decoder** (`proto`): PNG to fitted, dithered rows, plus host tests,
   corrupt-input and random-input tests, and decode-vs-reference checks on
   every PNG variant.
2. **Index, layout, records** (`proto`, `ui`, `fw`): `IMAGES.BIN`, image blocks
   in 12-byte records, the version bump, alt text unchanged, image goldens on
   both panels and orientations (reusing #17's orientation-marker assets).
3. **Decode pass, pack, draw, refresh** (`fw`): on-device bench on the X4
   (`page-turn`, `storage-cache`, decode timing, stack sizes).
4. **JPEG baseline**.

Each phase passes the full CI gate and gets an independent review.

## Measurements

Throwaway bench, branch `images/decode-bench`, on the X4 (160 MHz, flash at
80 MHz DIO), 2026-10-22. The images were embedded in flash; pixel sums matched
the host reference, so the decode was correct.

| Stage | Cost |
|---|---|
| Inflate, diagram-like (match-heavy) | 7.4 MB/s of raw pixel bytes |
| Inflate, photo-like (literal-heavy) | 2.5 MB/s |
| Inflate output buffer 512 B vs 4 KB vs 16 KB | no difference |
| Defilter, per byte | None ~0, Sub 76 ns, Up 76 ns, Average 108 ns, Paeth 285 ns |
| Luma + box filter (bench code, divides per pixel) | ~570 ns per source pixel |
| Floyd–Steinberg (bench code, divides per pixel) | ~965 ns per output pixel |
| SD sequential read (4 or 8 KB reads) | ~0.93 MB/s |
| SD sequential write | ~0.41 MB/s |

The luma, box and dither figures measure bench code with per-pixel divisions
and a checksum loop; the design's division-free loops should cut both by
several times. Inflate and card reads are the floor.

Projected cost of a 1600×1200 RGB figure, fitted to 601×451, stored entry:

- Diagram-like PNG (~565 KB): read 0.6 s, inflate 0.8 s, defilter 0.5 s,
  luma/box 1.1 s, dither 0.26 s, write 0.08 s, so **~3.3 s with the bench
  code and ~2 s with the planned loops**. Read plus inflate (1.4 s) is the floor.
- Photo saved as PNG (~3.5 MB): over 7 s, mostly card reads. Rare in books,
  where photos are JPEG.
- Deflated entry: add ~3.6 s per MB of PNG for the spool.
- Tiny images (*Euclid*'s 516-byte median): pixel work is negligible, so the
  per-image card operations dominate. Keep them to a handful per image.

Phase 1 decoder (`proto::image`, commit 308beb9) on the X4, the same 640×480
diagram fitted to 586×440, fed in 4 KB pieces over a dirty borrowed window:
**374 ms**. That is inflate 135, unfilter 35, accumulate 74, row shift 7,
output rows 53, dither 61 (ms). Inflate is the floor. The first version took
885 ms; the review-fix commit alone brought it to 438 ms, so code changes can
swing time 2× through the 16 KB flash cache. Time every decoder change on the
device. Code: about 13 KB on riscv32; deepest frame 224 B.

Scaled to a 1600×1200 diagram-like PNG: about 1.7 s to decode plus 0.6 s to
read it from the card, so **~2.3 s per large figure**.

The ESP32-C3 ROM `memmove` (which `esp-rom-sys` links) copies backward a byte at
a time: 77 ns per byte. The decoder now shifts rows by words itself. Other
large overlapping copies in the firmware (`reader_store.rs` `carry_last_page`,
`epub.rs` text buffer) copy forward, which may not be affected; time them
before assuming either way.

Code size: today's firmware leaves ~200 KB in the C3's 4 MB flash-mapped
window for code plus read-only data, which bounds the decoders.

## Open questions

1. **Inline images.** *Euclid*-style books put small images inside sentences.
   v1 lays every image out as its own block, which breaks those sentences.
   Supporting images inside a line means teaching the line builder about
   inline objects. Decide after v1 ships.
