use crate::display_flush::Epd;
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::{Operation, SpiBus as BlockingSpiBus, SpiDevice};
use embedded_sdmmc::{Directory, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use esp_hal::gpio::Output;
use esp_hal::spi::master::{Config as SpiConfig, SpiDmaBus};
use esp_hal::time::RateExtU32;
use esp_hal::Async;

/// SD SPI-mode identification must run at 100-400 kHz; data transfer is
/// specced to 25 MHz. The shared bus otherwise runs at the SSD1677's
/// 40 MHz, which is out of SD spec entirely and what the read-retry
/// machinery in the EPUB path was quietly absorbing.
const SD_IDENT_FREQ_KHZ: u32 = 400;
const SD_DATA_FREQ_MHZ: u32 = 20;
const DISPLAY_FREQ_MHZ: u32 = 40;

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

    // Identification phase: 400 kHz with at least 74 wake clocks while no
    // chip select is asserted, per the SD spec and embedded-sdmmc's docs.
    {
        let spi = epd.spi_mut();
        let _ = spi.apply_config(&SpiConfig::default().with_frequency(SD_IDENT_FREQ_KHZ.kHz()));
        let mut wake = [0xFFu8; 10];
        let _ = BlockingSpiBus::transfer_in_place(spi, &mut wake);
        let _ = BlockingSpiBus::flush(spi);
    }

    let result = {
        let spi = SdSpiDevice {
            spi: epd.spi_mut(),
            cs: sd_cs,
            delay: SdDelay,
        };
        let card = SdCard::new(spi, SdDelay);
        esp_println::println!("sd: card probe");
        if card.num_bytes().is_err() {
            Err(SdSessionError::CardInit)
        } else {
            esp_println::println!("sd: card ready");
            // Card acquired: switch to the in-spec data rate for the rest
            // of the session.
            card.spi(|device| {
                let _ = device
                    .spi
                    .apply_config(&SpiConfig::default().with_frequency(SD_DATA_FREQ_MHZ.MHz()));
            });
            let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
                VolumeManager::new_with_limits(card, StaticTime, 5000);
            let result = match volume_mgr.open_volume(VolumeIdx(0)) {
                Ok(volume) => {
                    esp_println::println!("sd: volume open");
                    let raw_volume = volume.to_raw_volume();
                    if let Ok(raw_root) = volume_mgr.open_root_dir(raw_volume) {
                        esp_println::println!("sd: root open");
                        let root = Directory::new(raw_root, &volume_mgr);
                        let value = f(&root);
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
    };

    esp_println::println!("sd: session exit");
    sd_cs.set_high();
    let _ = epd
        .spi_mut()
        .apply_config(&SpiConfig::default().with_frequency(DISPLAY_FREQ_MHZ.MHz()));
    result
}
