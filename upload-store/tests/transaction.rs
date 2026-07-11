//! End-to-end fault-injection tests for the upload transaction state
//! machine, run against a real embedded-sdmmc FAT16 filesystem on an
//! in-memory block device that can fail the Nth read or write on demand.
//!
//! Every scenario asserts the transaction's core promise: no failure —
//! mid-stream write error, client abort, sidecar failure, or a fault
//! during cleanup — ever costs the previously valid copy of a book.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, Mode, TimeSource, Timestamp, VolumeIdx,
    VolumeManager,
};
use proto::upload::UploadName;
use upload_store::PendingUpload;

const BLOCK_BYTES: usize = 512;
/// 16 MiB card: big enough that fatfs picks FAT16 and small enough to stay fast.
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

// ---------------------------------------------------------------------------
// Fault-injecting in-memory block device
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskError;

/// Arms exactly-once faults: `fail_write_in` = Some(n) fails the (n+1)th
/// subsequent write call, then disarms. Same for reads. Exactly-once matters:
/// cleanup paths issue their own I/O after a fault, and a sticky fault would
/// conflate "the write failed" with "the whole card died".
#[derive(Default)]
struct FaultPlan {
    fail_write_in: Cell<Option<u32>>,
    fail_read_in: Cell<Option<u32>>,
}

impl FaultPlan {
    fn take_fault(counter: &Cell<Option<u32>>) -> bool {
        match counter.get() {
            Some(0) => {
                counter.set(None);
                true
            }
            Some(n) => {
                counter.set(Some(n - 1));
                false
            }
            None => false,
        }
    }
}

struct FaultyDisk {
    data: RefCell<Vec<u8>>,
    fault: FaultPlan,
    /// Total write calls, for measuring how many writes an operation costs
    /// so a fault can be aimed past it (see `close_write_cost`).
    writes: Cell<u32>,
}

/// The tests hold one `Rc` handle for arming faults and inspecting raw bytes
/// while the `VolumeManager` owns another. The newtype exists because the
/// orphan rule forbids implementing the foreign `BlockDevice` for `Rc<_>`.
#[derive(Clone)]
struct SharedDisk(Rc<FaultyDisk>);

impl std::ops::Deref for SharedDisk {
    type Target = FaultyDisk;
    fn deref(&self) -> &FaultyDisk {
        &self.0
    }
}

impl BlockDevice for SharedDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        if FaultPlan::take_fault(&self.fault.fail_read_in) {
            return Err(DiskError);
        }
        let data = self.data.borrow();
        for (i, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            block.copy_from_slice(&data[at..at + BLOCK_BYTES]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        self.writes.set(self.writes.get() + 1);
        if FaultPlan::take_fault(&self.fault.fail_write_in) {
            return Err(DiskError);
        }
        let mut data = self.data.borrow_mut();
        for (i, block) in blocks.iter().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            data[at..at + BLOCK_BYTES].copy_from_slice(&block[..]);
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, DiskError> {
        Ok(BlockCount(DISK_BLOCKS))
    }
}

// ---------------------------------------------------------------------------
// FAT16 image: MBR partition table + fatfs-formatted partition
// ---------------------------------------------------------------------------

fn format_disk() -> Vec<u8> {
    let mut disk = vec![0u8; DISK_BLOCKS as usize * BLOCK_BYTES];
    let part_blocks = DISK_BLOCKS - PART_START_BLOCK;

    let mut partition = vec![0u8; part_blocks as usize * BLOCK_BYTES];
    fatfs::format_volume(
        std::io::Cursor::new(partition.as_mut_slice()),
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat16),
    )
    .expect("format FAT16 partition");
    disk[PART_START_BLOCK as usize * BLOCK_BYTES..].copy_from_slice(&partition);

    // One MBR entry: status 0, type 0x06 (FAT16), LBA start/count.
    // embedded-sdmmc ignores the CHS fields.
    let entry = 446;
    disk[entry] = 0x00;
    disk[entry + 4] = 0x06;
    disk[entry + 8..entry + 12].copy_from_slice(&PART_START_BLOCK.to_le_bytes());
    disk[entry + 12..entry + 16].copy_from_slice(&part_blocks.to_le_bytes());
    disk[510] = 0x55;
    disk[511] = 0xAA;
    disk
}

struct StaticTime;

impl TimeSource for StaticTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 4,
            zero_indexed_day: 19,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Harness: fresh VolumeManager per phase, like a session per power cycle
// ---------------------------------------------------------------------------

type Mgr = VolumeManager<SharedDisk, StaticTime, 8, 8, 1>;
type Dir<'a> = Directory<'a, SharedDisk, StaticTime, 8, 8, 1>;

fn new_card() -> SharedDisk {
    SharedDisk(Rc::new(FaultyDisk {
        data: RefCell::new(format_disk()),
        fault: FaultPlan::default(),
        writes: Cell::new(0),
    }))
}

fn open_mgr(disk: &SharedDisk) -> Mgr {
    VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000)
}

fn open_dirs(mgr: &Mgr) -> (Dir<'_>, Dir<'_>) {
    let volume = mgr.open_volume(VolumeIdx(0)).expect("open volume");
    let raw_volume = volume.to_raw_volume();
    let raw_root = mgr.open_root_dir(raw_volume).expect("open root");
    let root = Directory::new(raw_root, mgr);
    if root.open_dir("BOOKS").is_err() {
        root.make_dir_in_dir("BOOKS").expect("make BOOKS");
    }
    // Build BOOKS from the raw layer so it borrows the manager, not `root`
    // (Directory::open_dir would tie its lifetime to the parent handle).
    let raw_root = mgr.open_root_dir(raw_volume).expect("reopen root");
    let raw_books = mgr.open_dir(raw_root, "BOOKS").expect("open BOOKS");
    mgr.close_dir(raw_root).expect("close spare root handle");
    let books = Directory::new(raw_books, mgr);
    (root, books)
}

fn name(text: &str) -> UploadName {
    let mut name = UploadName::new();
    name.push_str(text).expect("name fits 8.3");
    name
}

/// One complete successful upload: begin, stream, close, commit.
fn upload(
    root: &Dir<'_>,
    books: &Dir<'_>,
    begin_name: &str,
    identity: u64,
    body: &[u8],
) -> UploadName {
    let pending = PendingUpload::begin(root, books, &name(begin_name), identity, "label.epub")
        .expect("begin upload");
    pending.write(body).expect("write body");
    pending.commit(root, books).expect("commit upload")
}

fn read_book(books: &Dir<'_>, book_name: &str) -> Vec<u8> {
    let file = books
        .open_file_in_dir(book_name, Mode::ReadOnly)
        .expect("book exists");
    let mut out = vec![0u8; file.length() as usize];
    let mut at = 0;
    while at < out.len() {
        let read = file.read(&mut out[at..]).expect("read book");
        assert!(read > 0, "short read");
        at += read;
    }
    out
}

fn book_names(books: &Dir<'_>) -> Vec<String> {
    let mut names = Vec::new();
    books
        .iterate_dir(|entry| {
            let entry_name = format!("{}", entry.name);
            if !entry.attributes.is_directory() && entry_name != "." && entry_name != ".." {
                names.push(entry_name);
            }
        })
        .expect("iterate BOOKS");
    names.sort();
    names
}

fn identity_of(root: &Dir<'_>, book_name: &str) -> Option<u64> {
    upload_store::read_upload_identity(root, book_name).expect("readable identity")
}

/// How many device write calls a File::close costs in the re-upload
/// scenario, measured on a scratch card so retire-time faults can be aimed
/// past commit's internal close instead of hardcoding a block count.
fn close_write_cost() -> u32 {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"old body");
    // A different identity, so the probe finds nothing to retire and
    // commit's writes are exactly the internal close's writes.
    let pending = PendingUpload::begin(&root, &books, &name("BOOK0000.EPU"), 0xBBBB, "label.epub")
        .expect("begin");
    pending.write(b"new body").expect("stream");
    let before = disk.writes.get();
    assert!(pending.commit(&root, &books).is_some());
    disk.writes.get() - before
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[test]
fn fresh_upload_lands_in_natural_slot_with_sidecars() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);

    let landed = upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"first body");

    assert_eq!(landed.as_str(), "BOOK0000.EPU");
    assert_eq!(book_names(&books), vec!["BOOK0000.EPU"]);
    assert_eq!(read_book(&books, "BOOK0000.EPU"), b"first body");
    assert_eq!(identity_of(&root, "BOOK0000.EPU"), Some(0xAAAA));
    let mut label = heapless::String::<64>::new();
    assert!(upload_store::read_upload_label(
        &root,
        "BOOK0000.EPU",
        &mut label
    ));
    assert_eq!(label.as_str(), "label.epub");
}

#[test]
fn reupload_replaces_only_after_success_and_old_survives_mid_stream() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"old body");

    let pending = PendingUpload::begin(&root, &books, &name("BOOK0000.EPU"), 0xAAAA, "label.epub")
        .expect("begin re-upload");
    // Mid-stream: the new data goes to a different slot and the old copy is
    // still whole on the card.
    assert_ne!(pending.target().as_str(), "BOOK0000.EPU");
    pending
        .write(b"new body, longer than before")
        .expect("stream");
    assert_eq!(read_book(&books, "BOOK0000.EPU"), b"old body");
    let landed = pending.commit(&root, &books).expect("commit");

    // Committed: exactly one copy, the new one, identity intact.
    assert_eq!(book_names(&books), vec![landed.as_str().to_string()]);
    assert_eq!(
        read_book(&books, landed.as_str()),
        b"new body, longer than before"
    );
    assert_eq!(identity_of(&root, landed.as_str()), Some(0xAAAA));
    assert_eq!(identity_of(&root, "BOOK0000.EPU"), None);
}

#[test]
fn client_abort_preserves_the_old_copy_and_leaves_no_debris() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"old body");

    let pending = PendingUpload::begin(&root, &books, &name("BOOK0000.EPU"), 0xAAAA, "label.epub")
        .expect("begin re-upload");
    pending.write(b"partial ne").expect("partial stream");
    pending.abort(&root, &books);

    assert_eq!(book_names(&books), vec!["BOOK0000.EPU"]);
    assert_eq!(read_book(&books, "BOOK0000.EPU"), b"old body");
    assert_eq!(identity_of(&root, "BOOK0000.EPU"), Some(0xAAAA));
    // The aborted slot left neither book bytes nor sidecars behind.
    assert_eq!(identity_of(&root, "BOOK0001.EPU"), None);
}

#[test]
fn sd_write_fault_mid_stream_preserves_the_old_copy() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"old body");

    let pending = PendingUpload::begin(&root, &books, &name("BOOK0000.EPU"), 0xAAAA, "label.epub")
        .expect("begin re-upload");
    // Stream enough that writes hit the device, then fail the next one —
    // the same shape as an SD card dropping out mid-body.
    let big = vec![0x5A_u8; 4096];
    pending.write(&big).expect("first chunk");
    disk.fault.fail_write_in.set(Some(0));
    assert!(
        pending.write(&big).is_err(),
        "the injected fault must surface"
    );
    pending.abort(&root, &books);

    // Reopen fresh (as after a power cycle): only the old copy remains.
    drop((root, books));
    drop(mgr);
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    assert_eq!(book_names(&books), vec!["BOOK0000.EPU"]);
    assert_eq!(read_book(&books, "BOOK0000.EPU"), b"old body");
    assert_eq!(identity_of(&root, "BOOK0000.EPU"), Some(0xAAAA));
}

#[test]
fn sidecar_write_fault_during_begin_stages_nothing() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"old body");

    // Fail the very next device write: the identity sidecar staging.
    disk.fault.fail_write_in.set(Some(0));
    let begun = PendingUpload::begin(&root, &books, &name("BOOK0000.EPU"), 0xAAAA, "label.epub");
    assert!(begun.is_err(), "begin must refuse after a sidecar fault");

    assert_eq!(book_names(&books), vec!["BOOK0000.EPU"]);
    assert_eq!(read_book(&books, "BOOK0000.EPU"), b"old body");
    assert_eq!(identity_of(&root, "BOOK0000.EPU"), Some(0xAAAA));
    // No sidecars remain staged for the slot that never got its book.
    assert_eq!(identity_of(&root, "BOOK0001.EPU"), None);
}

#[test]
fn probe_read_fault_aborts_before_touching_anything() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"old body");
    let before = disk.data.borrow().clone();

    disk.fault.fail_read_in.set(Some(0));
    let begun = PendingUpload::begin(&root, &books, &name("BOOK0000.EPU"), 0xAAAA, "label.epub");
    assert!(begun.is_err(), "begin must refuse after a probe fault");
    assert_eq!(*disk.data.borrow(), before, "no block was written");
}

#[test]
fn retire_fault_during_commit_keeps_the_old_copy_identity_matched() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"old body");

    let pending = PendingUpload::begin(&root, &books, &name("BOOK0000.EPU"), 0xAAAA, "label.epub")
        .expect("begin re-upload");
    pending.write(b"new body").expect("stream");
    // Fail the delete of the replaced copy inside commit, skipping the
    // write that commit's internal close performs first (measured by
    // close_write_cost below).
    disk.fault.fail_write_in.set(Some(close_write_cost()));
    let landed = pending.commit(&root, &books).expect("commit");
    let landed = landed.as_str().to_string();

    // Assert against the device, not this session: embedded-sdmmc's block
    // cache keeps the mutated directory block after a failed write-back, so
    // only a fresh manager (i.e. the next power cycle) sees the card's truth.
    drop((root, books));
    drop(mgr);
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);

    // Both copies exist, and crucially the old one kept its identity
    // sidecar, so it is still recognized as a replaceable prior copy.
    assert_eq!(
        book_names(&books),
        vec!["BOOK0000.EPU".to_string(), landed.clone()]
    );
    assert_eq!(identity_of(&root, "BOOK0000.EPU"), Some(0xAAAA));
    assert_eq!(identity_of(&root, landed.as_str()), Some(0xAAAA));

    // The next re-upload converges back to a single copy.
    let landed = upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"third body");
    assert_eq!(book_names(&books), vec![landed.as_str().to_string()]);
    assert_eq!(read_book(&books, landed.as_str()), b"third body");
}

#[test]
fn replacement_matches_identity_across_a_hole_in_the_chain() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);

    // A foreign file (no identity sidecar) occupies the natural slot, so the
    // first upload lands one slot down the chain.
    books
        .open_file_in_dir("BOOK0000.EPU", Mode::ReadWriteCreateOrTruncate)
        .expect("create blocker")
        .close()
        .expect("close blocker");
    let landed = upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"body one");
    assert_eq!(landed.as_str(), "BOOK0001.EPU");

    // The blocker disappears, leaving a hole before the identity match.
    books
        .delete_file_in_dir("BOOK0000.EPU")
        .expect("remove blocker");

    // The re-upload must find the match past the hole and still replace it.
    let landed = upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"body two");
    assert_eq!(landed.as_str(), "BOOK0000.EPU");
    assert_eq!(book_names(&books), vec!["BOOK0000.EPU"]);
    assert_eq!(read_book(&books, "BOOK0000.EPU"), b"body two");
}

#[test]
fn commit_retires_every_duplicate_left_by_earlier_interruptions() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);

    // Two identity-matched copies, as a mid-replace power loss leaves behind.
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"copy one");
    books
        .open_file_in_dir("BOOK0001.EPU", Mode::ReadWriteCreateOrTruncate)
        .expect("create duplicate")
        .close()
        .expect("close duplicate");
    assert!(upload_store::write_upload_identity(
        &root,
        "BOOK0001.EPU",
        0xAAAA
    ));

    let landed = upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"fresh body");

    assert_eq!(landed.as_str(), "BOOK0002.EPU");
    assert_eq!(book_names(&books), vec!["BOOK0002.EPU"]);
    assert_eq!(read_book(&books, "BOOK0002.EPU"), b"fresh body");
    assert_eq!(identity_of(&root, "BOOK0000.EPU"), None);
    assert_eq!(identity_of(&root, "BOOK0001.EPU"), None);
}

#[test]
fn malformed_identity_sidecar_is_skipped_not_fatal() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"old body");

    // Truncate the identity sidecar to a bad length. A malformed sidecar
    // never heals, so the probe must treat the book as a different file
    // rather than abort the window forever.
    let xteink = root.open_dir("XTEINK").expect("open XTEINK");
    let labels = xteink.open_dir("LABELS").expect("open LABELS");
    let sidecar = labels
        .open_file_in_dir("BOOK0000.ID", Mode::ReadWriteCreateOrTruncate)
        .expect("reopen sidecar");
    sidecar.write(&[0xDE, 0xAD, 0xBE, 0xEF]).expect("corrupt");
    sidecar.close().expect("close sidecar");
    assert_eq!(identity_of(&root, "BOOK0000.EPU"), None);

    let pending = PendingUpload::begin(&root, &books, &name("BOOK0000.EPU"), 0xAAAA, "label.epub")
        .expect("begin must proceed past the malformed sidecar");
    assert_eq!(pending.skipped_malformed_sidecars(), 1);
    pending.write(b"new body").expect("stream");
    let landed = pending.commit(&root, &books).expect("commit");

    // The unmatched old copy survives as a visible duplicate — the accepted
    // worst case — and the new copy carries a valid identity.
    assert_eq!(landed.as_str(), "BOOK0001.EPU");
    assert_eq!(
        book_names(&books),
        vec!["BOOK0000.EPU".to_string(), "BOOK0001.EPU".to_string()]
    );
    assert_eq!(read_book(&books, "BOOK0000.EPU"), b"old body");
    assert_eq!(read_book(&books, "BOOK0001.EPU"), b"new body");
    assert_eq!(identity_of(&root, "BOOK0001.EPU"), Some(0xAAAA));
}

#[test]
fn close_fault_during_commit_discards_target_and_keeps_old() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"old body");

    let pending = PendingUpload::begin(&root, &books, &name("BOOK0000.EPU"), 0xAAAA, "label.epub")
        .expect("begin re-upload");
    let target_name = pending.target().clone();
    pending.write(b"new body").expect("stream");
    // Fail commit's internal close: the new copy's metadata never lands,
    // so commit must discard the target and leave the old copy alone.
    disk.fault.fail_write_in.set(Some(0));
    assert!(
        pending.commit(&root, &books).is_none(),
        "commit must report failure when the close fails"
    );

    // Reopen fresh (as after a power cycle): only the old copy remains.
    drop((root, books));
    drop(mgr);
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    assert_eq!(book_names(&books), vec!["BOOK0000.EPU"]);
    assert_eq!(read_book(&books, "BOOK0000.EPU"), b"old body");
    assert_eq!(identity_of(&root, "BOOK0000.EPU"), Some(0xAAAA));
    // The discarded target's sidecars must also be gone.
    assert_eq!(
        identity_of(&root, target_name.as_str()),
        None,
        "discarded target's identity sidecar must be absent"
    );
    let mut label = heapless::String::<64>::new();
    assert!(
        !upload_store::read_upload_label(&root, target_name.as_str(), &mut label),
        "discarded target's label sidecar must be absent"
    );
}

#[test]
fn invalid_upload_names_are_rejected_without_touching_the_card() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let before = disk.data.borrow().clone();

    // "ABC\u{e9}000.EPU" is 12 valid UTF-8 bytes with a character straddling
    // byte offset 4: slicing by byte would panic without validation. The
    // rest cover a lowercase prefix, a non-base-36 tail, and a wrong
    // extension, each of which would probe an unrelated slot chain.
    for bad in [
        "ABC\u{e9}000.EPU",
        "book0000.EPU",
        "BOOK00x0.EPU",
        "BOOK0000.TXT",
    ] {
        // Arm a read fault: if begin touches the device at all, the fault
        // fires and is consumed. An unconsumed fault proves rejection was
        // purely syntactic.
        disk.fault.fail_read_in.set(Some(0));
        assert!(
            PendingUpload::begin(&root, &books, &name(bad), 0xAAAA, "label.epub").is_err(),
            "{bad:?} must be rejected"
        );
        assert!(
            disk.fault.fail_read_in.get().is_some(),
            "{bad:?} must be rejected without performing any device read"
        );
        disk.fault.fail_read_in.set(None);
    }
    assert_eq!(*disk.data.borrow(), before, "no block was written");
}

#[test]
fn different_identities_with_the_same_name_chain_coexist() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);

    // Same 4-char prefix and colliding tail, different logical files.
    upload(&root, &books, "BOOK0000.EPU", 0xAAAA, b"book a");
    let landed = upload(&root, &books, "BOOK0000.EPU", 0xBBBB, b"book b");

    assert_eq!(landed.as_str(), "BOOK0001.EPU");
    assert_eq!(book_names(&books), vec!["BOOK0000.EPU", "BOOK0001.EPU"]);
    assert_eq!(read_book(&books, "BOOK0000.EPU"), b"book a");
    assert_eq!(read_book(&books, "BOOK0001.EPU"), b"book b");
}
