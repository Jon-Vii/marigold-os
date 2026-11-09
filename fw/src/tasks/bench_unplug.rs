//! Development bench only (`bench-unplug`): a stretch off USB with button
//! presses and a battery log, run without a person.
//!
//! The stretch is a run of phases with light sleep off or on; off idles
//! exactly as before light sleep existed (`wfi`, 15 ms button poll), so the
//! phases compare the two on the same battery. Presses are made at the pin:
//! driving the side ladder (GPIO2) to 0 V is electrically what side Down
//! does, and each one is made as the chip is about to sleep, so with sleep
//! on it ends that sleep through the button-wake path. Presses only happen
//! on a Reading page; from Home the task opens the current book.
//!
//! With `bench-unplug-real` the stretch starts when the cable really comes
//! out, so the battery feeds the reader and its voltage slope is the
//! reader's draw. Without it, the D+ pull-up is turned off so the host sees
//! the cable pulled; on roci's USB that still powers the reader, so only
//! the timing and sleep figures mean anything there.
//!
//! The battery voltage, averaged over every input poll, is logged to flash
//! once a minute. When the host is back, the log prints.

use crate::crumbs::{self, Event};
use crate::light_sleep::{self, trace};
use embassy_time::{Duration, Instant, Timer};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_hal::peripherals::{GPIO, IO_MUX, USB_DEVICE};
use esp_storage::FlashStorage;
use portable_atomic::{AtomicU32, Ordering};

const SIDE_LADDER: usize = 2;
/// GPIO matrix signal index for "plain GPIO output".
const SIG_GPIO_OUT: u32 = 128;
const PRESS_EVERY_S: u64 = 30;
/// `(light sleep on, minutes)` per phase.
#[cfg(feature = "bench-unplug-real")]
const PHASES: [(bool, usize); 3] = [(false, 40), (true, 40), (false, 40)];
#[cfg(not(feature = "bench-unplug-real"))]
const PHASES: [(bool, usize); 2] = [(false, 25), (true, 25)];
const MINUTES: usize = {
    let mut total = 0;
    let mut i = 0;
    while i < PHASES.len() {
        total += PHASES[i].1;
        i += 1;
    }
    total
};

static BATTERY_SUM: AtomicU32 = AtomicU32::new(0);
static BATTERY_COUNT: AtomicU32 = AtomicU32::new(0);

/// From the input task, every poll.
pub fn battery_sample(mv: u16) {
    BATTERY_SUM.add(u32::from(mv), Ordering::Relaxed);
    BATTERY_COUNT.add(1, Ordering::Relaxed);
}

/// Mean battery millivolts ×10 since the last call.
fn battery_mean_x10() -> u32 {
    critical_section::with(|_| {
        let sum = BATTERY_SUM.swap(0, Ordering::Relaxed);
        let count = BATTERY_COUNT.swap(0, Ordering::Relaxed);
        if count == 0 {
            0
        } else {
            (u64::from(sum) * 10 / u64::from(count)) as u32
        }
    })
}

/// The run's log, in the `coredump` flash partition, which this firmware
/// never uses otherwise. Flash survives anything the reader can do while
/// nobody watches: a Power press, deep sleep, a flat battery. The host can
/// read it with `espflash read-flash 0xff0000 0x1000` too, so it never
/// depends on this firmware still running. A boot that finds a log with
/// minutes in it prints that log instead of starting over; erase the
/// sector to start a new run.
///
/// `[tag, real]`, at `DONE_AT` `[DONE]` once finished, and per minute at
/// `MINUTE_AT + 32 * m`: `[MINUTE, minute, mv ×10, phase, phase slept_ms,
/// phase sleeps, presses, elapsed_ms]`.
const LOG_AT: u32 = 0x00FF_0000;
const RUN_TAG: u32 = 0x4142_0004;
const DONE_AT: u32 = 32;
const DONE: u32 = 0x444F_4E45;
const MINUTE_AT: u32 = 64;
const MINUTE: u32 = 0x4D49_4E55;
const MINUTE_WORDS: usize = 8;
const _: () = assert!(MINUTE_AT as usize + 32 * MINUTES <= 4096);

fn flash() -> FlashStorage<'static> {
    // SAFETY: bench builds only; nothing else touches flash after boot.
    #[allow(unsafe_code)]
    FlashStorage::new(unsafe { esp_hal::peripherals::FLASH::steal() })
}

fn read_words<const N: usize>(offset: u32) -> [u32; N] {
    let mut bytes = [0u8; 32];
    let mut words = [0u32; N];
    let _ = flash().read(LOG_AT + offset, &mut bytes[..4 * N]);
    for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(4)) {
        *word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    words
}

fn write_words(offset: u32, words: &[u32]) {
    let mut bytes = [0u8; 32];
    for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    let _ = flash().write(LOG_AT + offset, &bytes[..4 * words.len()]);
}

fn minute_record(minute: usize) -> Option<[u32; MINUTE_WORDS]> {
    let record = read_words::<MINUTE_WORDS>(MINUTE_AT + 32 * minute as u32);
    (record[0] == MINUTE && record[1] == minute as u32 + 1).then_some(record)
}

fn logged_minutes() -> usize {
    if read_words::<1>(0)[0] != RUN_TAG {
        return 0;
    }
    (0..MINUTES)
        .take_while(|&m| minute_record(m).is_some())
        .count()
}

/// Counts as activity once a minute, so the idle timeout never deep-sleeps
/// the reader while a run is pending or unread: asleep, it could not be
/// read over USB without someone pressing Power.
fn keep_awake() {
    static LAST_MS: AtomicU32 = AtomicU32::new(0);
    let now = Instant::now().as_millis() as u32;
    if now.wrapping_sub(LAST_MS.load(Ordering::Relaxed)) > 60_000 {
        LAST_MS.store(now, Ordering::Relaxed);
        let _ = crate::POWER_EVENTS.try_send(crate::PowerEvent::Activity);
    }
}

#[embassy_executor::task]
pub async fn run() -> ! {
    let real = cfg!(feature = "bench-unplug-real");
    if logged_minutes() == 0 {
        measure(real).await;
    }
    // Print with a host attached, and again every minute after, so a host
    // that attaches late still gets the log.
    loop {
        while !light_sleep::usb_host_seen() {
            keep_awake();
            Timer::after_millis(500).await;
        }
        keep_awake();
        Timer::after_secs(5).await;
        let minutes = logged_minutes();
        let last = minutes
            .checked_sub(1)
            .and_then(minute_record)
            .unwrap_or_default();
        esp_println::println!(
            "bench: unplug end real={} finished={} elapsed_ms={} presses={} minutes={}",
            read_words::<2>(0)[1] != 0,
            read_words::<1>(DONE_AT)[0] == DONE,
            last[7],
            last[6],
            minutes
        );
        for (phase, &(sleep_on, phase_minutes)) in PHASES.iter().enumerate() {
            let record = (0..minutes)
                .rev()
                .filter_map(minute_record)
                .find(|record| record[3] == phase as u32)
                .unwrap_or_default();
            esp_println::println!(
                "bench: unplug_phase index={} light_sleep={} minutes={} slept_ms={} sleeps={}",
                phase,
                sleep_on,
                phase_minutes,
                record[4],
                record[5]
            );
        }
        for minute in 0..minutes {
            let record = minute_record(minute).unwrap_or_default();
            esp_println::println!(
                "bench: unplug_battery minute={} mv_x10={}",
                minute + 1,
                record[2]
            );
            Timer::after_millis(5).await;
        }
        Timer::after_secs(60).await;
    }
}

async fn measure(real: bool) {
    let _ = flash().erase(LOG_AT, LOG_AT + 4096);
    // Boot paint, and time for the host to open a book.
    Timer::after_secs(30).await;
    if real {
        esp_println::println!("bench: unplug waiting for the cable to come out");
        // Out for ten seconds running, not a glitch.
        let mut out_since: Option<Instant> = None;
        loop {
            keep_awake();
            Timer::after_millis(500).await;
            if light_sleep::usb_host_seen() {
                out_since = None;
            } else if out_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(10)
            {
                break;
            }
        }
    } else {
        set_pullup(false);
        light_sleep::FAKE_UNPLUGGED.store(true, Ordering::Relaxed);
    }
    esp_println::println!("bench: unplug start real={} minutes={}", real, MINUTES);
    crumbs::note(Event::FakeUnplug, 1);
    write_words(0, &[RUN_TAG, u32::from(real)]);

    let started = Instant::now();
    let mut presses = 0u32;
    let mut next_press = started + Duration::from_secs(PRESS_EVERY_S);
    let mut minute = 0;
    for (phase, &(sleep_on, phase_minutes)) in PHASES.iter().enumerate() {
        light_sleep::SLEEP_OFF.store(!sleep_on, Ordering::Relaxed);
        let (slept_before, sleeps_before) = light_sleep::stats();
        battery_mean_x10();
        for _ in 0..phase_minutes {
            let minute_end = started + Duration::from_secs(60 * (minute as u64 + 1));
            while next_press < minute_end {
                Timer::at(next_press).await;
                next_press += Duration::from_secs(PRESS_EVERY_S);
                keep_awake();
                if trace::HOME.load(Ordering::Relaxed) {
                    open_book();
                } else if trace::READING.load(Ordering::Relaxed) {
                    press().await;
                    presses += 1;
                }
            }
            Timer::at(minute_end).await;
            let (slept_now, sleeps_now) = light_sleep::stats();
            write_words(
                MINUTE_AT + 32 * minute as u32,
                &[
                    MINUTE,
                    minute as u32 + 1,
                    battery_mean_x10(),
                    phase as u32,
                    ((slept_now - slept_before) / 1000) as u32,
                    sleeps_now - sleeps_before,
                    presses,
                    started.elapsed().as_millis() as u32,
                ],
            );
            minute += 1;
        }
    }
    light_sleep::SLEEP_OFF.store(false, Ordering::Relaxed);
    if !real {
        light_sleep::FAKE_UNPLUGGED.store(false, Ordering::Relaxed);
        set_pullup(true);
    }
    crumbs::note(Event::FakeUnplug, 0);
    write_words(DONE_AT, &[DONE]);
}

/// On Home, Confirm continues the current book.
fn open_book() {
    if crate::INPUT_EVENTS
        .try_send(crate::InputEvent::button(crate::Button::Confirm))
        .is_ok()
    {
        crate::note_button_press();
    }
}

/// One side Down press, made as the chip is about to sleep.
async fn press() {
    crumbs::note(Event::SyntheticPress, 1);
    let saved = drive_low();
    trace::PIN_LOW_US.store(Instant::now().as_micros(), Ordering::Relaxed);
    light_sleep::SYNTHETIC_WAKE.store(true, Ordering::Relaxed);
    Timer::after_millis(120).await;
    release(saved);
}

fn set_pullup(on: bool) {
    USB_DEVICE::regs().conf0().modify(|_, w| {
        if on {
            w.dp_pullup().set_bit().pad_pull_override().clear_bit()
        } else {
            w.pad_pull_override().set_bit().dp_pullup().clear_bit()
        }
    });
}

/// The pin's IO_MUX and output-select settings before the press.
struct Saved {
    io_mux: u32,
    out_sel: u32,
}

fn drive_low() -> Saved {
    let gpio = GPIO::regs();
    let pad = IO_MUX::regs().gpio(SIDE_LADDER);
    let saved = Saved {
        io_mux: pad.read().bits(),
        out_sel: gpio.func_out_sel_cfg(SIDE_LADDER).read().bits(),
    };
    #[allow(unsafe_code)]
    // SAFETY: GPIO2 is the side button ladder; pulling it to ground is what
    // its Down button does. The pad is set back in `release`.
    unsafe {
        gpio.out_w1tc().write(|w| w.bits(1 << SIDE_LADDER));
        gpio.func_out_sel_cfg(SIDE_LADDER)
            .write(|w| w.bits(SIG_GPIO_OUT));
        pad.modify(|_, w| w.mcu_sel().bits(1));
        gpio.enable_w1ts().write(|w| w.bits(1 << SIDE_LADDER));
    }
    saved
}

fn release(saved: Saved) {
    let gpio = GPIO::regs();
    #[allow(unsafe_code)]
    // SAFETY: restores what `drive_low` changed.
    unsafe {
        gpio.enable_w1tc().write(|w| w.bits(1 << SIDE_LADDER));
        gpio.func_out_sel_cfg(SIDE_LADDER)
            .write(|w| w.bits(saved.out_sel));
        IO_MUX::regs()
            .gpio(SIDE_LADDER)
            .write(|w| w.bits(saved.io_mux));
    }
}
