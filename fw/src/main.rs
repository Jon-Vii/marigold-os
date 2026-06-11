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

// The only allocator user is the Wi-Fi sync session; reader paths stay
// allocation-free because no region exists until sync_mem donates the
// loaned buffers.
extern crate alloc;

pub use app_core::{
    AppView, Button, DisplayCommand, DisplayEvent, DisplayOrientation, InputEvent, LibraryEvent,
    PowerEvent, ReaderSource, RefreshPolicy, RenderKind, RenderRequest, StorageCommand,
    SyncCommand, SyncEvent,
};
use core::sync::atomic::{AtomicBool, AtomicU32};
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, Attenuation};
use esp_hal::gpio::{Input, Level, Output, Pull};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::interrupt::Priority;
use esp_hal::peripherals::ADC1;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::RateExtU32;
use esp_hal::timer::{timg::TimerGroup, AnyTimer};
use esp_hal_embassy::{Executor, InterruptExecutor};
use static_cell::StaticCell;
use tasks::input::InputPins;

pub mod catalog;
mod display_flush;
mod library_sd;
mod reader_cache;
mod reader_cache_files;
mod reader_layout;
mod reader_store;
mod sd_session;
mod sync_mem;
pub mod tasks;
mod views;

pub static INPUT_EVENTS: Channel<CriticalSectionRawMutex, InputEvent, 8> = Channel::new();
pub static INPUT_START: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
pub static INPUT_ENABLED: AtomicBool = AtomicBool::new(false);
pub static LATEST_READER_REQUEST_ID: AtomicU32 = AtomicU32::new(0);
pub static DISPLAY_COMMANDS: Channel<CriticalSectionRawMutex, DisplayCommand, 4> = Channel::new();
pub static DISPLAY_EVENTS: Channel<CriticalSectionRawMutex, DisplayEvent, 16> = Channel::new();
pub static LIBRARY_EVENTS: Channel<CriticalSectionRawMutex, LibraryEvent, 8> = Channel::new();
pub static STORAGE_COMMANDS: Channel<CriticalSectionRawMutex, StorageCommand, 4> = Channel::new();
pub static POWER_EVENTS: Channel<CriticalSectionRawMutex, PowerEvent, 4> = Channel::new();
pub static SYNC_COMMANDS: Channel<CriticalSectionRawMutex, SyncCommand, 2> = Channel::new();
pub static SYNC_EVENTS: Channel<CriticalSectionRawMutex, SyncEvent, 4> = Channel::new();
pub static SYNC_LOANS: Channel<CriticalSectionRawMutex, sync_mem::SyncLoan, 1> = Channel::new();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!("{}", info);
    loop {}
}

static EXECUTOR: StaticCell<Executor> = StaticCell::new();
static INPUT_EXECUTOR: StaticCell<InterruptExecutor<0>> = StaticCell::new();

#[esp_hal::main]
fn main() -> ! {
    // Config::default() leaves the ESP32-C3 at 80 MHz; layout, panel byte
    // transforms, and EPUB inflate are all CPU-bound, so run at full speed
    // and rely on race-to-idle for power.
    let config = esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::_160MHz);
    let peripherals = esp_hal::init(config);
    esp_println::println!("xteink-x4-os: boot");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let timg1 = TimerGroup::new(peripherals.TIMG1);
    esp_hal_embassy::init([AnyTimer::from(timg0.timer0), AnyTimer::from(timg1.timer0)]);

    let epd_cs = Output::new(peripherals.GPIO21, Level::High);
    let epd_dc = Output::new(peripherals.GPIO4, Level::High);
    let epd_rst = Output::new(peripherals.GPIO5, Level::High);
    let epd_busy = Input::new(peripherals.GPIO6, Pull::None);
    let sd_cs = Output::new(peripherals.GPIO12, Level::High);
    let power_button = Input::new(peripherals.GPIO3, Pull::Up);

    let mut adc_config = AdcConfig::new();
    let aux_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(peripherals.GPIO0, Attenuation::_11dB);
    let nav_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(peripherals.GPIO1, Attenuation::_11dB);
    let page_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(peripherals.GPIO2, Attenuation::_11dB);
    let adc1 = Adc::new(peripherals.ADC1, adc_config);

    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = esp_hal::dma_buffers!(8000);
    let dma_rx = esp_hal::dma::DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap();
    let dma_tx = esp_hal::dma::DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();
    let epd_spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(40_u32.MHz())
            .with_mode(esp_hal::spi::Mode::_0),
    )
    .expect("SPI2 config")
    .with_sck(peripherals.GPIO8)
    .with_mosi(peripherals.GPIO10)
    .with_miso(peripherals.GPIO7)
    .with_dma(peripherals.DMA_CH0)
    .with_buffers(dma_rx, dma_tx)
    .into_async();
    let epd_bus = hal_ext::spi_dma::EpdBus::new(epd_spi, epd_cs, epd_dc, epd_busy, epd_rst);

    // Input polls from an interrupt-priority executor so button sampling
    // keeps running while the thread executor blocks on SD/EPUB work; a
    // cold cache build no longer deafens the buttons. Channels between the
    // tasks already use CriticalSectionRawMutex, so handoff is unchanged.
    let sw_ints = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let input_executor = INPUT_EXECUTOR.init(InterruptExecutor::new(sw_ints.software_interrupt0));
    let input_spawner = input_executor.start(Priority::Priority1);
    esp_println::println!("main: spawn input");
    input_spawner
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

    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner: Spawner| {
        esp_println::println!("main: spawn display");
        spawner.spawn(tasks::display::run(epd_bus, sd_cs)).unwrap();
        let _lpwr = peripherals.LPWR;
        esp_println::println!("main: spawn app");
        spawner.spawn(tasks::app::run()).unwrap();
        esp_println::println!("main: spawn wifi");
        spawner
            .spawn(tasks::wifi::run(
                spawner,
                peripherals.WIFI,
                peripherals.SYSTIMER,
                peripherals.RNG,
                peripherals.RADIO_CLK,
            ))
            .unwrap();
    })
}
