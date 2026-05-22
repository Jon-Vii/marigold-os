use crate::display_flush::Epd;
use crate::reader_store::{LibraryScanStatus, ReaderStore};
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::{Operation, SpiBus as BlockingSpiBus, SpiDevice};
use embedded_sdmmc::{LfnBuffer, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use esp_hal::gpio::Output;
use esp_hal::prelude::*;
use heapless::String;

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

pub(crate) struct SdSpiDevice<'a, SPI, CS> {
    pub(crate) spi: &'a mut SPI,
    pub(crate) cs: &'a mut CS,
    pub(crate) delay: esp_hal::delay::Delay,
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
                Operation::Read(buffer) => self.spi.read(buffer),
                Operation::Write(buffer) => self.spi.write(buffer),
                Operation::Transfer(read, write) => self.spi.transfer(read, write),
                Operation::TransferInPlace(buffer) => self.spi.transfer_in_place(buffer),
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

pub(crate) fn scan_books(epd: &mut Epd, sd_cs: &mut Output<'static>, library: &mut ReaderStore) {
    esp_println::println!("sd: scan start");
    library.clear();
    epd.deselect_display();
    sd_cs.set_high();
    epd.spi_mut().change_bus_frequency(400_u32.kHz());

    let startup_clocks = [0xFF; 10];
    if BlockingSpiBus::write(epd.spi_mut(), &startup_clocks).is_err() {
        esp_println::println!("sd: startup clocks failed");
        epd.spi_mut().change_bus_frequency(40_u32.MHz());
        library.status = LibraryScanStatus::Error;
        return;
    }

    let status = 'scan: {
        let spi = SdSpiDevice {
            spi: epd.spi_mut(),
            cs: sd_cs,
            delay: esp_hal::delay::Delay::new(),
        };
        let card = SdCard::new(spi, esp_hal::delay::Delay::new());
        match card.num_bytes() {
            Ok(bytes) => esp_println::println!("sd: card size {} bytes", bytes),
            Err(err) => {
                esp_println::println!("sd: card init failed: {:?}", err);
                break 'scan LibraryScanStatus::Error;
            }
        }

        card.spi(|device| device.spi.change_bus_frequency(8_u32.MHz()));
        let volume_mgr: VolumeManager<_, _, 4, 4, 1> = VolumeManager::new(card, StaticTime);
        let volume = match volume_mgr.open_volume(VolumeIdx(0)) {
            Ok(volume) => volume,
            Err(err) => {
                esp_println::println!("sd: open volume failed: {:?}", err);
                break 'scan LibraryScanStatus::Error;
            }
        };
        let root = match volume.open_root_dir() {
            Ok(root) => root,
            Err(err) => {
                esp_println::println!("sd: open root failed: {:?}", err);
                break 'scan LibraryScanStatus::Error;
            }
        };

        if let Ok(books) = root.open_dir("BOOKS") {
            collect_epubs(&books, "/books/", true, library);
        }
        if library.count == 0 {
            collect_epubs(&root, "/", false, library);
        }

        if library.count == 0 {
            LibraryScanStatus::Empty
        } else {
            LibraryScanStatus::Ready
        }
    };
    epd.spi_mut().change_bus_frequency(40_u32.MHz());
    library.status = status;
    esp_println::println!("sd: scan complete, {} epub(s)", library.count);
}

fn collect_epubs<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    dir: &embedded_sdmmc::Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    prefix: &str,
    in_books_dir: bool,
    library: &mut ReaderStore,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut lfn_storage = [0u8; 192];
    let mut lfn_buffer = LfnBuffer::new(&mut lfn_storage);
    let _ = dir.iterate_dir_lfn(&mut lfn_buffer, |entry, long_name| {
        if entry.attributes.is_directory() || entry.attributes.is_volume() {
            return;
        }

        let mut name = String::<64>::new();
        let mut open_name = String::<16>::new();
        use core::fmt::Write;
        let _ = write!(open_name, "{}", entry.name);
        let Some(file_name) = long_name else {
            let _ = write!(name, "{}", entry.name);
            if !is_epub_name(&name) {
                return;
            }
            push_prefixed(prefix, &name, &open_name, in_books_dir, entry.size, library);
            return;
        };

        if is_epub_name(file_name) {
            push_prefixed(
                prefix,
                file_name,
                &open_name,
                in_books_dir,
                entry.size,
                library,
            );
        }
    });
}

fn push_prefixed(
    prefix: &str,
    name: &str,
    open_name: &str,
    in_books_dir: bool,
    byte_size: u32,
    library: &mut ReaderStore,
) {
    let mut path = String::<64>::new();
    let _ = path.push_str(prefix);
    let _ = path.push_str(name);
    library.push(&path, open_name, in_books_dir, byte_size);
}

fn is_epub_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 5 {
        return false;
    }
    let ext = &bytes[bytes.len() - 5..];
    ext[0] == b'.'
        && ext[1].eq_ignore_ascii_case(&b'e')
        && ext[2].eq_ignore_ascii_case(&b'p')
        && ext[3].eq_ignore_ascii_case(&b'u')
        && ext[4].eq_ignore_ascii_case(&b'b')
}
