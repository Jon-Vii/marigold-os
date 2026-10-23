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
gray levels, covers (still host-rendered). Images inside a text line came
after v1; see Inline images.

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

- Stored entries decode straight from ZIP reads. A deflated PNG is inflated
  to a spool file first and then decoded from it with the same inflater; one
  43 KB `InflateState` cannot serve both layers at once. (A deflated JPEG
  needs no spool: see JPEG.) Spooling costs about
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
4. **JPEG baseline** (done; see JPEG).

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

## Inline images

v1 laid every image out as its own block, which broke sentences that hold an
image (*Euclid*: "Complete the [▱] AEBG" put the symbol on a centred line of
its own) and split drop caps from their words (*Alice*: "HEY were…").

**Classification.** An `<img>` is inline when its paragraph, the parser's
current block, also holds text that is not whitespace, before or after it,
and it is small enough for a line. An image alone in its block (ignoring
whitespace) stays a block image, exactly as before, and so does every
svg-wrapped image. So does an illustration: an image whose block size is
taller than 128 rows (twice the inline cap) in either orientation stays a
block and ends the paragraph around it, as v1 did, even when its paragraph
has text (a plate beside a caption in `<div class="figcenter">`, say);
deciding by both orientations keeps one decoded size per image. For a sink
that keeps images in lines, `<figure>` and `<figcaption>` are blocks, so a
caption is never its figure's paragraph text. A book without an image index
keeps the old parse exactly: the parser ends the paragraph at every `<img>`,
and treats figures as it always did, unless its sink says
`wants_inline_images`, which only a sink with an index does.

The parser streams, so the decision waits for the paragraph. At an `<img>`
the parser ends the current run, not the paragraph, and the sink looks the
image up at once, as before. If the paragraph already has text, the image is
placed inline there. If not, it is held pending: a small record (slot, both
fitted sizes, and the block token or alt text it would need as a block) in the
idle tail of the container scratch, which `container.xml` leaves almost empty
during layout. Nothing is copied onto the parse callbacks' stack. The
paragraph's first text places the pending images inline ahead of it; a
paragraph that ends without text pushes them as image blocks (or alt-text
placeholders) exactly as v1 did. Spacing met among held images is held with
them, in order, so either way it lands where it was; text the sanitizer drops
(Gutenberg's metadata, title-page labels) is no text. If more images arrive
before any text than the record space holds, those held go out as blocks. A
`container.xml` that nearly fills the 4 KB container scratch leaves too
little room to hold even one record (about 200 bytes for an image with its
alt): every image then falls back to v1's block or alt-text placeholder,
even inside a sentence. Real containers are a few hundred bytes.

**Drop caps.** Project Gutenberg sets a drop cap in a floated block of its
own, outside the paragraph it starts: `<div class="figleft"><img alt="T"
…/></div><div class="unindent">HEY were…</div>` in Rackham's *Alice*. The
rule above would keep it a block, so one narrow addition: an image alone in a
block that floats left (class `figleft`, or a `float: left` style) whose alt
is a single letter, optionally after a quote, is carried into the next
paragraph and placed inline at its start when that paragraph is body text
beginning with a letter (the rest of the drop cap's word), in place of the
first-line indent (Gutenberg sets that paragraph `unindent`), its word glued
to it. Anything else commits it as a block: another image, a heading, a
centred line or scene break (typed `* * *` included), a list item's marker,
the chapter's end. Floated illustrations, whose alt is a caption, stay
blocks.

**Size.** `fit_inside(src_w, src_h, column, INLINE_IMAGE_MAX_HEIGHT)` with a
fixed 64 px cap, never upscaled. The column is the narrowest line any block
can start (the page column less the largest first-line indent and the wrap
margin), so an inline image always fits the line it starts. The cap does not
depend on type settings, so an image still decodes once per orientation. A
symbol keeps its natural size; a drop cap becomes a large initial.

**Token and record.** A line holding images carries one token per image in its
text: U+FFFC, then slot, width and height as four hex digits each (15 bytes,
no alt). Style markers and tokens never overlap, and a book with an index
drops any U+FFFC from its own text, so a token is never mistaken for text. The
line keeps `line_count` 1 and its 12-byte `BlockRecord`: `image_height`'s top
bit (0x8000) marks a line with inline images, and its low bits hold the
tallest one's height. A block image's height is under 0x8000 (a page) and
cache v27's zero bytes still read as text, so the field stays unambiguous.

**Line height.** An inline image's bottom sits on the baseline, and text in
its line draws on that baseline. The line's slot above the baseline is the
larger of the line advance and the image plus the block images' top gap (the
body descent and a clear row), and the block images' bottom gap follows it,
so the line above's descenders and the line below's tallest marks clear the
image by a row, as they clear a block image. `block_height` works this out
from the record and the type settings, without reading the text. A line with
a small symbol grows only by the bottom gap (0 to 5 rows by size and spacing;
3 at the default).

**Line breaking.** An inline image is one unbreakable unit of its fitted width
for wrapping and justification; the spaces around it in the source are kept as
they are (a glued neighbour may still wrap away from it, as a styled run's
does). Tokens never reach the hyphenator. An image that cannot be shown
(unsupported, like a progressive JPEG, missing, or failing its probe) places its alt
text inline in italics instead; an empty alt places nothing, and no alt places
`[Image]`, as the block placeholder does. At draw time an inline image that is
not decoded, or fails, draws an outline of its fitted size in its place.

A line holding images is justified whenever it fills two thirds of the
measure, however far its gaps stretch, where a line of text stops at one and
a half spaces a gap. An image takes width but adds no gap, so such a line has
few gaps for its measure: Alice's first line, `[T]HEY were indeed a
queer-looking party that assembled on the`, cannot take `bank—` beside the
64-row initial, and its nine gaps would need two spaces each to fill it.

**Inline and block use of one image.** An image used inline and also alone
elsewhere has two fitted sizes when it is taller than the cap. The index entry
gets a sticky `inline` flag (flags bit 1), set when layout first places the
image inline at a size other than its block size; from then on that image
decodes at its inline size, once per orientation. If it had already decoded at
its block size in an orientation where the two sizes differ, that decode is
forgotten in the same write (its record becomes an orphan in the pack) and is
redone once; an orientation where they agree keeps its decode. The flag is
also set again if it is lost while sections that show the image inline
survive (an index built again, an entry read back fresh): a page that asks
for the image at its inline size, which differs from its block size and only
a line asks for, marks it before decoding. Block uses keep their block layout
and draw the smaller record centred in their box. Nothing alternates between
sizes, so nothing thrashes; the cost is a smaller picture in the rare book
that shows one tall image both ways. Images under the cap, such as *Euclid*'s
symbols, have one size either way and never set the flag.

**Downstream.** The spacing reflow carries `image_height` through, so a line
keeps its images' height; tokens pass its stored-line rules (non-empty, no
edge whitespace, at most 768 bytes). Page windows, `carry_last_page` and
section files move whole records, so nothing changes there. The decode passes
find a page's inline images by walking its image lines' tokens: in RAM for
pages the store holds, and from section files a few dozen bytes at a time
otherwise. The page draw opens its SD session for a page with either kind of
image and hands both to the same drawer, which accepts a record of the image no
larger than its rect and centres it. The custom-font draw walks tokens as the
built-in draw does. A chapter that meets unprobed images still stops, probes
them and lays itself out again; classification happens in that second pass.

**Caches.** `CACHE_V2_VERSION` goes to 29. A v27 index still gets the one-time
image check and is stamped (now as 29) when the book has no PNGs; v28 caches
existed only on development devices and simply rebuild. A book without images
builds through the same code as before, byte for byte apart from the version
stamp: the parser's old path, no tokens, no flagged records, and one branch per
line.

## JPEG

Phase 4 (2026-10-22, branch `images/phase5-jpeg`). `proto::image::jpeg`
decodes sequential Huffman JPEG with 8-bit samples (SOF0 baseline and
SOF1 extended) with the PNG decoder's contract: fed in pieces of any
size, rows to a `LumaSink` at exactly the fitted size, everything carved
from borrowed scratch and initialized before use, no panic on any input.

**What decodes.** Gray, or YCbCr with any sampling factors of 1 or 2
(4:4:4, 4:2:2, 4:2:0, 4:4:0, and odd mixes, luma coarser than chroma
included), in one interleaved scan or a scan per component in any order,
with restart intervals. APPn and COM are skipped, so EXIF orientation is
ignored. Progressive, arithmetic, lossless and hierarchical frames,
12-bit samples, CMYK/YCCK, two components, factors of 3 or 4, and RGB
(Adobe transform 0, or components named R, G, B without JFIF) probe as
unsupported and keep their alt text. At most four Huffman tables are
kept at once (all baseline files; extended allows eight).

**Luma only.** Only Y is dequantized and inverse transformed. Cb and Cr
are Huffman-decoded to stay in step with the stream and dropped, and a
scan without Y is passed over to its next marker. JPEG's Y uses the
BT.601 weights the PNG luma rounds to.

**DCT scaling.** The decoder picks the smallest of 1/8, 1/4, 1/2 and 1 at
which the image (each side rounded up, as libjpeg does) still covers the
fitted size both ways. Reduced IDCTs (DC only, 2×2 from DC and the odd
coefficients, 4×4 from all but the fourth) give each pixel the exact mean
of the square of full-size pixels it stands for, in 13-bit fixed point
with no division; the full size uses libjpeg's islow. The PNG decoder's
division-free box filter then takes the scaled image to the exact size,
across each MCU as it completes and down each row of MCUs. A 1600 × 1200
photo fitted to 601 × 451 decodes at 1/2, a quarter of the IDCT and
filter work; a 4,000-px photo at 1/4.

**Scratch.** `4,227 + (4 + rows) · out_w` bytes, rows = the block side
times luma's vertical factor (one for gray), at most 16. It does not
depend on the source width: at the X4's 784 px the worst case is 19,907
bytes (a 4:2:0 image just under twice the page width, decoded at full
size), which fits the XHTML window beside the ditherer and a 1 KB inflate
buffer; a 4,000-px 4:2:0 photo fitted to 784 needs 10,499. Huffman codes
up to 8 bits resolve by one lookup, longer ones by a walk over the code
lengths.

**Bounds on hostile input.** Work is bounded per byte, not per image:
every block takes at least two symbols and every symbol a bit, so a byte
of scan data decodes at most four blocks, and about 64 KB can describe a
4096 × 4096 gray image of flat blocks, which then takes as long as that
image. Data that runs into a marker part way through an MCU finishes it
on zero bits, as libjpeg does, and the next MCU fails, so a marker buys
at most one made-up MCU; a restart interval with no data fails, so a
scan of nothing but restart markers (two bytes an MCU otherwise) fails at
its first. Restart markers must come in sequence where they are due,
after nothing but fill; anything else is corrupt, without resync. An EOI
with rows still to come fails at once, and at most 256 bytes in all may
lie outside segments (junk, stray TEM and RST markers), so neither junk
after an early EOI nor junk between segments is read to the end of the
file. What that leaves is well-formed input that never reaches the last
row (endless segments the decoder passes over, or a long scan without
luma), read as far as the entry goes: a stored file's size, or a deflated
entry's declared size, itself capped (below).

**Probing.** The frame header can follow 64 KB of EXIF thumbnail.
`ImageProber` reads a stored image a card block at a time and seeks past
every segment it does not read; a deflated image is inflated and the
skipped bytes dropped. At most 256 KB are read, or inflated (a chain of
maximal APP1 segments deflates about 1,000 to 1, so a cap on compressed
input alone would let one probe inflate a quarter of a GB), after which
the image is failed. A PNG still stops at its first 33 bytes.

**Decoding.** A stored JPEG streams from ZIP reads. A deflated one
streams through the ZIP inflater straight into the decoder, a kilobyte of
output at a time, with no spool: the JPEG decoder needs no inflater, so
the one inflater serves only the ZIP layer. As for a spooled PNG, a
deflated entry past 4 MB by its declared size is refused, and one that
inflates to more than it declares is damage, so a small stream cannot
inflate on until a press (which records nothing, and would start again).
Packs, records, scheduling, cancellation and drawing are the PNG path's.

**Index and caches.** `IMAGES.BIN` version 2 holds JPEGs (`.jpg`,
`.jpeg`); a version 1 index is built again, deleting its packs, as any
rebuild does. `CACHE_V2_VERSION` goes to 30. A v27 index keeps its
one-time check, now counting JPEGs as well as PNGs, with one exception:
nearly every book has a JPEG cover, so counting it would rebuild nearly
every book once after the update. The check walks the whole central
directory, and a book whose only image entry's file name contains
`cover` (in any case) counts as having none: it is stamped 30 and keeps
its sections. The trade-off, accepted: such a book's cover page keeps its
v27 rendering (alt text, or nothing for an svg-wrapped cover) until it is
laid out again for another reason, since its index says it has no images.
A fresh build or relayout checks the ZIP again, counting the cover, and
shows it. A cover and any other image, or one image not named as a cover,
rebuild as before; v28 and v29 never shipped, and rebuild. svg-wrapped
JPEG covers become image blocks.

**Cost.** About 13 KB of `.text` on riscv32 for the decoder (its main
loop with the IDCTs and filter inlined, 10.8 KB), 1.4 KB for the probe,
1.3 KB of firmware glue; flash tables are the 64-byte zigzag only.
Statics are unchanged on both panels and the X3's main-stack region stays
30,984 bytes. The deepest image path under the display task is the
deflated JPEG's (decode, inflate callback, decoder, pack write, card
write) at 10,432 bytes against the PNG decode's 10,112 before; the
deepest build path (18,944) and the task's overall deepest (19,520,
the OTA check) are unchanged. Two existing frames moved: fat LTO no
longer inlines the ditherer, now shared by two sinks, into PNG's row loop,
whose frame went from 240 to 416 bytes (its path is 10,288), and the
probe's from 288 to 368 for the prober it holds (its path is 13,936).

**Tests.** Against zune-jpeg on jpeg-encoder files (every layout, sizes 1
× 1 to 250 × 180, qualities 20 to 97, restart intervals), at full size
within 2 levels per pixel and a quarter level mean. zune-jpeg 0.5.15
decodes subsampled luma in a scan per component wrongly, so those files
must decode exactly as their interleaved twins. A small in-test encoder
writes the layouts jpeg-encoder cannot (odd sampling, separate scans in
any order, SOF1 with 16-bit tables and table ids 2 and 3, Adobe and RGB
markers, five tables) against a float-IDCT reference: within 1 level at
full size, and at every scale within 2 per pixel of the reference
averaged over the scale's squares and box-filtered the same way. Against
the full-size reference box-filtered straight to the output, smooth
photo-like images stay within 2 levels mean and 8 per pixel (measured
1.9 and 7): where an output column's span does not line up with the
scale's squares, a square that straddles two output pixels counts whole
in one. Saturated black-on-white line art differs more from the
reference, because the reduced IDCTs clamp the mean of a square rather
than each pixel: ringing past 255 beside white paper and below 0 beside
black ink cancels in the mean, but per-pixel clamping keeps the half it
does not cut, so the reference is grayer at both ends. Measured on a test
drawing (mean, worst): 2.8/7, 2.7/10 and 2.9/19 levels at 1/8, 1/4 and
1/2 at quality 60; 0.9/3 to 1.1/7 at 85; within 2 at 95. Against the
drawing itself the decoder is the closer of the two at every scale and
quality (at 60 and 1/2, 1.8 mean against 3.4), which the test checks.
Plus 1-byte and random feeding, dirty and reused scratch, a 20,000-file
mutation loop and random segments, an EOI before or part way through the
scan with megabytes of junk after it, junk and stray markers between
segments, restart markers out of sequence, missing, or with data before
them, a scan of only restart markers claiming a 16,384 × 4,096 image, a
deflated APP1 chain that inflates 1,000 to 1, and firmware tests that
probe past a 64 KB APP1 without reading it, decode stored and deflated
JPEGs into the packs with no more card writes than stored, and fail
deflated JPEGs that outgrow their declared size or end early unread.

**To measure on the X4.** Decode time of Gutenberg's illustrated *Alice*
(55 baseline JPEGs) and Rackham's colour plates, large and typical, stored
and deflated, against the PNG figures above; probe time for a JPEG behind
an EXIF block; and frames of svg-wrapped JPEG covers and the plates,
checking the white point and dither on photographic content.
