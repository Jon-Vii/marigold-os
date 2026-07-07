use crate::display_flush::Epd;
use crate::upload::{UploadBegin, UploadChunk};
use crate::{UPLOAD_BEGINS, UPLOAD_CHUNKS, UPLOAD_RESULTS, UPLOAD_RETURNS};
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use core::sync::atomic::{AtomicU8, Ordering};
use embedded_hal::spi::{Operation, SpiBus as BlockingSpiBus, SpiDevice};
use embedded_sdmmc::sdcard::CardType;
use embedded_sdmmc::{Directory, Mode, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use esp_hal::gpio::Output;
use esp_hal::spi::master::{Config as SpiConfig, SpiDmaBus};
use esp_hal::time::RateExtU32;
use esp_hal::Async;

/// SD SPI-mode identification must run at 100-400 kHz; data transfer is
/// specced to 25 MHz. The shared bus otherwise runs at the active panel's
/// clock, which on the X4 (SSD1677, 40 MHz) is out of SD spec entirely and
/// what the read-retry machinery in the EPUB path was quietly absorbing.
const SD_IDENT_FREQ_KHZ: u32 = 400;
const SD_DATA_FREQ_MHZ: u32 = 20;
/// Restore frequency after SD access: the active panel's SPI clock. This
/// MUST be per-panel — the UC8253 (X3) can't decode above ~20 MHz, so
/// restoring the X4's 40 MHz leaves the panel deaf to every subsequent
/// command (init included, since the boot catalog read precedes it).
const DISPLAY_FREQ_HZ: u32 = display::epd::SPI_HZ;

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

const SD_SPI_CHUNK_BYTES: usize = 64;

#[repr(align(4))]
struct AlignedSdChunk([u8; SD_SPI_CHUNK_BYTES]);

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
            let mut bounce = AlignedSdChunk([0xFF; SD_SPI_CHUNK_BYTES]);
            self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])?;
            chunk.copy_from_slice(&bounce.0[..chunk.len()]);
        }
        Ok(())
    }

    fn write_chunked(&mut self, buffer: &[u8]) -> Result<(), SPI::Error> {
        for chunk in buffer.chunks(SD_SPI_CHUNK_BYTES) {
            let mut bounce = AlignedSdChunk([0xFF; SD_SPI_CHUNK_BYTES]);
            bounce.0[..chunk.len()].copy_from_slice(chunk);
            self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])?;
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
            let mut bounce = AlignedSdChunk([0xFF; SD_SPI_CHUNK_BYTES]);
            bounce.0[..write_chunk.len()].copy_from_slice(write_chunk);
            self.spi
                .transfer_in_place(&mut bounce.0[..write_chunk.len()])?;
            read_chunk.copy_from_slice(&bounce.0[..read_chunk.len()]);
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
            let mut bounce = AlignedSdChunk([0xFF; SD_SPI_CHUNK_BYTES]);
            bounce.0[..chunk.len()].copy_from_slice(chunk);
            self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])?;
            chunk.copy_from_slice(&bounce.0[..chunk.len()]);
        }
        Ok(())
    }
}

type SdSpi<'a> = SdSpiDevice<'a, SpiDmaBus<'static, Async>, Output<'static>>;

type SdCardDevice<'a> = SdCard<SdSpi<'a>, SdDelay>;
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
        .apply_config(&SpiConfig::default().with_frequency(DISPLAY_FREQ_HZ.Hz()));
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
        let _ = spi.apply_config(&SpiConfig::default().with_frequency(SD_IDENT_FREQ_KHZ.kHz()));
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
            .apply_config(&SpiConfig::default().with_frequency(SD_DATA_FREQ_MHZ.MHz()));
    });
    let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(card, StaticTime, 5000);
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
        let _ = spi.apply_config(&SpiConfig::default().with_frequency(SD_IDENT_FREQ_KHZ.kHz()));
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
            .apply_config(&SpiConfig::default().with_frequency(SD_DATA_FREQ_MHZ.MHz()));
    });
    let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(card, StaticTime, 5000);
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
            if removed {
                crate::reader_cache_files::delete_upload_label(&root, begin.name.as_str());
            }
            removed
        } else {
            let written = write_one_book(&books, &begin).await;
            // Stash the readable filename so the Library can label the book
            // before it is ever opened; the 8.3 name on the card can't carry it.
            if written && !begin.label.is_empty() {
                crate::reader_cache_files::write_upload_label(
                    &root,
                    begin.name.as_str(),
                    begin.label.as_str(),
                );
            }
            written
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
    books: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    begin: &UploadBegin,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(file) = books.open_file_in_dir(begin.name.as_str(), Mode::ReadWriteCreateOrTruncate)
    else {
        drain_until_end().await;
        return false;
    };
    let mut failed = false;
    let mut aborted = false;
    loop {
        let chunk = UPLOAD_CHUNKS.receive().await;
        if !failed && !chunk.abort {
            if let Some(buffer) = &chunk.buffer {
                if file.write(&buffer[..chunk.len.min(buffer.len())]).is_err() {
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
    drop(file);
    if failed || aborted {
        let _ = books.delete_file_in_dir(begin.name.as_str());
        return false;
    }
    true
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
