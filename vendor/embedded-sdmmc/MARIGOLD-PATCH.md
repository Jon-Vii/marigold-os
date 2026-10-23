# Marigold local FAT patches

Source: `https://github.com/Jon-Vii/embedded-sdmmc-rs.git`, commit
`329b3a53a55fb527f62a7813557b420817e831d8` (version 0.9.0).
The source, upstream tests, README, and license notices are vendored from that
exact revision. The SD initialization path remains unchanged, and no upstream
driver upgrade is involved. The existing CMD18 path also gains a narrow
error cleanup: always attempt CMD12 after a data transfer error, preserving the
original error if stopping fails too. A scripted SPI test covers successful
transfers, errors before/after a good block, and failed stop commands. The
CMD25 path gains the matching cleanup, described with the write feature below.

The always-enabled directory lookup fix distinguishes a full-sector miss from
FAT's `0x00` directory-end marker. Previously the inner search stopped at the
marker but returned the same result as a full-sector miss; the outer search
then scanned every remaining allocated sector (and sometimes another cluster).
Both FAT16 and FAT32 now stop the complete lookup immediately at the marker and
return `NotFound`. Deleted `0xE5` entries remain skippable, and full sectors or
clusters still continue normally. This also prevents stale bytes after the end
marker from being mistaken for live directory entries. Production code adds no
buffer or heap allocation, and the SD initialization path is unchanged.

Sparse-disk tests cover FAT16/FAT32 roots and subdirectories: missing names and
junk after the marker read exactly the first directory sector, deleted entries
are skipped, full sectors continue, and full clusters follow their FAT links.
This lookup correction is independent of the experimental batching features.

Deleting a file or directory now releases its clusters. Upstream
`delete_file_in_dir` only marks the directory entry deleted, so every cluster
of a deleted file leaked (Marigold truncated first, which still left the first
cluster allocated; a deleted directory leaked all of its own). After the entry
is deleted, `free_cluster_chain` frees the whole chain from the entry's first
cluster, updating the free-cluster hint and count. The entry goes first, so a
failure between the two steps leaks clusters rather than leaving an entry that
points at freed ones. `ui/tests/cache_files.rs` counts free clusters on a
simulated card to cover single files and a whole book cache.

The default-off `contiguous-file-read` feature changes only the FAT file read
path. Full sectors within the current cluster reach `BlockDevice::read` in
batches of at most 16 sectors (8 KiB), which the existing SD driver handles with
CMD18. Reads stop at each FAT cluster end and follow the existing FAT chain
before proceeding. Partial leading/trailing sectors retain the single-sector
cache. There is no new bounce buffer, heap allocation, static buffer, or FAT
lookahead state. The caller's existing byte buffer is borrowed as transparent
`Block` storage; its alignment remains one byte. Direct reads invalidate the
single-sector cache and advance file position only after a successful batch.

The crate keeps the feature off by default. Since October 2026 the firmware
turns it on, and so does `ui`, so its host tests build the same FAT code
(their 8 MiB test disk has one-sector clusters, so no read there forms a
batch; the tests in this crate cover batching). On an X4 bench build it took a sector from about 528 µs to
443 µs in a 2 KiB read loop (the command and the wait for the first data token
are paid once per run), image decodes 3-6% and spacing reflows about 5%; the
measurements are in the pull request that enabled it. Only the X4 sends CMD18:
no X3 has run it, so `fw::sd_session::CountingDevice` splits each batch into
single-sector reads there.

The default-off `contiguous-file-write` feature is the write-side counterpart
and changes only the FAT file write path. Without it, every sector goes out as
its own CMD24 through the single-sector cache, and the card programs each one
separately. With it, when a write reaches a sector-aligned file offset with at
least two whole sectors of the caller's buffer left inside the current
cluster, up to 16 of them (8 KiB) reach `BlockDevice::write` in one call
straight from the caller's buffer, which the SD driver sends as ACMD23 and
CMD25. A batch is only issued once the existing loop has located the sector
with `find_data_on_disk`, so cluster allocation is untouched: at a cluster end
the next iteration follows the FAT link or, at end of file, allocates and links
a new cluster exactly as before. A batch never spans two clusters, even ones
that happen to be adjacent. Partial leading/trailing sectors, and runs of a
single whole sector, keep the cached per-sector path.

Batches go through `BlockCache::block_device`, which invalidates the cached
sector, so the cache never holds a stale copy of a sector written directly.
After a successful batch the written count, the file's `current_cluster`, its
offset, and its length advance exactly as the per-sector path would advance
them. On a device error none of them move past the batch's start, so a caller
can seek back and retry; a cluster allocated just before the failed batch stays
linked to the file and the retry reuses it. As with reads, there is no new
buffer, heap allocation, static, or lookahead state, and `Block::from_bytes`
borrows the caller's bytes as transparent `Block`s with one-byte alignment.

The SD driver's CMD25 path now always ends an accepted stream, whether or not
the feature is enabled (nothing in the firmware reaches it without the
feature). Previously a rejected data response, busy timeout, or SPI error
returned at once and left the card in its receive state, where the next
command would be swallowed as block data. Now it waits for not-busy, sends the
Stop Tran token, skips the one byte the card sends before raising busy, waits
for not-busy again, and reads status with CMD13, preserving the original error
if any of that fails too. The CMD13 check also runs after a clean stream, so a
programming failure that only shows once busy ends (typically on the last
block) is reported as `WriteError`, as the single-block path already does. A
CMD25 whose R1 carries any error bit now returns `WriteError` without sending
data. The SD spec's data-response section asks for CMD12 after a rejected
block; the driver follows Linux `mmc_spi` and FatFs, which send Stop Tran
instead. Scripted SPI tests cover a clean stream, a block rejected first and
later, an SPI failure mid-block, a busy timeout, failed cleanup, a failed final
status, and a rejected CMD25. The sequence has not yet run on hardware.

The write feature is an experiment until matched hardware measurements justify
enabling it. Host transaction counts are not hardware throughput claims. Keep
feature-off builds as the baseline and verify both X4/X3 stack links, writes
(section text, image packs, browser uploads), and sleep/wake before enabling it
in release builds.

## Verification

Use your host triple (for example `aarch64-apple-darwin` on Apple silicon):

```
tools/cargo.sh test --manifest-path vendor/embedded-sdmmc/Cargo.toml --target x86_64-unknown-linux-gnu --test contiguous_read --test contiguous_write --no-default-features -- --nocapture
tools/cargo.sh test --manifest-path vendor/embedded-sdmmc/Cargo.toml --target x86_64-unknown-linux-gnu --lib --tests --features contiguous-file-read -- --test-threads=1
tools/cargo.sh test --manifest-path vendor/embedded-sdmmc/Cargo.toml --target x86_64-unknown-linux-gnu --lib --tests --features contiguous-file-read,contiguous-file-write -- --test-threads=1
```

The sparse FAT16/FAT32 tests use real file operations over an instrumented
BlockDevice. Fragmented three-cluster files exercise forward and backward FAT
links, an EOF tail, unaligned caller memory, odd file offsets, backward seeks,
write/read coherence, and retry after a partially overwritten failed transfer.
Measured data read calls for the same sectors:

| Cluster size | Feature off | Feature on | Sectors read |
| --- | ---: | ---: | ---: |
| FAT16, 4 sectors | 12 | 4 | 12 |
| FAT32, 8 sectors | 24 | 4 | 24 |
| FAT16/FAT32, 32 sectors | 96 | 7 | 96 |

The write tests use the same layout with 1-, 4-, 8- and 32-sector clusters:
aligned writes into a new file, unaligned head and tail sectors, growth from a
partial last cluster through fragmented free space, an overwrite across the
file's backward FAT link, cache coherence around batched writes, mixed-size
chunks across close and reopen, and an injected failed transfer followed by a
retry. Each asserts the exact data write calls and reads the bytes back through
a fresh VolumeManager. Without the feature the same scenarios produce identical
bytes from single-sector writes. Measured data write calls for one `write` of
three clusters plus 1 KiB into a new file (FAT updates excluded):

| Cluster size | Feature off | Feature on | Sectors written |
| --- | ---: | ---: | ---: |
| FAT16, 1 sector | 5 | 5 | 5 |
| FAT16, 4 sectors | 14 | 4 | 14 |
| FAT32, 8 sectors | 26 | 4 | 26 |
| FAT16/FAT32, 32 sectors | 98 | 7 | 98 |

Upstream image-based tests decompress a 512 MiB fixture for each test; use one
test thread to keep host memory bounded.
