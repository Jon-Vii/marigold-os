//! FAT read batching against a sparse, instrumented FAT16/FAT32 disk.
//! These exercise the public File interface and the actual BlockDevice calls.
use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Mode, TimeSource, Timestamp, VolumeIdx, VolumeManager,
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

#[derive(Clone)]
struct Disk(Rc<RefCell<State>>);
struct State {
    blocks: BTreeMap<u32, Block>,
    reads: Vec<(u32, usize)>,
    fail_data_read: bool,
    successful_reads_before_failure: usize,
    data_start: u32,
}
impl BlockDevice for Disk {
    type Error = &'static str;
    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), Self::Error> {
        let mut state = self.0.borrow_mut();
        state.reads.push((start.0, blocks.len()));
        if state.fail_data_read && start.0 >= state.data_start {
            if state.successful_reads_before_failure == 0 {
                state.fail_data_read = false;
                // Model a failed transfer that has already overwritten some bytes.
                blocks[0].contents.fill(0xEE);
                return Err("injected data read failure");
            }
            state.successful_reads_before_failure -= 1;
        }
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
            zero_indexed_month: 8,
            zero_indexed_day: 11,
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

fn fixture(fat32: bool, sectors_per_cluster: u8) -> (Disk, Vec<u8>) {
    let reserved = if fat32 { 32u16 } else { 1 };
    let fat_sectors = if fat32 { 600u32 } else { 128 };
    let total_sectors = if fat32 { 75000 } else { 8192 } * u32::from(sectors_per_cluster);
    let root_sectors = if fat32 { 0 } else { 32 };
    let fat_start = 1 + u32::from(reserved);
    let data_start = fat_start + fat_sectors + root_sectors;
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
    bpb[13] = sectors_per_cluster;
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
    let mut fat = Block::new();
    // The last link runs backwards, proving reads follow FAT rather than
    // assuming neighboring clusters are consecutive sectors.
    for (cluster, next) in [
        (0, 0xFFFFFFF8),
        (1, 0xFFFFFFFF),
        (2, 0xFFFFFFFF),
        (3, 9),
        (9, 4),
        (4, 0xFFFFFFFF),
    ] {
        if fat32 {
            u32_at(&mut fat.contents, cluster * 4, next);
        } else {
            u16_at(&mut fat.contents, cluster * 2, next as u16);
        }
    }
    blocks.insert(fat_start, fat);
    let cluster_bytes = usize::from(sectors_per_cluster) * 512;
    let expected: Vec<u8> = (0..cluster_bytes * 3 - 117)
        .map(|i| ((i * 31 + i / 512) % 251) as u8)
        .collect();
    let mut root = Block::new();
    root[..11].copy_from_slice(b"BATCH   DAT");
    root[11] = 0x20;
    u16_at(&mut root.contents, 26, 3);
    u32_at(&mut root.contents, 28, expected.len() as u32);
    blocks.insert(
        if fat32 {
            data_start
        } else {
            fat_start + fat_sectors
        },
        root,
    );
    for (i, cluster) in [3u32, 9, 4].into_iter().enumerate() {
        for sector in 0..u32::from(sectors_per_cluster) {
            let mut block = Block::new();
            let pos = i * cluster_bytes + sector as usize * 512;
            let len = expected.len().saturating_sub(pos).min(512);
            if len > 0 {
                block[..len].copy_from_slice(&expected[pos..pos + len]);
            }
            blocks.insert(
                data_start + (cluster - 2) * u32::from(sectors_per_cluster) + sector,
                block,
            );
        }
    }
    (
        Disk(Rc::new(RefCell::new(State {
            blocks,
            reads: Vec::new(),
            fail_data_read: false,
            successful_reads_before_failure: 0,
            data_start,
        }))),
        expected,
    )
}

#[test]
fn batches_fragmented_fat16_and_fat32_without_reading_past_eof() {
    for (fat32, cluster_sectors) in [(false, 4), (true, 8), (false, 32), (true, 32)] {
        let (disk, expected) = fixture(fat32, cluster_sectors);
        let observer = disk.clone();
        let manager = VolumeManager::new(disk, Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root.open_file_in_dir("BATCH.DAT", Mode::ReadOnly).unwrap();
        observer.0.borrow_mut().reads.clear();
        // Intentionally unaligned RAM address: the direct path requires no
        // stronger alignment than the existing byte-buffer File interface.
        let mut out = vec![0xAA; expected.len() + 515];
        let count = file.read(&mut out[1..]).unwrap();
        assert_eq!(count, expected.len());
        assert_eq!(&out[1..1 + count], expected);
        assert_eq!(out[0], 0xAA);
        assert!(out[1 + count..].iter().all(|&byte| byte == 0xAA));
        let state = observer.0.borrow();
        let data_reads: Vec<_> = state
            .reads
            .iter()
            .filter(|(start, _)| *start >= state.data_start)
            .collect();
        let transactions = data_reads.len();
        let blocks: usize = data_reads.iter().map(|(_, count)| count).sum();
        assert_eq!(blocks, usize::from(cluster_sectors) * 3);
        if cfg!(feature = "contiguous-file-read") {
            let sectors = usize::from(cluster_sectors);
            assert_eq!(
                transactions,
                2 * sectors.div_ceil(16) + (sectors - 1).div_ceil(16) + 1
            );
            assert!(data_reads.iter().all(|(_, count)| *count <= 16));
            assert!(data_reads.iter().any(|(_, count)| *count > 1));
        } else {
            assert_eq!(transactions, blocks);
        }
        eprintln!(
            "FAT{}: {transactions} data BlockDevice reads, {blocks} sectors",
            if fat32 { 32 } else { 16 }
        );
    }
}

#[test]
fn odd_file_offsets_backward_seeks_and_write_then_read_stay_coherent() {
    for (fat32, sectors) in [(false, 4), (true, 8)] {
        let (disk, mut expected) = fixture(fat32, sectors);
        let manager = VolumeManager::new(disk, Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let file = root
            .open_file_in_dir("BATCH.DAT", Mode::ReadWriteAppend)
            .unwrap();
        for (offset, length) in [
            (37, 4500),
            (1024, 1777),
            (511, 8192),
            (0, 4096),
            (512, 1536),
        ] {
            file.seek_from_start(offset).unwrap();
            let mut bytes = vec![0; length];
            let count = file.read(&mut bytes).unwrap();
            assert_eq!(
                &bytes[..count],
                &expected[offset as usize..offset as usize + count]
            );
        }
        file.seek_from_start(517).unwrap();
        file.write(&[0x59; 2300]).unwrap();
        expected[517..2817].fill(0x59);
        file.seek_from_start(0).unwrap();
        let mut bytes = vec![0; expected.len()];
        assert_eq!(file.read(&mut bytes).unwrap(), expected.len());
        assert_eq!(bytes, expected);
    }
}

#[test]
fn failed_data_transfer_can_be_retried_at_the_same_offset() {
    let (disk, expected) = fixture(false, 4);
    let observer = disk.clone();
    let manager = VolumeManager::new(disk, Clock);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let file = root.open_file_in_dir("BATCH.DAT", Mode::ReadOnly).unwrap();
    observer.0.borrow_mut().fail_data_read = true;
    let mut bytes = vec![0; 2048];
    assert!(file.read(&mut bytes).is_err());
    assert_eq!(file.offset(), 0);
    assert_eq!(file.read(&mut bytes).unwrap(), bytes.len());
    assert_eq!(bytes, expected[..2048]);
}

#[test]
fn failed_later_batch_preserves_only_the_successful_prefix() {
    let (disk, expected) = fixture(false, 4);
    let observer = disk.clone();
    let manager = VolumeManager::new(disk, Clock);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let file = root.open_file_in_dir("BATCH.DAT", Mode::ReadOnly).unwrap();
    {
        let mut state = observer.0.borrow_mut();
        state.fail_data_read = true;
        state.successful_reads_before_failure = 1;
    }
    let mut bytes = vec![0; 4096];
    assert!(file.read(&mut bytes).is_err());
    let completed = if cfg!(feature = "contiguous-file-read") {
        2048
    } else {
        512
    };
    assert_eq!(file.offset(), completed);
    assert_eq!(
        &bytes[..completed as usize],
        &expected[..completed as usize]
    );
    file.seek_from_start(0).unwrap();
    assert_eq!(file.read(&mut bytes).unwrap(), bytes.len());
    assert_eq!(bytes, expected[..4096]);
}

#[test]
fn empty_and_eof_reads_do_not_issue_device_transactions() {
    let (disk, _) = fixture(true, 8);
    let observer = disk.clone();
    let manager = VolumeManager::new(disk, Clock);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    let file = root.open_file_in_dir("BATCH.DAT", Mode::ReadOnly).unwrap();
    observer.0.borrow_mut().reads.clear();
    assert_eq!(file.read(&mut []).unwrap(), 0);
    file.seek_from_end(0).unwrap();
    let mut bytes = [0xAA; 1024];
    assert_eq!(file.read(&mut bytes).unwrap(), 0);
    assert!(observer.0.borrow().reads.is_empty());
    assert_eq!(bytes, [0xAA; 1024]);
}

// Directory lookup regressions share the sparse disk adapter so they can
// assert the exact physical reads, not only the eventual NotFound result.
fn put_directory_entry(
    block: &mut Block,
    slot: usize,
    name: &[u8; 11],
    directory: bool,
    cluster: u16,
) {
    let offset = slot * 32;
    block[offset..offset + 11].copy_from_slice(name);
    block[offset + 11] = if directory { 0x10 } else { 0x20 };
    u16_at(&mut block.contents, offset + 26, cluster);
}

fn directory_fixture(fat32: bool, subdirectory: bool) -> (Disk, u32, u32, Option<u32>) {
    let sectors = if fat32 { 8 } else { 4 };
    let (disk, _) = fixture(fat32, sectors);
    let mut state = disk.0.borrow_mut();
    let root_start = if fat32 {
        state.data_start
    } else {
        state.data_start - 32
    };
    let cluster = if subdirectory { 12 } else { 2 };
    let directory_start = if subdirectory {
        state.data_start + (cluster - 2) * u32::from(sectors)
    } else {
        root_start
    };
    let next_start = state.data_start + 15 * u32::from(sectors); // cluster 17
    let mut root = Block::new();
    if subdirectory {
        put_directory_entry(&mut root, 0, b"SUBDIR     ", true, cluster as u16);
    }
    state.blocks.insert(root_start, root);
    let directory_sectors = if !fat32 && !subdirectory {
        32
    } else {
        u32::from(sectors)
    };
    for sector in 0..directory_sectors {
        state.blocks.insert(directory_start + sector, Block::new());
    }
    let next = if fat32 || subdirectory {
        let fat_start = if fat32 { 33 } else { 2 };
        let fat = state.blocks.get_mut(&fat_start).unwrap();
        if fat32 {
            u32_at(&mut fat.contents, cluster as usize * 4, 17);
            u32_at(&mut fat.contents, 17 * 4, 0x0FFFFFFF);
        } else {
            u16_at(&mut fat.contents, cluster as usize * 2, 17);
            u16_at(&mut fat.contents, 17 * 2, 0xFFFF);
        }
        state.blocks.insert(next_start, Block::new());
        Some(next_start)
    } else {
        None
    };
    drop(state);
    (disk, directory_start, directory_sectors, next)
}

#[test]
fn directory_end_marker_stops_lookup_before_unused_sectors_and_clusters() {
    for (fat32, subdirectory) in [(false, false), (false, true), (true, false), (true, true)] {
        for name in ["MISSING.DAT", "LATE.DAT"] {
            let (disk, start, _, next) = directory_fixture(fat32, subdirectory);
            {
                let mut state = disk.0.borrow_mut();
                // Entry zero is 0x00. Junk in this sector, the next sector,
                // and an allocated following cluster must all stay invisible.
                put_directory_entry(
                    state.blocks.get_mut(&start).unwrap(),
                    1,
                    b"LATE    DAT",
                    false,
                    3,
                );
                put_directory_entry(
                    state.blocks.get_mut(&(start + 1)).unwrap(),
                    0,
                    b"LATE    DAT",
                    false,
                    3,
                );
                if let Some(next) = next {
                    put_directory_entry(
                        state.blocks.get_mut(&next).unwrap(),
                        0,
                        b"LATE    DAT",
                        false,
                        3,
                    );
                }
            }
            let observer = disk.clone();
            let manager = VolumeManager::new(disk, Clock);
            let volume = manager.open_volume(VolumeIdx(0)).unwrap();
            let root = volume.open_root_dir().unwrap();
            let child = if subdirectory {
                Some(root.open_dir("SUBDIR").unwrap())
            } else {
                None
            };
            let directory = child.as_ref().unwrap_or(&root);
            observer.0.borrow_mut().reads.clear();
            assert!(matches!(
                directory.find_directory_entry(name),
                Err(embedded_sdmmc::Error::NotFound)
            ));
            assert_eq!(
                observer.0.borrow().reads.as_slice(),
                &[(start, 1)],
                "lookup must stop after one sector: FAT32={fat32} subdirectory={subdirectory}"
            );
        }
    }
}

#[test]
fn directory_deleted_entries_are_skipped_and_full_sectors_continue() {
    for (fat32, subdirectory) in [(false, false), (false, true), (true, false), (true, true)] {
        for full_first_sector in [false, true] {
            let (disk, start, _, _) = directory_fixture(fat32, subdirectory);
            let expected_sector = start + u32::from(full_first_sector);
            {
                let mut state = disk.0.borrow_mut();
                let first = state.blocks.get_mut(&start).unwrap();
                first[0] = 0xE5; // deleted is not the end marker
                if full_first_sector {
                    for slot in 1..16 {
                        put_directory_entry(first, slot, b"OTHER   DAT", false, 3);
                    }
                }
                let slot = if full_first_sector { 0 } else { 1 };
                put_directory_entry(
                    state.blocks.get_mut(&expected_sector).unwrap(),
                    slot,
                    b"LATE    DAT",
                    false,
                    3,
                );
            }
            let observer = disk.clone();
            let manager = VolumeManager::new(disk, Clock);
            let volume = manager.open_volume(VolumeIdx(0)).unwrap();
            let root = volume.open_root_dir().unwrap();
            let child = if subdirectory {
                Some(root.open_dir("SUBDIR").unwrap())
            } else {
                None
            };
            let directory = child.as_ref().unwrap_or(&root);
            observer.0.borrow_mut().reads.clear();
            assert!(directory.find_directory_entry("LATE.DAT").is_ok());
            assert_eq!(
                observer.0.borrow().reads.len(),
                if full_first_sector { 2 } else { 1 }
            );
        }
    }
}

#[test]
fn full_directory_clusters_still_follow_the_fat_chain() {
    for (fat32, subdirectory) in [(false, true), (true, false), (true, true)] {
        let (disk, start, sectors, next) = directory_fixture(fat32, subdirectory);
        let next = next.unwrap();
        {
            let mut state = disk.0.borrow_mut();
            for sector in start..start + sectors {
                let block = state.blocks.get_mut(&sector).unwrap();
                for slot in 0..16 {
                    put_directory_entry(block, slot, b"OTHER   DAT", false, 3);
                }
            }
            put_directory_entry(
                state.blocks.get_mut(&next).unwrap(),
                0,
                b"LATE    DAT",
                false,
                3,
            );
        }
        let observer = disk.clone();
        let manager = VolumeManager::new(disk, Clock);
        let volume = manager.open_volume(VolumeIdx(0)).unwrap();
        let root = volume.open_root_dir().unwrap();
        let child = if subdirectory {
            Some(root.open_dir("SUBDIR").unwrap())
        } else {
            None
        };
        let directory = child.as_ref().unwrap_or(&root);
        observer.0.borrow_mut().reads.clear();
        assert!(directory.find_directory_entry("LATE.DAT").is_ok());
        let state = observer.0.borrow();
        assert_eq!(state.reads.len(), sectors as usize + 2); // first cluster, FAT link, next cluster
        assert_eq!(state.reads.last(), Some(&(next, 1)));
    }
}
