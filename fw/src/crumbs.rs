//! Bench builds: evidence that survives a reset.
//!
//! A bench device can stop answering while it is off USB, where nothing it
//! prints reaches the host. This keeps the last events, a panic's stack
//! words, and the reason for the last reset in RTC fast memory, which a
//! system reset leaves alone, and prints them on the next boot. Each boot
//! also appends the record it found to the `coredump` flash partition, so a
//! second reset before a host reads it loses nothing (`espflash read-flash
//! 0xff1000 0xf000`; erase that region to start over). The RTC
//! watchdog turns a hang into such a reset: the input task feeds it, and
//! only while the thread executor is still making progress.

use embassy_time::Instant;
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::{Rtc, RwdtStage, RwdtStageAction};
use portable_atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum Event {
    Wake = 2,
    Press = 3,
    Render = 4,
    Refresh = 5,
    UsbHost = 6,
    #[cfg_attr(not(feature = "bench-unplug"), allow(dead_code))]
    FakeUnplug = 7,
    #[cfg_attr(not(feature = "bench-unplug"), allow(dead_code))]
    SyntheticPress = 8,
    Boot = 9,
}

const RING: usize = 192;
const MAGIC: u32 = 0x4352_554D;
const PANIC_WORDS: usize = 12;

/// `[magic, next, panicked, panic words.., (event<<24 | value, ms)..]`.
const HEADER: usize = 3 + PANIC_WORDS;
const WORDS: usize = HEADER + 2 * RING;

#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut STORE: [u32; WORDS] = [0; WORDS];

fn get(slot: usize) -> u32 {
    // SAFETY: in-bounds word of a static that only this module touches,
    // always inside a critical section.
    #[allow(unsafe_code)]
    unsafe {
        core::ptr::addr_of!(STORE)
            .cast::<u32>()
            .add(slot)
            .read_volatile()
    }
}

fn set(slot: usize, value: u32) {
    #[allow(unsafe_code)]
    // SAFETY: as in `get`.
    unsafe {
        core::ptr::addr_of_mut!(STORE)
            .cast::<u32>()
            .add(slot)
            .write_volatile(value)
    }
}

/// Records one event; `value` keeps its low 24 bits.
pub fn note(event: Event, value: u32) {
    let ms = Instant::now().as_millis() as u32;
    critical_section::with(|_| {
        if get(0) != MAGIC {
            return;
        }
        let next = get(1) as usize % RING;
        set(
            HEADER + 2 * next,
            (event as u32) << 24 | (value & 0xFF_FFFF),
        );
        set(HEADER + 2 * next + 1, ms);
        set(1, get(1).wrapping_add(1));
    });
}

/// The previous run's record, kept until a host is attached to read it: a
/// reset that happens off USB would otherwise print it to nobody.
static PREVIOUS: critical_section::Mutex<core::cell::RefCell<Option<Previous>>> =
    critical_section::Mutex::new(core::cell::RefCell::new(None));

struct Previous {
    words: [u32; WORDS],
    reset_reason: Option<esp_hal::rtc_cntl::SocResetReason>,
}

/// Keeps what the previous run left for `print_previous`, then starts a
/// fresh record.
pub fn boot() {
    critical_section::with(|cs| {
        if get(0) == MAGIC {
            let mut words = [0u32; WORDS];
            for (slot, word) in words.iter_mut().enumerate() {
                *word = get(slot);
            }
            let reset_reason = esp_hal::system::reset_reason();
            save(
                &words,
                reset_reason.map_or(u32::MAX, |reason| reason as u32),
            );
            PREVIOUS.borrow_ref_mut(cs).replace(Previous {
                words,
                reset_reason,
            });
        }
        for slot in 0..WORDS {
            set(slot, 0);
        }
        set(0, MAGIC);
    });
    note(Event::Boot, 0);
}

const SAVE_AT: u32 = 0x00FF_1000;
const SAVE_END: u32 = 0x0100_0000;
const SLOT: u32 = 2048;
const _: () = assert!(4 * (WORDS + 1) <= SLOT as usize);

/// Appends `[reset reason, record..]` to the first erased slot, if any.
fn save(words: &[u32; WORDS], reset_reason: u32) {
    use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
    // SAFETY: bench builds only; at boot nothing else is using flash.
    #[allow(unsafe_code)]
    let mut flash = esp_storage::FlashStorage::new(unsafe { esp_hal::peripherals::FLASH::steal() });
    let mut slot = SAVE_AT;
    while slot < SAVE_END {
        let mut head = [0u8; 4];
        if flash.read(slot, &mut head).is_err() {
            return;
        }
        if head == [0xFF; 4] {
            break;
        }
        slot += SLOT;
    }
    if slot >= SAVE_END {
        return;
    }
    let mut bytes = [0u8; 4 * (WORDS + 1)];
    for (chunk, word) in bytes
        .chunks_exact_mut(4)
        .zip(core::iter::once(&reset_reason).chain(words))
    {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    let _ = flash.write(slot, &bytes);
}

/// Prints the previous run's record once, the first time it is called with
/// a host attached.
pub fn print_previous() {
    if !crate::light_sleep::usb_host_seen() {
        return;
    }
    let Some(previous) = critical_section::with(|cs| PREVIOUS.borrow_ref_mut(cs).take()) else {
        return;
    };
    let words = &previous.words;
    esp_println::println!(
        "bench: crumbs reset_reason={:?} panicked={} events={}",
        previous.reset_reason,
        words[2],
        words[1]
    );
    if words[2] != 0 {
        esp_println::println!("bench: crumbs panic_stack={:08x?}", &words[3..HEADER]);
    }
    let total = words[1] as usize;
    for i in total.saturating_sub(RING)..total {
        let slot = HEADER + 2 * (i % RING);
        esp_println::println!(
            "bench: crumb ms={} event={} value={}",
            words[slot + 1],
            words[slot] >> 24,
            words[slot] & 0xFF_FFFF
        );
    }
}

/// Called from `custom_halt`: keeps the stack words that look like code
/// addresses (flash text or IRAM), the panic's call chain, for the next
/// boot to print.
pub fn panicked() {
    let sp: usize;
    // SAFETY: reads the stack pointer only.
    #[allow(unsafe_code)]
    unsafe {
        core::arch::asm!("mv {}, sp", out(reg) sp)
    };
    critical_section::with(|_| {
        set(2, 1);
        let mut found = 0;
        for offset in 0..1024usize {
            // SAFETY: reads up the current stack, which is mapped DRAM; the
            // walk stops well before its top (44 KB region, 4 KB read).
            #[allow(unsafe_code)]
            let word = unsafe { ((sp + offset * 4) as *const u32).read_volatile() };
            let code = (0x4200_0000..0x4280_0000).contains(&word)
                || (0x4038_0000..0x403E_0000).contains(&word);
            if code {
                set(3 + found, word);
                found += 1;
                if found == PANIC_WORDS {
                    break;
                }
            }
        }
    });
}

/// Thread-executor heartbeat (systimer µs), checked before each feed.
static HEARTBEAT_US: AtomicU64 = AtomicU64::new(0);
const STALL_US: u64 = 20_000_000;

pub fn start_watchdog() {
    // SAFETY: the RWDT registers are only touched here, in `feed` and in
    // `stop_watchdog`; the power task's `Rtc` never uses them.
    #[allow(unsafe_code)]
    let mut rtc = Rtc::new(unsafe { LPWR::steal() });
    rtc.rwdt
        .set_timeout(RwdtStage::Stage0, esp_hal::time::Duration::from_secs(25));
    rtc.rwdt
        .set_stage_action(RwdtStage::Stage0, RwdtStageAction::ResetSystem);
    rtc.rwdt.enable();
    rtc.rwdt.feed();
    HEARTBEAT_US.store(Instant::now().as_micros(), Ordering::Relaxed);
}

/// Before deep sleep, which this watchdog would otherwise cut short.
pub fn stop_watchdog() {
    #[allow(unsafe_code)]
    let mut rtc = Rtc::new(unsafe { LPWR::steal() });
    rtc.rwdt.disable();
}

/// From the input task, every poll: feeds while the thread executor is
/// alive, so a hang on either executor ends in a reset.
pub fn feed() {
    let now = Instant::now().as_micros();
    if now.saturating_sub(HEARTBEAT_US.load(Ordering::Relaxed)) > STALL_US {
        return;
    }
    #[allow(unsafe_code)]
    // SAFETY: see `start_watchdog`.
    let mut rtc = Rtc::new(unsafe { LPWR::steal() });
    rtc.rwdt.feed();
}

#[embassy_executor::task]
pub async fn heartbeat() {
    loop {
        HEARTBEAT_US.store(Instant::now().as_micros(), Ordering::Relaxed);
        embassy_time::Timer::after_secs(1).await;
    }
}
