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

#[allow(unsafe_code)]
#[link_section = ".rodata_desc"]
#[used]
#[no_mangle]
pub static _ESP_APP_DESC: EspAppDesc = EspAppDesc {
    magic_word: 0xABCD5432,
    secure_version: 0,
    reserv1: [0; 2],
    version: descriptor_version(env!("MARIGOLD_VERSION")),
    #[cfg(not(feature = "device-x3"))]
    project_name: *b"xteink-x4\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
    #[cfg(feature = "device-x3")]
    project_name: *b"xteink-x3\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
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

/// Copies the build's version string (`MARIGOLD_VERSION`, set by build.rs)
/// into the descriptor's 32-byte field: truncated to 31 bytes, NUL-padded.
const fn descriptor_version(version: &str) -> [u8; 32] {
    let bytes = version.as_bytes();
    let mut field = [0u8; 32];
    let mut index = 0;
    while index < bytes.len() && index < field.len() - 1 {
        field[index] = bytes[index];
        index += 1;
    }
    field
}

/// What follows a panic, after esp-backtrace has printed the trace. A device
/// in someone's hand must not sit halted with the CPU running and no
/// watchdog until the battery is flat, so release builds give the trace two
/// seconds to drain over USB and then reboot into the reader. Bench builds
/// halt instead, keeping the evidence on the console.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "Rust" fn custom_halt() -> ! {
    #[cfg(feature = "bench-serial")]
    loop {
        core::hint::spin_loop();
    }
    #[cfg(not(feature = "bench-serial"))]
    {
        esp_hal::delay::Delay::new().delay_millis(2_000);
        esp_hal::system::software_reset()
    }
}

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
use esp_hal::analog::adc::{
    Adc, AdcCalCurve, AdcCalScheme, AdcChannel, AdcConfig, AdcPin, Attenuation,
};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::interrupt::Priority;
use esp_hal::peripherals::ADC1;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_rtos::embassy::Executor;
use esp_rtos::embassy::InterruptExecutor;
use static_cell::StaticCell;
use tasks::input::InputPins;

mod block_sink;
mod book_images;
pub mod catalog;
mod custom_font;
mod display_flush;
mod firmware_files;
mod image_decode;
mod library_sd;
#[cfg(not(feature = "device-x3"))]
mod next_page;
mod ota_update;
mod page_images;
mod reader_cache;
mod reader_cache_files;
mod reader_layout;
mod reader_store;
mod sd_session;
mod sync_mem;
pub mod tasks;
pub mod upload;
mod views;
mod wake_mark;

/// Main-stack high-water mark. Boot paints the unused stack with a pattern;
/// `report` later finds the deepest word overwritten and logs it whenever
/// the mark grows, so hardware runs show real stack use next to the
/// link-time floor in build.rs. The executor and the interrupt-priority
/// input executor both run on this stack; radio threads have their own.
pub(crate) mod stack_probe {
    use core::sync::atomic::{AtomicU32, Ordering};

    const PAINT: u32 = 0x5A5A_A5A5;
    /// Bytes above `_stack_end` left untouched: esp-hal's stack-guard word
    /// sits 60 bytes in, under a write watchpoint.
    const GUARD_SKIP: usize = 128;
    static REPORTED: AtomicU32 = AtomicU32::new(0);

    // Linker symbols bounding the main stack (it grows down from the start).
    #[allow(unsafe_code)]
    unsafe extern "C" {
        static _stack_start: u32;
        static _stack_end: u32;
    }

    fn bounds() -> (usize, usize) {
        let top = core::ptr::addr_of!(_stack_start) as usize;
        let bottom = core::ptr::addr_of!(_stack_end) as usize + GUARD_SKIP;
        (bottom, top)
    }

    /// Paint everything below the current frame. Call first thing in main.
    #[allow(unsafe_code)]
    pub(crate) fn paint() {
        let sp: usize;
        // SAFETY: reads the stack pointer only.
        unsafe { core::arch::asm!("mv {}, sp", out(reg) sp) };
        let (bottom, _) = bounds();
        let mut word = bottom & !3;
        // Leave a margin under the live frame for this function's own use.
        let end = sp.saturating_sub(256) & !3;
        while word < end {
            // SAFETY: [bottom, sp - 256) is unused main stack at boot: no
            // frame lives below the current one yet and the guard word is
            // excluded above.
            unsafe { (word as *mut u32).write_volatile(PAINT) };
            word += 4;
        }
    }

    /// Bytes of main stack ever used (top minus the deepest touched word).
    #[allow(unsafe_code)]
    pub(crate) fn high_water() -> usize {
        let (bottom, top) = bounds();
        let mut word = bottom & !3;
        while word < top {
            // SAFETY: in-bounds aligned read of the main stack region.
            if unsafe { (word as *const u32).read_volatile() } != PAINT {
                break;
            }
            word += 4;
        }
        top - word
    }

    /// Log the mark when it has grown since the last report.
    pub(crate) fn report(context: &str) {
        let used = high_water() as u32;
        if used > REPORTED.load(Ordering::Relaxed) {
            REPORTED.store(used, Ordering::Relaxed);
            let (bottom, top) = bounds();
            esp_println::println!(
                "bench: stack_high_water used={} region={} context={}",
                used,
                top - bottom + GUARD_SKIP,
                context
            );
        }
    }
}

pub static INPUT_EVENTS: Channel<CriticalSectionRawMutex, InputEvent, 8> = Channel::new();
pub static INPUT_START: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
pub static INPUT_ENABLED: AtomicBool = AtomicBool::new(false);
pub static LATEST_READER_REQUEST_ID: AtomicU32 = AtomicU32::new(0);
/// Moves on every button press queued on `INPUT_EVENTS` (battery samples
/// carry none), so work running on the display task can see a press the
/// app task has not yet been able to receive and give way to it.
pub static BUTTON_PRESSES: AtomicU32 = AtomicU32::new(0);

/// Count a button press for `BUTTON_PRESSES`; every producer of button
/// events calls this as it queues one. Readers only compare against an
/// earlier value, so two writers racing still move the count.
pub fn note_button_press() {
    let presses = BUTTON_PRESSES.load(core::sync::atomic::Ordering::Relaxed);
    BUTTON_PRESSES.store(
        presses.wrapping_add(1),
        core::sync::atomic::Ordering::Relaxed,
    );
}
pub static DISPLAY_COMMANDS: Channel<CriticalSectionRawMutex, DisplayCommand, 4> = Channel::new();
pub static DISPLAY_EVENTS: Channel<CriticalSectionRawMutex, DisplayEvent, 16> = Channel::new();
pub static LIBRARY_EVENTS: Channel<CriticalSectionRawMutex, LibraryEvent, 8> = Channel::new();
pub static STORAGE_COMMANDS: Channel<CriticalSectionRawMutex, StorageCommand, 4> = Channel::new();
pub static POWER_EVENTS: Channel<CriticalSectionRawMutex, PowerEvent, 4> = Channel::new();
pub static SYNC_COMMANDS: Channel<CriticalSectionRawMutex, SyncCommand, 2> = Channel::new();
pub static SYNC_EVENTS: Channel<CriticalSectionRawMutex, SyncEvent, 4> = Channel::new();
pub static SYNC_LOANS: Channel<CriticalSectionRawMutex, sync_mem::SyncLoan, 1> = Channel::new();
pub static UPLOAD_BEGINS: Channel<CriticalSectionRawMutex, upload::UploadBegin, 1> = Channel::new();
pub static UPLOAD_CHUNKS: Channel<CriticalSectionRawMutex, upload::UploadChunk, 2> = Channel::new();
pub static UPLOAD_RETURNS: Channel<CriticalSectionRawMutex, &'static mut [u8], 2> = Channel::new();
pub static UPLOAD_RESULTS: Channel<CriticalSectionRawMutex, bool, 1> = Channel::new();
pub static UPLOAD_STOP_REQUESTS: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
pub static UPLOAD_STOPPED: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
pub static WIFI_STORAGE_RESULTS: Channel<CriticalSectionRawMutex, bool, 1> = Channel::new();

static EXECUTOR: StaticCell<Executor> = StaticCell::new();
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
    stack_probe::paint();
    // Config::default() leaves the ESP32-C3 at 80 MHz; layout, panel byte
    // transforms, and EPUB inflate are all CPU-bound, so run at full speed
    // and rely on race-to-idle for power.
    let config = esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::_160MHz);
    let peripherals = esp_hal::init(config);
    esp_println::println!("xteink-x4-os: boot");

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
    // only ever carries SD traffic, a chunk at a time - the EPD is
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
    {
        let input_executor =
            INPUT_EXECUTOR.init(InterruptExecutor::new(sw_ints.software_interrupt1));
        let input_spawner = input_executor.start(Priority::Priority1);
        #[cfg(feature = "bench-serial")]
        input_spawner.spawn(tasks::bench_serial::run(peripherals.USB_DEVICE).unwrap());
        esp_println::println!("main: spawn input");
        input_spawner.spawn(
            tasks::input::run(
                adc1,
                InputPins {
                    power: power_button,
                    #[cfg(not(feature = "device-x3"))]
                    aux_pin: aux_adc,
                    nav_pin: nav_adc,
                    page_pin: page_adc,
                },
            )
            .unwrap(),
        );
    }

    // Built here, at the base of the stack, not at the first book open.
    let epub_scratch = tasks::display::init_epub_scratch();

    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner: Spawner| {
        #[cfg(feature = "device-x3")]
        spawner.spawn(tasks::input::battery_run(battery_gauge).unwrap());
        esp_println::println!("main: spawn display");
        spawner.spawn(tasks::display::run(epd_bus, sd_cs, epub_scratch).unwrap());
        esp_println::println!("main: spawn power");
        spawner.spawn(tasks::power::run(peripherals.LPWR).unwrap());
        esp_println::println!("main: spawn app");
        spawner.spawn(tasks::app::run().unwrap());
        esp_println::println!("main: spawn wifi");
        spawner.spawn(tasks::wifi::run(spawner, peripherals.WIFI).unwrap());
    })
}
