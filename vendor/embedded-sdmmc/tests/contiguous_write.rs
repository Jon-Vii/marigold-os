//! FAT write batching against a sparse, instrumented FAT16/FAT32 disk.
//!
//! These drive the public File interface and record every BlockDevice call,
//! so they pin both the bytes that reach the disk and the shape of the data
//! writes. Built without `contiguous-file-write`, the same scenarios check
//! that the bytes are identical and every data write is a single sector.
use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Mode, TimeSource, Timestamp, VolumeIdx, VolumeManager,
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

const BATCHING: bool = cfg!(feature = "contiguous-file-write");

/// (FAT32, sectors per cluster). One-sector clusters can never batch.
const CONFIGS: [(bool, u32); 5] = [(false, 1), (false, 4), (true, 8), (false, 32), (true, 32)];

/// Clusters of the existing BATCH.DAT. The last link runs backwards, so
/// writes must follow the FAT rather than assume neighbouring clusters.
const BATCH_CHAIN: [u32; 3] = [3, 9, 4];

#[derive(Clone)]
struct Disk(Rc<RefCell<State>>);

struct State {
    blocks: BTreeMap<u32, Block>,
    reads: Vec<(u32, usize)>,
    writes: Vec<(u32, usize)>,
    /// Data-area writes to let through before one injected failure.
    fail_data_write_after: Option<usize>,
    data_start: u32,
}

impl BlockDevice for Disk {
    type Error = &'static str;
    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Self::Error> {
        let mut state = self.0.borrow_mut();
        state.reads.push((start.0, blocks.len()));
        for (i, block) in blocks.iter_mut().enumerate() {
            *block = state
                .blocks
                .get(&(start.0 + i as u32))
                .cloned()
                .unwrap_or_default();
        }
        Ok(())
    }
    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), Self::Error> {
        let mut state = self.0.borrow_mut();
        state.writes.push((start.0, blocks.len()));
        if start.0 >= state.data_start {
            match state.fail_data_write_after {
                Some(0) => {
                    state.fail_data_write_after = None;
                    // Model a transfer that programmed its first sector and
                    // then failed, as a CMD25 rejected mid-stream would.
                    state.blocks.insert(start.0, blocks[0].clone());
                    return Err("injected data write failure");
                }
                Some(left) => state.fail_data_write_after = Some(left - 1),
                None => {}
            }
        }
        for (i, block) in blocks.iter().enumerate() {
            state.blocks.insert(start.0 + i as u32, block.clone());
        }
        Ok(())
    }
    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        Ok(BlockCount(4000001))
    }
}

struct Clock;
impl TimeSource for Clock {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 9,
            zero_indexed_day: 22,
            hours: 12,
            minutes: 0,
            seconds: 0,
        }
    }
}

fn u16_at(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn u32_at(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    fat32: bool,
    spc: u32,
    fat_start: u32,
    root_start: u32,
    data_start: u32,
}

impl Geometry {
    fn cluster_bytes(self) -> usize {
        self.spc as usize * Block::LEN
    }
    fn block(self, cluster: u32, sector: u32) -> u32 {
        self.data_start + (cluster - 2) * self.spc + sector
    }
    fn cluster_of(self, block: u32) -> u32 {
        (block - self.data_start) / self.spc + 2
    }
    /// Past the FAT32 root directory (cluster 2): only file data lives here.
    fn file_area(self) -> u32 {
        self.block(3, 0)
    }
    fn batch_len(self) -> usize {
        self.cluster_bytes() * 3 - 117
    }
}

/// A sparse FAT16/FAT32 volume with one file, BATCH.DAT, on clusters 3, 9, 4
/// (its last cluster partly used), and `also_used` clusters taken by others.
fn fixture(fat32: bool, spc: u32, also_used: &[u32]) -> (Disk, Geometry, Vec<u8>) {
    let reserved = if fat32 { 32u16 } else { 1 };
    let fat_sectors = if fat32 { 600u32 } else { 128 };
    let total_sectors = if fat32 { 75000 } else { 8192 } * spc;
    let root_sectors = if fat32 { 0 } else { 32 };
    let fat_start = 1 + u32::from(reserved);
    let data_start = fat_start + fat_sectors + root_sectors;
    let geometry = Geometry {
        fat32,
        spc,
        fat_start,
        root_start: if fat32 {
            data_start
        } else {
            fat_start + fat_sectors
        },
        data_start,
    };
    let mut blocks = BTreeMap::new();
    let mut mbr = Block::new();
    mbr[450] = if fat32 { 0x0C } else { 0x0E };
    u32_at(&mut mbr.contents, 454, 1);
    u32_at(&mut mbr.contents, 458, total_sectors);
    mbr[510..512].copy_from_slice(&[0x55, 0xAA]);
    blocks.insert(0, mbr);
    let mut bpb = Block::new();
    bpb[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    u16_at(&mut bpb.contents, 11, 512);
    bpb[13] = spc as u8;
    u16_at(&mut bpb.contents, 14, reserved);
    bpb[16] = 1;
    bpb[21] = 0xF8;
    u32_at(&mut bpb.contents, 32, total_sectors);
    if fat32 {
        u32_at(&mut bpb.contents, 36, fat_sectors);
        u32_at(&mut bpb.contents, 44, 2);
        u16_at(&mut bpb.contents, 48, 1);
        bpb[66] = 0x29;
        bpb[82..90].copy_from_slice(b"FAT32   ");
        let mut fsinfo = Block::new();
        u32_at(&mut fsinfo.contents, 0, 0x41615252);
        u32_at(&mut fsinfo.contents, 484, 0x61417272);
        u32_at(&mut fsinfo.contents, 488, 0xFFFFFFFF);
        u32_at(&mut fsinfo.contents, 492, 0xFFFFFFFF);
        u32_at(&mut fsinfo.contents, 508, 0xAA550000);
        blocks.insert(2, fsinfo);
    } else {
        u16_at(&mut bpb.contents, 17, 512);
        u16_at(&mut bpb.contents, 22, fat_sectors as u16);
        bpb[38] = 0x29;
        bpb[54..62].copy_from_slice(b"FAT16   ");
    }
    bpb[510..512].copy_from_slice(&[0x55, 0xAA]);
    blocks.insert(1, bpb);
    let eof = 0x0FFF_FFFF;
    let mut links = vec![
        (0, 0x0FFF_FFF8),
        (1, eof),
        (2, eof),
        (3, 9),
        (9, 4),
        (4, eof),
    ];
    links.extend(also_used.iter().map(|&cluster| (cluster, eof)));
    let mut fat = Block::new();
    for (cluster, next) in links {
        if fat32 {
            u32_at(&mut fat.contents, cluster as usize * 4, next);
        } else {
            u16_at(&mut fat.contents, cluster as usize * 2, next as u16);
        }
    }
    blocks.insert(fat_start, fat);
    let contents = pattern(geometry.batch_len(), 1);
    let mut root = Block::new();
    root[..11].copy_from_slice(b"BATCH   DAT");
    root[11] = 0x20;
    u16_at(&mut root.contents, 26, BATCH_CHAIN[0] as u16);
    u32_at(&mut root.contents, 28, contents.len() as u32);
    blocks.insert(geometry.root_start, root);
    let cluster_bytes = geometry.cluster_bytes();
    for (i, cluster) in BATCH_CHAIN.into_iter().enumerate() {
        for sector in 0..spc {
            let mut block = Block::new();
            let pos = i * cluster_bytes + sector as usize * Block::LEN;
            let len = contents.len().saturating_sub(pos).min(Block::LEN);
            block[..len].copy_from_slice(&contents[pos..pos + len]);
            blocks.insert(geometry.block(cluster, sector), block);
        }
    }
    let disk = Disk(Rc::new(RefCell::new(State {
        blocks,
        reads: Vec::new(),
        writes: Vec::new(),
        fail_data_write_after: None,
        data_start,
    })));
    (disk, geometry, contents)
}

fn pattern(len: usize, seed: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 7 + seed * 101 + i / 512) % 253) as u8)
        .collect()
}

/// Apply a write to the expected file contents, growing them if needed.
fn overwrite(expected: &mut Vec<u8>, offset: usize, data: &[u8]) {
    if expected.len() < offset + data.len() {
        expected.resize(offset + data.len(), 0);
    }
    expected[offset..offset + data.len()].copy_from_slice(data);
}

/// The data-area writes one `File::write` should issue over `chain`: partial
/// sectors one at a time through the cache, and whole sectors in batches of
/// at most 16 that never leave the cluster (or one at a time when off).
fn expected_writes(g: Geometry, chain: &[u32], offset: usize, len: usize) -> Vec<(u32, usize)> {
    let cluster_bytes = g.cluster_bytes();
    let (mut pos, end) = (offset, offset + len);
    let mut writes = Vec::new();
    while pos < end {
        let sector = (pos % cluster_bytes) / Block::LEN;
        let block = g.block(chain[pos / cluster_bytes], sector as u32);
        let whole = if pos % Block::LEN == 0 {
            ((end - pos) / Block::LEN)
                .min(g.spc as usize - sector)
                .min(16)
        } else {
            0
        };
        if BATCHING && whole > 1 {
            writes.push((block, whole));
            pos += whole * Block::LEN;
        } else {
            writes.push((block, 1));
            pos = ((pos / Block::LEN + 1) * Block::LEN).min(end);
        }
    }
    writes
}

/// Drain the recorded data-area writes.
fn take_file_writes(disk: &Disk, g: Geometry) -> Vec<(u32, usize)> {
    let mut state = disk.0.borrow_mut();
    let writes = state.writes.drain(..);
    writes
        .filter(|(start, _)| *start >= g.file_area())
        .collect()
}

fn assert_within_clusters(g: Geometry, writes: &[(u32, usize)]) {
    for &(start, count) in writes {
        assert!((1..=16).contains(&count), "write of {count} sectors");
        assert_eq!(
            g.cluster_of(start),
            g.cluster_of(start + count as u32 - 1),
            "write at {start} of {count} sectors crosses a cluster end"
        );
    }
}

/// The FAT chain from `first`, read straight from the disk image.
fn chain(disk: &Disk, g: Geometry, first: u32) -> Vec<u32> {
    let state = disk.0.borrow();
    let entry = |cluster: u32| {
        let (bytes, eof) = if g.fat32 {
            (4, 0x0FFF_FFF8)
        } else {
            (2, 0xFFF8)
        };
        let offset = cluster * bytes;
        let block = state
            .blocks
            .get(&(g.fat_start + offset / 512))
            .cloned()
            .unwrap_or_default();
        let at = (offset % 512) as usize;
        let value = if g.fat32 {
            u32::from_le_bytes(block[at..at + 4].try_into().unwrap()) & 0x0FFF_FFFF
        } else {
            u32::from(u16::from_le_bytes(block[at..at + 2].try_into().unwrap()))
        };
        (value < eof).then_some(value)
    };
    let mut clusters = vec![first];
    while let Some(next) = entry(*clusters.last().unwrap()) {
        assert!(next >= 2, "chain runs into a free or reserved cluster");
        clusters.push(next);
        assert!(clusters.len() < 10_000, "FAT loop");
    }
    clusters
}

/// The first cluster of a root directory entry, read from the disk image.
fn first_cluster(disk: &Disk, g: Geometry, name: &[u8; 11]) -> u32 {
    let state = disk.0.borrow();
    let root = &state.blocks[&g.root_start];
    let entry = root
        .chunks_exact(32)
        .find(|entry| &entry[..11] == name)
        .expect("directory entry");
    u32::from(u16::from_le_bytes([entry[26], entry[27]]))
        | u32::from(u16::from_le_bytes([entry[20], entry[21]])) << 16
}

/// Free clusters in the order the allocator hands them out on this fixture.
fn allocation_order(also_used: &[u32], count: usize) -> Vec<u32> {
    (5..)
        .filter(|cluster| !BATCH_CHAIN.contains(cluster) && !also_used.contains(cluster))
        .take(count)
        .collect()
}

/// Read a whole file back through a fresh VolumeManager (no shared cache).
fn read_back(disk: &Disk, name: &str) -> Vec<u8> {
    let manager = VolumeManager::new(disk.clone(), Clock);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let file = root.open_file_in_dir(name, Mode::ReadOnly).unwrap();
    let mut bytes = vec![0; file.length() as usize];
    assert_eq!(file.read(&mut bytes).unwrap(), bytes.len());
    bytes
}

#[test]
fn aligned_writes_to_a_new_file_batch_inside_each_cluster() {
    for (fat32, spc) in CONFIGS {
        let (disk, g, _) = fixture(fat32, spc, &[]);
        let manager = VolumeManager::new(disk.clone(), Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root
            .open_file_in_dir("NEW.DAT", Mode::ReadWriteCreate)
            .unwrap();
        let cluster_bytes = g.cluster_bytes();
        let data = pattern(cluster_bytes * 3 + 1024, 2);
        // Intentionally unaligned caller memory: the direct path borrows the
        // byte buffer as Blocks, which have one-byte alignment.
        let mut storage = vec![0xAA; data.len() + 1];
        storage[1..].copy_from_slice(&data);
        disk.0.borrow_mut().writes.clear();
        file.write(&storage[1..]).unwrap();
        let all_writes = disk.0.borrow().writes.clone();
        let writes = take_file_writes(&disk, g);
        assert_eq!(file.offset() as usize, data.len());
        assert_eq!(file.length() as usize, data.len());
        file.close().unwrap();

        let clusters = data.len().div_ceil(cluster_bytes);
        let expected_chain = allocation_order(&[], clusters);
        let first = first_cluster(&disk, g, b"NEW     DAT");
        assert_eq!(chain(&disk, g, first), expected_chain);
        assert_eq!(
            writes,
            expected_writes(g, &expected_chain, 0, data.len()),
            "FAT32={fat32} spc={spc}"
        );
        assert_within_clusters(g, &writes);
        let sectors: usize = writes.iter().map(|(_, count)| count).sum();
        assert_eq!(sectors, data.len() / Block::LEN);
        // Anchor the model with literal call lists, FAT updates included.
        let b = |cluster, sector| g.block(cluster, sector);
        let fat = g.fat_start;
        match (fat32, spc, BATCHING) {
            (false, 4, true) => assert_eq!(
                all_writes,
                [
                    (fat, 1),
                    (b(5, 0), 4),
                    (fat, 1),
                    (fat, 1),
                    (b(6, 0), 4),
                    (fat, 1),
                    (fat, 1),
                    (b(7, 0), 4),
                    (fat, 1),
                    (fat, 1),
                    (b(8, 0), 2),
                ]
            ),
            (false, 32, true) => assert_eq!(
                writes,
                [
                    (b(5, 0), 16),
                    (b(5, 16), 16),
                    (b(6, 0), 16),
                    (b(6, 16), 16),
                    (b(7, 0), 16),
                    (b(7, 16), 16),
                    (b(8, 0), 2),
                ]
            ),
            (_, _, false) => assert!(writes.iter().all(|&(_, count)| count == 1)),
            _ => {}
        }
        eprintln!(
            "FAT{} {spc}-sector clusters: {} data writes for {sectors} sectors",
            if fat32 { 32 } else { 16 },
            writes.len()
        );
        assert_eq!(read_back(&disk, "NEW.DAT"), data);
    }
}

#[test]
fn unaligned_head_and_tail_sectors_use_the_cache() {
    for (fat32, spc) in CONFIGS {
        let (disk, g, mut expected) = fixture(fat32, spc, &[]);
        let manager = VolumeManager::new(disk.clone(), Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root
            .open_file_in_dir("BATCH.DAT", Mode::ReadWriteAppend)
            .unwrap();
        let (offset, len) = (100, g.cluster_bytes() * 2);
        let data = pattern(len, 3);
        file.seek_from_start(offset as u32).unwrap();
        {
            let mut state = disk.0.borrow_mut();
            state.writes.clear();
            state.reads.clear();
        }
        file.write(&data).unwrap();
        let writes = take_file_writes(&disk, g);
        assert_eq!(writes, expected_writes(g, &BATCH_CHAIN, offset, len));
        assert_within_clusters(g, &writes);
        // Only the two partial sectors are read, to merge with old bytes.
        let reads: Vec<_> = disk
            .0
            .borrow()
            .reads
            .iter()
            .copied()
            .filter(|(start, _)| *start >= g.file_area())
            .collect();
        let end = offset + len;
        let tail_cluster = BATCH_CHAIN[end / g.cluster_bytes()];
        let tail_sector = (end % g.cluster_bytes() / Block::LEN) as u32;
        assert_eq!(
            reads,
            [
                (g.block(BATCH_CHAIN[0], 0), 1),
                (g.block(tail_cluster, tail_sector), 1)
            ]
        );
        assert_eq!(file.offset() as usize, end);
        assert_eq!(file.length() as usize, expected.len());
        file.close().unwrap();
        expected[offset..end].copy_from_slice(&data);
        assert_eq!(read_back(&disk, "BATCH.DAT"), expected);
    }
}

#[test]
fn growth_from_a_partial_cluster_follows_and_extends_a_fragmented_chain() {
    // Cluster 6 belongs to someone else, so the file grows 4 -> 5 -> 7 -> 8.
    let also_used = [6];
    for (fat32, spc) in CONFIGS {
        let (disk, g, mut expected) = fixture(fat32, spc, &also_used);
        let manager = VolumeManager::new(disk.clone(), Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root
            .open_file_in_dir("BATCH.DAT", Mode::ReadWriteAppend)
            .unwrap();
        let offset = expected.len();
        assert_eq!(file.offset() as usize, offset);
        let len = 117 + g.cluster_bytes() * 2 + 1000;
        let data = pattern(len, 4);
        disk.0.borrow_mut().writes.clear();
        file.write(&data).unwrap();
        let writes = take_file_writes(&disk, g);
        assert_eq!(file.offset() as usize, offset + len);
        assert_eq!(file.length() as usize, offset + len);
        file.close().unwrap();

        let clusters = (offset + len).div_ceil(g.cluster_bytes());
        let mut expected_chain = BATCH_CHAIN.to_vec();
        expected_chain.extend(allocation_order(&also_used, clusters - 3));
        assert_eq!(chain(&disk, g, BATCH_CHAIN[0]), expected_chain);
        assert_eq!(writes, expected_writes(g, &expected_chain, offset, len));
        assert_within_clusters(g, &writes);
        expected.extend_from_slice(&data);
        assert_eq!(read_back(&disk, "BATCH.DAT"), expected);
    }
}

#[test]
fn overwriting_the_middle_keeps_neighbours_and_length() {
    for (fat32, spc) in CONFIGS {
        let (disk, g, mut expected) = fixture(fat32, spc, &[]);
        let manager = VolumeManager::new(disk.clone(), Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root
            .open_file_in_dir("BATCH.DAT", Mode::ReadWriteAppend)
            .unwrap();
        // From the middle of cluster 9 across its backward link into 4.
        let offset = g.cluster_bytes() + Block::LEN * (spc as usize / 2);
        let len = g.cluster_bytes() + 37;
        let data = pattern(len, 5);
        file.seek_from_start(offset as u32).unwrap();
        disk.0.borrow_mut().writes.clear();
        file.write(&data).unwrap();
        let writes = take_file_writes(&disk, g);
        assert_eq!(writes, expected_writes(g, &BATCH_CHAIN, offset, len));
        assert_within_clusters(g, &writes);
        assert_eq!(file.offset() as usize, offset + len);
        assert_eq!(file.length() as usize, expected.len());
        // Read back through the same manager (and its cache) first.
        expected[offset..offset + len].copy_from_slice(&data);
        file.seek_from_start(0).unwrap();
        let mut bytes = vec![0; expected.len()];
        assert_eq!(file.read(&mut bytes).unwrap(), expected.len());
        assert_eq!(bytes, expected);
        file.close().unwrap();
        assert_eq!(chain(&disk, g, BATCH_CHAIN[0]), BATCH_CHAIN);
        assert_eq!(read_back(&disk, "BATCH.DAT"), expected);
    }
}

#[test]
fn batched_writes_never_leave_a_stale_cached_sector() {
    for (fat32, spc) in CONFIGS {
        let (disk, g, mut expected) = fixture(fat32, spc, &[]);
        let manager = VolumeManager::new(disk.clone(), Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root
            .open_file_in_dir("BATCH.DAT", Mode::ReadWriteAppend)
            .unwrap();
        let mut probe = [0u8; 10];
        // 1. A small read leaves sector 2 in the single-sector cache.
        file.seek_from_start(1024).unwrap();
        assert_eq!(file.read(&mut probe).unwrap(), probe.len());
        assert_eq!(probe, expected[1024..1034]);
        // A batch over sectors 0..4 writes that sector behind the cache.
        let first = pattern(4 * Block::LEN, 6);
        file.seek_from_start(0).unwrap();
        disk.0.borrow_mut().writes.clear();
        file.write(&first).unwrap();
        if spc >= 4 {
            // One direct transfer when batching; four cached ones when not.
            let writes = take_file_writes(&disk, g);
            assert_eq!(writes, expected_writes(g, &BATCH_CHAIN, 0, first.len()));
            assert_eq!(writes.len(), if BATCHING { 1 } else { 4 });
        }
        overwrite(&mut expected, 0, &first);
        file.seek_from_start(1024).unwrap();
        assert_eq!(file.read(&mut probe).unwrap(), probe.len());
        assert_eq!(probe, expected[1024..1034]);

        // 2. A partial write leaves sector 1, modified, in the cache.
        let partial = pattern(100, 7);
        file.seek_from_start(522).unwrap();
        file.write(&partial).unwrap();
        overwrite(&mut expected, 522, &partial);
        // Another batch rewrites that sector directly.
        let second = pattern(4 * Block::LEN, 8);
        file.seek_from_start(0).unwrap();
        file.write(&second).unwrap();
        overwrite(&mut expected, 0, &second);
        let mut bytes = [0u8; 100];
        file.seek_from_start(522).unwrap();
        assert_eq!(file.read(&mut bytes).unwrap(), bytes.len());
        assert_eq!(bytes, expected[522..622]);

        file.seek_from_start(0).unwrap();
        let mut all = vec![0; expected.len()];
        assert_eq!(file.read(&mut all).unwrap(), all.len());
        assert_eq!(all, expected);
        file.close().unwrap();
        assert_eq!(read_back(&disk, "BATCH.DAT"), expected);
    }
}

#[test]
fn mixed_chunks_survive_close_and_reopen() {
    for (fat32, spc) in CONFIGS {
        let (disk, g, _) = fixture(fat32, spc, &[]);
        let manager = VolumeManager::new(disk.clone(), Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root
            .open_file_in_dir("MIXED.DAT", Mode::ReadWriteCreate)
            .unwrap();
        let chunks = [700, 4096, 3000, 1, 9000, 512, 1024, 8192 + 512];
        let data = pattern(chunks.iter().sum(), 9);
        let mut ranges = Vec::new();
        let mut offset = 0;
        disk.0.borrow_mut().writes.clear();
        for len in chunks {
            file.write(&data[offset..offset + len]).unwrap();
            ranges.push((offset, len));
            offset += len;
            assert_eq!(file.offset() as usize, offset);
            assert_eq!(file.length() as usize, offset);
        }
        let writes = take_file_writes(&disk, g);
        file.close().unwrap();
        drop(root);
        drop(volume);
        drop(manager);

        let clusters = data.len().div_ceil(g.cluster_bytes());
        let expected_chain = allocation_order(&[], clusters);
        let first = first_cluster(&disk, g, b"MIXED   DAT");
        assert_eq!(chain(&disk, g, first), expected_chain);
        let expected: Vec<_> = ranges
            .iter()
            .flat_map(|&(offset, len)| expected_writes(g, &expected_chain, offset, len))
            .collect();
        assert_eq!(writes, expected);
        assert_within_clusters(g, &writes);

        // Reopen for append and add one more aligned tail, then reopen again.
        let manager = VolumeManager::new(disk.clone(), Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root
            .open_file_in_dir("MIXED.DAT", Mode::ReadWriteAppend)
            .unwrap();
        assert_eq!(file.length() as usize, data.len());
        let tail = pattern(3 * Block::LEN, 10);
        file.write(&tail).unwrap();
        file.close().unwrap();
        let mut expected = data.clone();
        expected.extend_from_slice(&tail);
        assert_eq!(read_back(&disk, "MIXED.DAT"), expected);
    }
}

#[test]
fn failed_write_leaves_position_and_length_at_the_failed_transfer() {
    for (fat32, spc) in CONFIGS {
        let (disk, g, _) = fixture(fat32, spc, &[]);
        let manager = VolumeManager::new(disk.clone(), Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root
            .open_file_in_dir("FAIL.DAT", Mode::ReadWriteCreate)
            .unwrap();
        let prefix = pattern(1024, 11);
        file.write(&prefix).unwrap();
        let len = g.cluster_bytes() * 2;
        let data = pattern(len, 12);
        let expected_chain = allocation_order(&[], (1024 + len).div_ceil(g.cluster_bytes()));
        let planned = expected_writes(g, &expected_chain, 1024, len);
        // Let the first data transfer through, fail the second.
        disk.0.borrow_mut().fail_data_write_after = Some(1);
        assert!(file.write(&data).is_err());
        let completed = 1024 + planned[0].1 * Block::LEN;
        assert_eq!(file.offset() as usize, completed, "FAT32={fat32} spc={spc}");
        assert_eq!(file.length() as usize, completed);
        assert_eq!(
            completed,
            if BATCHING && spc > 2 {
                1024 + (spc as usize - 2).min(16) * Block::LEN
            } else {
                1536
            }
        );
        // Retry the whole failed write from where it started.
        file.seek_from_start(1024).unwrap();
        file.write(&data).unwrap();
        assert_eq!(file.offset() as usize, 1024 + len);
        assert_eq!(file.length() as usize, 1024 + len);
        file.close().unwrap();
        // The cluster allocated before the failure is reused, not leaked.
        let first = first_cluster(&disk, g, b"FAIL    DAT");
        assert_eq!(chain(&disk, g, first), expected_chain);
        let mut expected = prefix.clone();
        expected.extend_from_slice(&data);
        assert_eq!(read_back(&disk, "FAIL.DAT"), expected);
    }
}
