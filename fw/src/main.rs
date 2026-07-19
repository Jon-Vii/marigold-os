#![no_std]
#![no_main]
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

// Zero-pad a string into a fixed descriptor field. The fields are always
// [u8; 32] in the image, so filling them costs no bytes; overlong input
// fails the const evaluation instead of truncating silently.
const fn desc_field<const N: usize>(s: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

#[allow(unsafe_code)]
#[link_section = ".rodata_desc"]
#[used]
#[no_mangle]
pub static _ESP_APP_DESC: EspAppDesc = EspAppDesc {
    magic_word: 0xABCD5432,
    secure_version: 0,
    reserv1: [0; 2],
    version: desc_field(env!("CARGO_PKG_VERSION")),
    project_name: desc_field("CalendulaOS (MarigoldOS)"),
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
use core::sync::atomic::AtomicU32;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use esp_backtrace as _;
use esp_hal::analog::adc::{
    Adc, AdcCalCurve, AdcCalScheme, AdcChannel, AdcConfig, AdcPin, Attenuation,
};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::interrupt::software::SoftwareInterruptControl;
#[cfg(not(feature = "device-x3"))]
use esp_hal::interrupt::Priority;
use esp_hal::peripherals::ADC1;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_rtos::embassy::Executor;
#[cfg(not(feature = "device-x3"))]
use esp_rtos::embassy::InterruptExecutor;
use static_cell::StaticCell;
use tasks::input::InputPins;

pub mod catalog;
mod custom_font;
mod display_flush;
mod library_sd;
mod ota_update;
mod reader_cache;
mod reader_cache_files;
mod reader_layout;
mod reader_store;
mod sd_session;
mod sleep_marker;
mod sync_mem;
pub mod tasks;
pub mod upload;
mod views;

pub static INPUT_EVENTS: Channel<CriticalSectionRawMutex, InputEvent, 8> = Channel::new();
pub static LATEST_READER_REQUEST_ID: AtomicU32 = AtomicU32::new(0);
pub static DISPLAY_COMMANDS: Channel<CriticalSectionRawMutex, DisplayCommand, 4> = Channel::new();
// 8 slots (270 B each) is enough for the cache-build burst case: required
// events retry with library-event eviction on a full queue (see
// send_required_display_event), so a shorter queue costs at most extra
// retries, and the ~2.1 KB of .bss saved widens the main stack region.
pub static DISPLAY_EVENTS: Channel<CriticalSectionRawMutex, DisplayEvent, 8> = Channel::new();
pub static LIBRARY_EVENTS: Channel<CriticalSectionRawMutex, LibraryEvent, 8> = Channel::new();
pub static STORAGE_COMMANDS: Channel<CriticalSectionRawMutex, StorageCommand, 4> = Channel::new();
pub static POWER_EVENTS: Channel<CriticalSectionRawMutex, PowerEvent, 4> = Channel::new();
// Power button (GPIO3) handoff for the terminal deep-sleep path. The input
// task owns the pin and polls it for the whole run; the power task needs it
// back as the RTC wake source, and re-materialising it there is only sound
// while no other handle is live. A request on WAKE_PIN_REQUESTS asks the input
// task to stop polling and surrender its `Input`, which comes back over
// WAKE_PIN_HANDOFF. One request is ever sent -- the branch that sends it does
// not return -- so both stay at a single slot.
//
// SAFETY INVARIANT: WAKE_PIN_HANDOFF must be consumed immediately before
// GPIO3::steal(). No other code may construct GPIO3 directly.
pub static WAKE_PIN_REQUESTS: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
pub static WAKE_PIN_HANDOFF: Channel<CriticalSectionRawMutex, Input<'static>, 1> = Channel::new();
pub static SYNC_COMMANDS: Channel<CriticalSectionRawMutex, SyncCommand, 2> = Channel::new();
pub static SYNC_EVENTS: Channel<CriticalSectionRawMutex, SyncEvent, 4> = Channel::new();
// Carries Err when the display task refuses the loan (progress flush failed);
// the wifi task reports the failure and returns to waiting for Start, so the
// Wireless screen can offer a retry instead of hanging on a loan that will
// never arrive.
pub static SYNC_LOANS: Channel<
    CriticalSectionRawMutex,
    Result<sync_mem::SyncLoan, app_core::SyncError>,
    1,
> = Channel::new();
pub static UPLOAD_BEGINS: Channel<CriticalSectionRawMutex, upload::UploadBegin, 1> = Channel::new();
pub static UPLOAD_CHUNKS: Channel<CriticalSectionRawMutex, upload::UploadChunk, 2> = Channel::new();
pub static UPLOAD_RETURNS: Channel<CriticalSectionRawMutex, &'static mut [u8], 2> = Channel::new();
pub static UPLOAD_RESULTS: Channel<CriticalSectionRawMutex, bool, 1> = Channel::new();

static EXECUTOR: StaticCell<Executor> = StaticCell::new();
#[cfg(not(feature = "device-x3"))]
static INPUT_EXECUTOR: StaticCell<InterruptExecutor<1>> = StaticCell::new();

type BoardAdc = ADC1<'static>;
type BoardAdcDriver = Adc<'static, BoardAdc, esp_hal::Blocking>;

/// Blocking median-of-three ADC read for the boot-time recovery combo check,
/// before the async input task exists. Median rejects a single noisy sample.
fn median3_adc<P, CS>(adc: &mut BoardAdcDriver, pin: &mut AdcPin<P, BoardAdc, CS>) -> u16
where
    P: AdcChannel,
    CS: AdcCalScheme<BoardAdc>,
{
    let mut v = [0u16; 3];
    for slot in v.iter_mut() {
        *slot = loop {
            match adc.read_oneshot(pin) {
                Ok(mv) => break mv,
                Err(nb::Error::WouldBlock) => {}
                Err(_) => break 0,
            }
        };
    }
    v[0].max(v[1]).min(v[0].min(v[1]).max(v[2]))
}

#[esp_hal::main]
fn main() -> ! {
    // Config::default() leaves the ESP32-C3 at 80 MHz; layout, panel byte
    // transforms, and EPUB inflate are all CPU-bound, so run at full speed
    // and rely on race-to-idle for power.
    let config = esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::_160MHz);
    let peripherals = esp_hal::init(config);
    esp_println::println!("calendula-os: boot");

    // Deep sleep is terminal, so waking is this cold boot; the RTC wake
    // cause and RTC RAM are its only trace. Fast wake needs both: the
    // Power-button (GPIO) wake cause proves how the chip woke, and the
    // sleep_marker records that the pre-sleep handshake actually settled
    // the sleep frame on the panel — a failed flush still powers down, so
    // the wake cause alone can't vouch for the pixels. Only when both hold
    // does the display task seed its refresh planner for the fast wake
    // waveform. Battery pulls, crashes, and software resets read false.
    // The marker is consumed unconditionally so it never outlives one boot.
    let woke_by_button = hal_ext::rtc::woke_from_deep_sleep_gpio();
    let sleep_image_settled = sleep_marker::take_sleep_image_settled();
    let deep_sleep_wake = woke_by_button && sleep_image_settled;
    esp_println::println!(
        "main: deep_sleep_wake={} (gpio={}, sleep_image={})",
        deep_sleep_wake,
        woke_by_button,
        sleep_image_settled
    );

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_ints = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_ints.software_interrupt0);

    let epd_cs = Output::new(peripherals.GPIO21, Level::High, OutputConfig::default());
    let epd_dc = Output::new(peripherals.GPIO4, Level::High, OutputConfig::default());
    let epd_rst = Output::new(peripherals.GPIO5, Level::High, OutputConfig::default());
    let epd_busy = Input::new(peripherals.GPIO6, InputConfig::default());
    let sd_cs = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());
    let power_button = Input::new(
        peripherals.GPIO3,
        InputConfig::default().with_pull(Pull::Up),
    );

    let mut adc_config = AdcConfig::new();
    // GPIO0 is the battery ADC divider on the X4; on the X3 it is I2C SCL
    // (paired with GPIO20 SDA) for the fuel gauge, so it is not an ADC pin.
    #[cfg(not(feature = "device-x3"))]
    let aux_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<BoardAdc>>(peripherals.GPIO0, Attenuation::_11dB);
    let mut nav_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<BoardAdc>>(peripherals.GPIO1, Attenuation::_11dB);
    let mut page_adc = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<BoardAdc>>(peripherals.GPIO2, Attenuation::_11dB);
    let mut adc1 = Adc::new(peripherals.ADC1, adc_config);

    // X3 fuel gauge on I2C0: SCL=GPIO0, SDA=GPIO20, 400 kHz.
    #[cfg(feature = "device-x3")]
    let battery_gauge = {
        let i2c = esp_hal::i2c::master::I2c::new(
            peripherals.I2C0,
            esp_hal::i2c::master::Config::default()
                .with_frequency(Rate::from_khz(400))
                // The BQ27220 clock-stretches for milliseconds while it
                // processes a command; esp-hal's default SCL timeout of 10
                // bus cycles (25 us) aborts every read with Timeout. Allow
                // 2000 cycles (~5 ms at 400 kHz, rounded up to a power of
                // two by the hardware), matching the 4 ms Wire timeout the
                // stock firmware uses.
                .with_timeout(esp_hal::i2c::master::BusTimeout::BusCycles(2000)),
        )
        .expect("I2C0 config")
        .with_scl(peripherals.GPIO0)
        .with_sda(peripherals.GPIO20)
        .into_async();
        hal_ext::bq27220::Bq27220::new(i2c)
    };

    // RecoveryBoot escape hatch: holding Back + Up at reset repoints otadata at
    // slot 0 and reboots into it — a way back if the far slot's firmware boots
    // but misbehaves. Sampled here, the earliest point, before any task owns the
    // ADC; the stock bootloader can't read buttons, so only the running firmware
    // can honour the combo. Median-of-3 so a single noisy read can't trip it.
    {
        let nav_mv = median3_adc(&mut adc1, &mut nav_adc);
        let page_mv = median3_adc(&mut adc1, &mut page_adc);
        if ota_update::recovery_combo_held(nav_mv, page_mv) {
            esp_println::println!("recovery: Back+Up held (nav={} page={})", nav_mv, page_mv);
            if ota_update::recover_to_slot0() {
                esp_hal::system::software_reset();
            }
        }
    }

    ota_update::mark_running_slot_valid();

    // One display band must fit a single TX DMA buffer (X4 fills it
    // exactly; the X3's 99-byte rows leave 80 bytes slack). The RX side
    // only ever carries the SD session's bounce chunk - the EPD is
    // write-only - so it stays at chunk size; every byte saved in .bss
    // is main-stack headroom now (see build.rs).
    const _: () = assert!(display::BAND_BYTES <= 8000);
    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) =
        esp_hal::dma_buffers!(sd_session::SD_SPI_CHUNK_BYTES, 8000);
    let dma_rx = esp_hal::dma::DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap();
    let dma_tx = esp_hal::dma::DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();
    let epd_spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_hz(display::epd::SPI_HZ))
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
    #[cfg(not(feature = "device-x3"))]
    {
        let input_executor =
            INPUT_EXECUTOR.init(InterruptExecutor::new(sw_ints.software_interrupt1));
        let input_spawner = input_executor.start(Priority::Priority1);
        esp_println::println!("main: spawn input");
        input_spawner.spawn(
            tasks::input::run(
                adc1,
                InputPins {
                    power: Some(power_button),
                    aux_pin: aux_adc,
                    nav_pin: nav_adc,
                    page_pin: page_adc,
                },
            )
            .unwrap(),
        );
    }

    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner: Spawner| {
        #[cfg(feature = "device-x3")]
        {
            esp_println::println!("main: spawn input");
            spawner.spawn(
                tasks::input::run(
                    adc1,
                    InputPins {
                        power: Some(power_button),
                        nav_pin: nav_adc,
                        page_pin: page_adc,
                        gauge: battery_gauge,
                    },
                )
                .unwrap(),
            );
        }
        esp_println::println!("main: spawn display");
        spawner.spawn(tasks::display::run(epd_bus, sd_cs, deep_sleep_wake).unwrap());
        esp_println::println!("main: spawn power");
        spawner.spawn(tasks::power::run(peripherals.LPWR).unwrap());
        esp_println::println!("main: spawn app");
        spawner.spawn(tasks::app::run().unwrap());
        esp_println::println!("main: spawn wifi");
        spawner.spawn(tasks::wifi::run(spawner, peripherals.WIFI).unwrap());
    })
}
