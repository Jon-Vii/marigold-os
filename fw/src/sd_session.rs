use crate::display_flush::Epd;
use crate::upload::{UploadBegin, UploadChunk, UploadName};
use crate::{UPLOAD_BEGINS, UPLOAD_CHUNKS, UPLOAD_RESULTS, UPLOAD_RETURNS};
use core::sync::atomic::{AtomicU8, Ordering};
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::{Operation, SpiBus as BlockingSpiBus, SpiDevice};
use embedded_sdmmc::sdcard::CardType;
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use embedded_sdmmc::{Directory, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use esp_hal::gpio::Output;
use esp_hal::spi::master::{Config as SpiConfig, SpiDmaBus};
use esp_hal::time::Rate;
use esp_hal::Async;

/// SD SPI-mode identification must run at 100-400 kHz; data transfer is
/// specced to 25 MHz. The shared bus otherwise runs at the active panel's
/// clock, which on the X4 (SSD1677, 40 MHz) is out of SD spec entirely and
/// what the read-retry machinery in the EPUB path was quietly absorbing.
const SD_IDENT_FREQ_KHZ: u32 = 400;
const SD_DATA_FREQ_MHZ: u32 = 25;
/// Restore frequency after SD access: the active panel's SPI clock. This
/// MUST be per-panel — the UC8253 (X3) can't decode above ~20 MHz, so
/// restoring the X4's 40 MHz leaves the panel deaf to every subsequent
/// command (init included, since the boot catalog read precedes it).
const DISPLAY_FREQ_HZ: u32 = display::epd::SPI_HZ;

/// Block-level SD transaction counters for `bench:` telemetry. Single-writer
/// (all SD traffic runs on the storage/display task), so plain load+store is
/// enough on this RV32IMC core — no RMW atomics needed. Read via `snapshot`
/// deltas around a workload; never reset, so concurrent snapshots stay
/// comparable.
pub(crate) mod sd_stats {
    use core::sync::atomic::{AtomicU32, Ordering};

    pub(crate) static READ_CALLS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static READ_BLOCKS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static WRITE_CALLS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static WRITE_BLOCKS: AtomicU32 = AtomicU32::new(0);

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Snapshot {
        pub(crate) read_calls: u32,
        pub(crate) read_blocks: u32,
        pub(crate) write_calls: u32,
        pub(crate) write_blocks: u32,
    }

    pub(crate) fn snapshot() -> Snapshot {
        Snapshot {
            read_calls: READ_CALLS.load(Ordering::Relaxed),
            read_blocks: READ_BLOCKS.load(Ordering::Relaxed),
            write_calls: WRITE_CALLS.load(Ordering::Relaxed),
            write_blocks: WRITE_BLOCKS.load(Ordering::Relaxed),
        }
    }

    impl Snapshot {
        pub(crate) fn since(self, start: Snapshot) -> Snapshot {
            Snapshot {
                read_calls: self.read_calls.wrapping_sub(start.read_calls),
                read_blocks: self.read_blocks.wrapping_sub(start.read_blocks),
                write_calls: self.write_calls.wrapping_sub(start.write_calls),
                write_blocks: self.write_blocks.wrapping_sub(start.write_blocks),
            }
        }
    }

    pub(crate) fn bump(counter: &AtomicU32, amount: u32) {
        let value = counter.load(Ordering::Relaxed).wrapping_add(amount);
        counter.store(value, Ordering::Relaxed);
    }
}

/// Counts physical block transactions on their way to the SD card, so bench
/// telemetry can report exact CMD17/CMD24-level traffic per workload.
pub(crate) struct CountingDevice<B>(B);

impl<B: BlockDevice> BlockDevice for CountingDevice<B> {
    type Error = B::Error;

    fn read(&self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        sd_stats::bump(&sd_stats::READ_CALLS, 1);
        sd_stats::bump(&sd_stats::READ_BLOCKS, blocks.len() as u32);
        self.0.read(blocks, start_block_idx)
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        sd_stats::bump(&sd_stats::WRITE_CALLS, 1);
        sd_stats::bump(&sd_stats::WRITE_BLOCKS, blocks.len() as u32);
        self.0.write(blocks, start_block_idx)
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        self.0.num_blocks()
    }
}

pub(crate) struct StaticTime;

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

#[derive(Clone, Copy)]
pub(crate) struct SdDelay;

impl DelayNs for SdDelay {
    fn delay_ns(&mut self, ns: u32) {
        sd_spi_pace(ns.saturating_div(100).max(1));
    }
}

pub(crate) struct SdSpiDevice<'a, SPI, CS> {
    pub(crate) spi: &'a mut SPI,
    pub(crate) cs: &'a mut CS,
    pub(crate) delay: SdDelay,
}

/// Also sizes the shared bus's RX DMA buffer in main.rs: SD traffic is the
/// only read path on SPI2 (the EPD is write-only), and every SD operation
/// bounces through one of these chunks. Sized to one SD block so a 512-B
/// data read/write is a single DMA transaction instead of eight; the
/// extra ~448 B of DRAM comes out of the stack headroom build.rs asserts.
pub(crate) const SD_SPI_CHUNK_BYTES: usize = 512;

#[repr(align(4))]
struct AlignedSdChunk([u8; SD_SPI_CHUNK_BYTES]);

/// The one bounce chunk all SD SPI operations share. Static rather than a
/// local so the 512-B block never lands on the reader's deep-call stack
/// (the 27 KB link-time budget in build.rs is nearly spent); as .bss it is
/// instead counted against the stack-headroom ASSERT. Sound for the same
/// reason `sd_stats` uses plain load/store: every SD transaction runs on
/// the storage/display task, and the borrows below never overlap.
struct SdBounce {
    chunk: core::cell::UnsafeCell<AlignedSdChunk>,
    /// A shared static cannot rule out overlapping borrows at compile
    /// time, so this flag turns any overlap (including reentrancy from
    /// the callback) into a panic instead of aliased `&mut`s.
    busy: portable_atomic::AtomicBool,
}
// Safety: only the single SD-owning task touches the chunk (see above),
// and `with_sd_bounce` panics on overlapping access.
#[allow(unsafe_code)]
unsafe impl Sync for SdBounce {}
static SD_BOUNCE: SdBounce = SdBounce {
    chunk: core::cell::UnsafeCell::new(AlignedSdChunk([0xFF; SD_SPI_CHUNK_BYTES])),
    busy: portable_atomic::AtomicBool::new(false),
};

/// Runs `f` with exclusive access to the shared bounce chunk, refilled
/// with the 0xFF idle pattern SD cards expect on MOSI during reads. The
/// closure signature keeps the borrow from escaping.
#[allow(unsafe_code)]
fn with_sd_bounce<R>(f: impl FnOnce(&mut AlignedSdChunk) -> R) -> R {
    use portable_atomic::Ordering;
    if SD_BOUNCE.busy.swap(true, Ordering::Acquire) {
        panic!("sd bounce chunk borrowed twice");
    }
    // Safety: the busy flag above makes this the only live borrow, and
    // it cannot outlive `f`.
    let chunk = unsafe { &mut *SD_BOUNCE.chunk.get() };
    chunk.0.fill(0xFF);
    let result = f(chunk);
    SD_BOUNCE.busy.store(false, Ordering::Release);
    result
}

fn sd_spi_pace(iterations: u32) {
    for _ in 0..iterations {
        core::hint::spin_loop();
    }
}

impl<SPI, CS> embedded_hal::spi::ErrorType for SdSpiDevice<'_, SPI, CS>
where
    SPI: embedded_hal::spi::ErrorType,
{
    type Error = SPI::Error;
}

impl<SPI, CS> SpiDevice for SdSpiDevice<'_, SPI, CS>
where
    SPI: BlockingSpiBus<u8>,
    CS: OutputPin,
{
    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
        let _ = self.cs.set_low();
        let mut result = Ok(());

        for operation in operations {
            result = match operation {
                Operation::Read(buffer) => self.read_with_sd_clocks(buffer),
                Operation::Write(buffer) => self.write_chunked(buffer),
                Operation::Transfer(read, write) => self.transfer_chunked(read, write),
                Operation::TransferInPlace(buffer) => self.transfer_in_place_chunked(buffer),
                Operation::DelayNs(ns) => {
                    self.delay.delay_ns(*ns);
                    Ok(())
                }
            };

            if result.is_err() {
                break;
            }
        }

        let _ = self.spi.flush();
        let _ = self.cs.set_high();
        result
    }
}

impl<SPI, CS> SdSpiDevice<'_, SPI, CS>
where
    SPI: BlockingSpiBus<u8>,
{
    fn read_with_sd_clocks(&mut self, buffer: &mut [u8]) -> Result<(), SPI::Error> {
        for chunk in buffer.chunks_mut(SD_SPI_CHUNK_BYTES) {
            with_sd_bounce(|bounce| {
                self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])?;
                chunk.copy_from_slice(&bounce.0[..chunk.len()]);
                Ok(())
            })?;
        }
        Ok(())
    }

    fn write_chunked(&mut self, buffer: &[u8]) -> Result<(), SPI::Error> {
        for chunk in buffer.chunks(SD_SPI_CHUNK_BYTES) {
            with_sd_bounce(|bounce| {
                bounce.0[..chunk.len()].copy_from_slice(chunk);
                self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])
            })?;
        }
        Ok(())
    }

    fn transfer_chunked(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), SPI::Error> {
        let common = read.len().min(write.len());
        let (read_common, read_tail) = read.split_at_mut(common);
        let (write_common, write_tail) = write.split_at(common);

        for (read_chunk, write_chunk) in read_common
            .chunks_mut(SD_SPI_CHUNK_BYTES)
            .zip(write_common.chunks(SD_SPI_CHUNK_BYTES))
        {
            with_sd_bounce(|bounce| {
                bounce.0[..write_chunk.len()].copy_from_slice(write_chunk);
                self.spi
                    .transfer_in_place(&mut bounce.0[..write_chunk.len()])?;
                read_chunk.copy_from_slice(&bounce.0[..read_chunk.len()]);
                Ok(())
            })?;
        }
        if !read_tail.is_empty() {
            self.read_with_sd_clocks(read_tail)?;
        }
        if !write_tail.is_empty() {
            self.write_chunked(write_tail)?;
        }
        Ok(())
    }

    fn transfer_in_place_chunked(&mut self, buffer: &mut [u8]) -> Result<(), SPI::Error> {
        for chunk in buffer.chunks_mut(SD_SPI_CHUNK_BYTES) {
            with_sd_bounce(|bounce| {
                bounce.0[..chunk.len()].copy_from_slice(chunk);
                self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])?;
                chunk.copy_from_slice(&bounce.0[..chunk.len()]);
                Ok(())
            })?;
        }
        Ok(())
    }
}

type SdSpi<'a> = SdSpiDevice<'a, SpiDmaBus<'static, Async>, Output<'static>>;

type SdCardDevice<'a> = CountingDevice<SdCard<SdSpi<'a>, SdDelay>>;
pub(crate) type SdRoot<'a> = Directory<'a, SdCardDevice<'a>, StaticTime, 8, 8, 1>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SdSessionError {
    CardInit,
    Volume,
    Root,
}

/// Each per-page-turn SD access shares the SPI bus with the panel, so the
/// card is re-acquired every call. A *cold* acquire runs the SD SPI init
/// handshake (CMD0/ACMD41), specced to take up to hundreds of milliseconds
/// — far more than the section read itself. While the device stays awake
/// the card never loses power or its SPI-mode init, so after the first
/// successful acquire we remember its `CardType` and skip the handshake on
/// later sessions via `mark_card_as_init`. Deep sleep resets the chip and
/// clears this static, forcing one honest cold acquire on wake. A warm
/// acquire that can't open the volume falls back to a cold one, so a wrong
/// guess is at worst as slow as before — never a failed read.
const WARM_CARD_NONE: u8 = 0;
static WARM_CARD_CODE: AtomicU8 = AtomicU8::new(WARM_CARD_NONE);

fn remembered_card_type() -> Option<CardType> {
    match WARM_CARD_CODE.load(Ordering::Relaxed) {
        1 => Some(CardType::SD1),
        2 => Some(CardType::SD2),
        3 => Some(CardType::SDHC),
        _ => None,
    }
}

fn remember_card_type(card_type: CardType) {
    let code = match card_type {
        CardType::SD1 => 1,
        CardType::SD2 => 2,
        CardType::SDHC => 3,
    };
    WARM_CARD_CODE.store(code, Ordering::Relaxed);
}

fn forget_card_warmth() {
    WARM_CARD_CODE.store(WARM_CARD_NONE, Ordering::Relaxed);
}

/// Kept out of line: the VolumeManager/SdCard session state is multi-KB
/// and must not be pooled into every caller's frame.
#[inline(never)]
pub(crate) fn with_root<R>(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    f: impl for<'a> FnOnce(&SdRoot<'a>) -> R,
) -> Result<R, SdSessionError> {
    epd.deselect_display();
    sd_cs.set_high();
    esp_println::println!("sd: session enter");

    // The callback is consumed only once the root dir is open, so a warm
    // acquire that bails before then leaves it intact for the cold retry.
    let mut pending = Some(f);
    let mut result = Err(SdSessionError::CardInit);
    if let Some(card_type) = remembered_card_type() {
        result = run_sd_session(epd, sd_cs, Some(card_type), &mut pending);
        if result.is_err() {
            esp_println::println!("sd: warm reuse failed, cold retry");
            forget_card_warmth();
        }
    }
    if pending.is_some() {
        result = run_sd_session(epd, sd_cs, None, &mut pending);
    }

    esp_println::println!("sd: session exit");
    sd_cs.set_high();
    let _ = epd
        .spi_mut()
        .apply_config(&SpiConfig::default().with_frequency(Rate::from_hz(DISPLAY_FREQ_HZ)));
    result
}

/// One SD acquire + open-root + callback. `assume_init` skips the init
/// handshake for a card known to still be warm. The callback is taken from
/// `f` only after the root dir opens, so any earlier failure returns with
/// `f` untouched for the caller to retry cold.
#[allow(unsafe_code)]
#[inline(never)]
fn run_sd_session<R, F>(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    assume_init: Option<CardType>,
    f: &mut Option<F>,
) -> Result<R, SdSessionError>
where
    F: for<'a> FnOnce(&SdRoot<'a>) -> R,
{
    // Identification phase: 400 kHz with at least 74 wake clocks while no
    // chip select is asserted, per the SD spec and embedded-sdmmc's docs.
    // Harmless for an already-initialised card (it ignores the bus while
    // deselected), so the warm path runs it too rather than special-casing.
    {
        let spi = epd.spi_mut();
        let _ = spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_khz(SD_IDENT_FREQ_KHZ)));
        let mut wake = [0xFFu8; 10];
        let _ = BlockingSpiBus::transfer_in_place(spi, &mut wake);
        let _ = BlockingSpiBus::flush(spi);
    }

    let spi = SdSpiDevice {
        spi: epd.spi_mut(),
        cs: sd_cs,
        delay: SdDelay,
    };
    let card = SdCard::new(spi, SdDelay);
    match assume_init {
        Some(card_type) => {
            // SAFETY: the card has stayed powered and in SPI mode since the
            // cold acquire that recorded this type; reads below skip
            // re-init because the type is already set. A stale guess
            // surfaces as an open_volume failure and a cold retry, so this
            // never reads with the wrong addressing mode silently.
            unsafe { card.mark_card_as_init(card_type) };
        }
        None => {
            esp_println::println!("sd: card probe");
            if card.num_bytes().is_err() {
                return Err(SdSessionError::CardInit);
            }
            esp_println::println!("sd: card ready");
            if let Some(card_type) = card.get_card_type() {
                remember_card_type(card_type);
            }
        }
    }

    // Card acquired: switch to the in-spec data rate for the rest of the
    // session.
    card.spi(|device| {
        let _ = device
            .spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_mhz(SD_DATA_FREQ_MHZ)));
    });
    let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(CountingDevice(card), StaticTime, 5000);
    // Bind the outcome so the open_volume scrutinee temporary (which borrows
    // volume_mgr) is dropped at the `;` while volume_mgr is still alive,
    // rather than racing volume_mgr's own drop at the function tail.
    let result = match volume_mgr.open_volume(VolumeIdx(0)) {
        Ok(volume) => {
            esp_println::println!("sd: volume open");
            let raw_volume = volume.to_raw_volume();
            if let Ok(raw_root) = volume_mgr.open_root_dir(raw_volume) {
                esp_println::println!("sd: root open");
                let root = Directory::new(raw_root, &volume_mgr);
                let callback = f.take().expect("sd session callback present");
                let value = callback(&root);
                esp_println::println!("sd: root callback done");
                drop(root);
                let _ = volume_mgr.close_volume(raw_volume);
                Ok(value)
            } else {
                let _ = volume_mgr.close_volume(raw_volume);
                Err(SdSessionError::Root)
            }
        }
        Err(_) => Err(SdSessionError::Volume),
    };
    result
}

/// The upload phase: one SD session held open for the rest of the sync
/// session, writing browser-sent books to /BOOKS as they stream in.
/// Diverges by design — only the session-ending reset leaves it, so the
/// display task must not be needed for anything else once this starts.
pub(crate) async fn upload_session(epd: &mut Epd, sd_cs: &mut Output<'static>) -> ! {
    epd.deselect_display();
    sd_cs.set_high();
    esp_println::println!("upload: session enter");

    {
        let spi = epd.spi_mut();
        let _ = spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_khz(SD_IDENT_FREQ_KHZ)));
        let mut wake = [0xFFu8; 10];
        let _ = BlockingSpiBus::transfer_in_place(spi, &mut wake);
        let _ = BlockingSpiBus::flush(spi);
    }

    let spi = SdSpiDevice {
        spi: epd.spi_mut(),
        cs: sd_cs,
        delay: SdDelay,
    };
    let card = SdCard::new(spi, SdDelay);
    if card.num_bytes().is_err() {
        esp_println::println!("upload: card init failed");
        refuse_uploads_forever().await;
    }
    card.spi(|device| {
        let _ = device
            .spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_mhz(SD_DATA_FREQ_MHZ)));
    });
    let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(CountingDevice(card), StaticTime, 5000);
    let Ok(volume) = volume_mgr.open_volume(VolumeIdx(0)) else {
        esp_println::println!("upload: volume open failed");
        refuse_uploads_forever().await;
    };
    let raw_volume = volume.to_raw_volume();
    let Ok(raw_root) = volume_mgr.open_root_dir(raw_volume) else {
        esp_println::println!("upload: root open failed");
        refuse_uploads_forever().await;
    };
    let root = Directory::new(raw_root, &volume_mgr);
    // New books invalidate the catalog snapshot: the next boot's cache
    // load misses and runs a full scan, which is how uploads surface.
    if let Ok(xteink) = root.open_dir("XTEINK") {
        let _ = xteink.delete_file_in_dir("CATALOG.BIN");
        esp_println::println!("upload: catalog snapshot invalidated");
    }
    let books = match root.open_dir("BOOKS") {
        Ok(books) => books,
        Err(_) => match root.make_dir_in_dir("BOOKS") {
            Ok(()) => match root.open_dir("BOOKS") {
                Ok(books) => books,
                Err(_) => refuse_uploads_forever().await,
            },
            Err(_) => refuse_uploads_forever().await,
        },
    };

    loop {
        let begin = UPLOAD_BEGINS.receive().await;
        let ok = if begin.delete {
            let removed = if begin.in_books {
                books.delete_file_in_dir(begin.name.as_str()).is_ok()
            } else {
                root.delete_file_in_dir(begin.name.as_str()).is_ok()
            };
            if removed && begin.in_books {
                upload_store::delete_upload_sidecars(&root, begin.name.as_str());
            }
            removed
        } else {
            write_one_book(&root, &books, &begin).await.is_some()
        };
        esp_println::println!(
            "upload: '{}' {} ok={}",
            begin.name,
            if begin.delete { "delete" } else { "write" },
            ok
        );
        UPLOAD_RESULTS.send(ok).await;
    }
}

async fn write_one_book<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    books: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    begin: &UploadBegin,
) -> Option<UploadName>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // The probe/sidecar/replace state machine lives in upload-store (where
    // the host fault-injection tests exercise it); this shell owns only the
    // chunk streaming between begin and commit/abort.
    let begun = upload_store::PendingUpload::begin(
        root,
        books,
        &begin.name,
        begin.identity_hash,
        begin.label.as_str(),
    );
    let Ok(pending) = begun else {
        drain_until_end().await;
        return None;
    };
    let malformed = pending.skipped_malformed_sidecars();
    if malformed > 0 {
        esp_println::println!(
            "upload: {} malformed identity sidecar(s) treated as absent",
            malformed
        );
    }
    let mut failed = false;
    let mut aborted = false;
    loop {
        let chunk = UPLOAD_CHUNKS.receive().await;
        if !failed && !chunk.abort {
            if let Some(buffer) = &chunk.buffer {
                // One blocking whole-chunk write, on purpose. Pacing this
                // as 512-B slices with a yield between them (to keep
                // net_task fed under the theory that the 10-30 ms write
                // starves TCP) was tried and measured on hardware
                // 2026-07-11: it cost ~1 s per 3.2 MB upload and bought
                // nothing — TCP rides out the stall via buffering. Don't
                // reintroduce pacing without a timed upload A/B.
                if pending
                    .write(&buffer[..chunk.len.min(buffer.len())])
                    .is_err()
                {
                    failed = true;
                }
            }
        }
        let last = chunk.last;
        aborted |= chunk.abort;
        recycle(chunk).await;
        if last || aborted {
            break;
        }
    }
    if failed || aborted {
        pending.abort(root, books);
        return None;
    }
    // commit closes the file and retires the replaced copies only if the
    // close succeeded; a failed close discards the target and returns None.
    pending.commit(root, books)
}

/// Consumes one file's worth of chunks without a file to write into.
async fn drain_until_end() {
    loop {
        let chunk = UPLOAD_CHUNKS.receive().await;
        let done = chunk.last || chunk.abort;
        recycle(chunk).await;
        if done {
            return;
        }
    }
}

async fn recycle(chunk: UploadChunk) {
    if let Some(buffer) = chunk.buffer {
        UPLOAD_RETURNS.send(buffer).await;
    }
}

/// Setup failed: answer every upload attempt with failure, forever; the
/// session ends with the reset like everything else in sync mode.
async fn refuse_uploads_forever() -> ! {
    loop {
        let _ = UPLOAD_BEGINS.receive().await;
        drain_until_end().await;
        UPLOAD_RESULTS.send(false).await;
    }
}
