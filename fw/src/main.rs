#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]
#![deny(unsafe_code)]
#![allow(clippy::manual_div_ceil)] // False positive inside esp_hal::dma_buffers!.
#![deny(clippy::large_stack_arrays)]
#![deny(clippy::large_types_passed_by_value)]

#[repr(C)]
pub struct EspAppDesc {
    pub magic_word: u32,
    pub secure_version: u32,
    pub reserv1: [u32; 2],
    pub version: [u8; 32],
    pub project_name: [u8; 32],
    pub time: [u8; 16],
    pub date: [u8; 16],
    pub idf_ver: [u8; 32],
    pub app_elf_sha256: [u8; 32],
    pub min_efuse_blk_rev_full: u16,
    pub max_efuse_blk_rev_full: u16,
    pub mmu_page_size: u8,
    pub spi_flash_mode: u8,
    pub reserv3: [u8; 2],
    pub reserv2: [u32; 18],
}

#[allow(unsafe_code)]
#[link_section = ".rodata_desc"]
#[used]
#[no_mangle]
pub static _ESP_APP_DESC: EspAppDesc = EspAppDesc {
    magic_word: 0xABCD5432,
    secure_version: 0,
    reserv1: [0; 2],
    version: [0; 32],
    project_name: [0; 32],
    time: *b"00:00:00\0\0\0\0\0\0\0\0",
    date: *b"2026-05-20\0\0\0\0\0\0",
    idf_ver: [0; 32],
    app_elf_sha256: [0; 32],
    min_efuse_blk_rev_full: 0,
    max_efuse_blk_rev_full: 65535,
    mmu_page_size: 16,
    spi_flash_mode: 2,
    reserv3: [0; 2],
    reserv2: [0; 18],
};

pub use app_core::{
    AppView, Button, DisplayCommand, DisplayEvent, DisplayOrientation, InputEvent, LibraryEvent,
    PowerEvent, RefreshPolicy, RenderKind, RenderRequest,
};
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, Attenuation};
use esp_hal::dma::{Dma, DmaPriority};
use esp_hal::entry;
use esp_hal::gpio::{Input, Io, Level, Output, Pull};
use esp_hal::peripherals::ADC1;
use esp_hal::prelude::*;
use esp_hal::spi::master::Spi;
use esp_hal::timer::timg::TimerGroup;
use esp_hal_embassy::Executor;
use static_cell::StaticCell;
use tasks::input::InputPins;

pub mod catalog;
mod display_flush;
mod library_sd;
mod reader_cache;
mod reader_layout;
mod reader_store;
pub mod tasks;
mod views;

pub static INPUT_EVENTS: Channel<CriticalSectionRawMutex, InputEvent, 8> = Channel::new();
pub static DISPLAY_COMMANDS: Channel<CriticalSectionRawMutex, DisplayCommand, 1> = Channel::new();
pub static DISPLAY_EVENTS: Channel<CriticalSectionRawMutex, DisplayEvent, 4> = Channel::new();
pub static LIBRARY_EVENTS: Channel<CriticalSectionRawMutex, LibraryEvent, 64> = Channel::new();
pub static POWER_EVENTS: Channel<CriticalSectionRawMutex, PowerEvent, 4> = Channel::new();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!("{}", info);
    loop {}
}

static EXECUTOR: StaticCell<Executor> = StaticCell::new();

#[entry]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    esp_println::println!("xteink-x4-os: boot");

    let timers = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timers.timer0);

    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);
    let epd_cs = Output::new(io.pins.gpio21, Level::High);
    let epd_dc = Output::new(io.pins.gpio4, Level::High);
    let epd_rst = Output::new(io.pins.gpio5, Level::High);
    let epd_busy = Input::new(io.pins.gpio6, Pull::None);
    let sd_cs = Output::new(io.pins.gpio12, Level::High);
    let power_button = Input::new(io.pins.gpio3, Pull::Up);

    let mut adc_config = AdcConfig::new();
    let aux_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(io.pins.gpio0, Attenuation::Attenuation11dB);
    let nav_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(io.pins.gpio1, Attenuation::Attenuation11dB);
    let page_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(io.pins.gpio2, Attenuation::Attenuation11dB);
    let adc1 = Adc::new(peripherals.ADC1, adc_config);

    let dma = Dma::new(peripherals.DMA);
    let dma_channel = dma
        .channel0
        .configure_for_async(false, DmaPriority::Priority0);
    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = esp_hal::dma_buffers!(8000);
    let dma_rx = esp_hal::dma::DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap();
    let dma_tx = esp_hal::dma::DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();
    let epd_spi = Spi::new(peripherals.SPI2, 40_u32.MHz(), esp_hal::spi::SpiMode::Mode0)
        .with_sck(io.pins.gpio8)
        .with_mosi(io.pins.gpio10)
        .with_miso(io.pins.gpio7)
        .with_dma(dma_channel)
        .with_buffers(dma_rx, dma_tx);
    let epd_bus = hal_ext::spi_dma::EpdBus::new(epd_spi, epd_cs, epd_dc, epd_busy, epd_rst);

    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner: Spawner| {
        spawner.spawn(tasks::app::run()).unwrap();
        spawner.spawn(tasks::display::run(epd_bus, sd_cs)).unwrap();
        spawner
            .spawn(tasks::input::run(
                adc1,
                InputPins {
                    power: power_button,
                    aux_pin: aux_adc,
                    nav_pin: nav_adc,
                    page_pin: page_adc,
                },
            ))
            .unwrap();
        spawner.spawn(tasks::power::run(peripherals.LPWR)).unwrap();
        spawner.spawn(tasks::wifi::run(peripherals.WIFI)).unwrap();
    })
}
