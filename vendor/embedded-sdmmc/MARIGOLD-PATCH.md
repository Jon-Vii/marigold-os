# Marigold local FAT patches

Source: `https://github.com/Jon-Vii/embedded-sdmmc-rs.git`, commit
`329b3a53a55fb527f62a7813557b420817e831d8` (version 0.9.0).
The source, upstream tests, README, and license notices are vendored from that
exact revision. The SD initialization and VFAT write implementations remain unchanged; no
upstream driver upgrade is involved. The existing CMD18 path also gains a narrow
error cleanup: always attempt CMD12 after a data transfer error, preserving the
original error if stopping fails too. A scripted SPI test covers successful
transfers, errors before/after a good block, and failed stop commands.

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
This lookup correction is independent of the experimental batching feature.

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

This is an experiment until matched hardware measurements justify enabling it.
Host transaction counts are not hardware throughput claims. Keep feature-off
builds as the baseline and verify both X4/X3 stack links, cold/warm book reads,
writes, and sleep/wake before enabling it in release builds.

## Verification

```
cargo test --manifest-path vendor/embedded-sdmmc/Cargo.toml --target aarch64-apple-darwin --test contiguous_read --no-default-features -- --nocapture
cargo test --manifest-path vendor/embedded-sdmmc/Cargo.toml --target aarch64-apple-darwin --lib --tests --features contiguous-file-read -- --test-threads=1
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

Upstream image-based tests decompress a 512 MiB fixture for each test; use one
test thread to keep host memory bounded.
