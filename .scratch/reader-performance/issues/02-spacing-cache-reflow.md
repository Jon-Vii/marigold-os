# Repaginate cached text after spacing-only settings changes
Status: needs-triage

load_v2_book_index rejects any font_config mismatch. This prevents normal opens
from reaching load_v2_section_cache's existing spacing-only repagination path.

Do not simply relax the header comparison: book section starts, total pages,
TOC targets, and the resident chapter map would remain stale. Also, sections
were cut at whole pages under the OLD spacing; treating each old section as a
new forced page boundary produces unnecessary short pages mid-chapter.

Requirements:
- Stream compatible cached lines through the shared page plan across old section
  boundaries, carrying the last incomplete page with its paragraph flags.
- Recompute all section/global-page/TOC mappings before publishing a new index.
- Write a separate cache generation so interruption cannot combine an old index
  with overwritten sections; retain or invalidate the old generation atomically.
- Fall back to EPUB parsing for incompatible font, size, weight, orientation,
  font-pack identity, malformed records, or missing sections.
- Test all spacing directions against a fresh build, including long paragraphs
  across sections, source/pack changes, and interrupted writes.

Not implemented in the September 12 pass: preserving section continuity and
crash recovery is necessary before enabling this optimization.
