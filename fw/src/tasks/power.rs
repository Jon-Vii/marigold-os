use crate::{DisplayCommand, PowerEvent, DISPLAY_COMMANDS, POWER_EVENTS};
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::{GPIO3, LPWR};
use esp_hal::rtc_cntl::Rtc;
use portable_atomic::{AtomicBool, Ordering};

/// Idle time with no input before the device puts itself into deep sleep.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest the sleep handshake waits for the display task. The display
/// always answers a Sleep (it sleeps even after a failed write or refresh),
/// so this only fires if that task is stuck; deep sleep then still happens
/// instead of leaving the device awake until the battery dies.
const SLEEP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a sleep rests in standby before deep sleep. In standby the
/// sleep screen is up and the panel is powered down, as in deep sleep, but
/// the chip only light-sleeps with the Power button as its one wake source,
/// so a reader who picks the device up again finds it where they left it
/// after one page turn's refresh instead of a reboot. Light sleep draws
/// more than deep sleep (on the order of a milliamp against microamps), so
/// the window is bounded: an hour of it costs a fraction of a percent of
/// the battery, and a reader put down for the night still spends the night
/// in deep sleep.
#[cfg(not(feature = "bench-serial"))]
const STANDBY: Duration = Duration::from_secs(3600);
/// Past this long since boot a sleep skips standby, so the wake is a boot.
/// The panel's temperature is read at boot and sets the length of a page
/// turn; waking from standby time after time would let one reading stand
/// for a whole day of carrying the reader between rooms and weather.
const STANDBY_UPTIME_LIMIT: Duration = Duration::from_secs(3 * 3600);
/// Bench builds go straight to deep sleep, which their timer wake makes
/// unattended, unless `MARIGOLD_STANDBY_SECS` gave a window at build time.
#[cfg(feature = "bench-serial")]
const STANDBY: Duration = Duration::from_secs(parse_secs(option_env!("MARIGOLD_STANDBY_SECS")));

#[cfg(feature = "bench-serial")]
const fn parse_secs(text: Option<&str>) -> u64 {
    let Some(text) = text else {
        return 0;
    };
    let bytes = text.as_bytes();
    let mut secs = 0u64;
    let mut index = 0;
    while index < bytes.len() {
        assert!(
            bytes[index].is_ascii_digit(),
            "MARIGOLD_STANDBY_SECS must be a whole number of seconds"
        );
        secs = secs * 10 + (bytes[index] - b'0') as u64;
        index += 1;
    }
    secs
}

/// Set while a sleep runs, from its start (asked for, or after the idle
/// timeout) until a press ends it.
static SLEEPING: AtomicBool = AtomicBool::new(false);

/// Whether a sleep has begun and nothing has ended it yet. The app hears
/// only the Power button meanwhile.
pub fn sleeping() -> bool {
    SLEEPING.load(Ordering::Relaxed)
}

#[embassy_executor::task]
pub async fn run(lpwr: LPWR<'static>) {
    esp_println::println!("power: started");
    let mut rtc = Rtc::new(lpwr);
    let mut deadline = Instant::now() + IDLE_TIMEOUT;

    loop {
        match select(POWER_EVENTS.receive(), Timer::at(deadline)).await {
            Either::First(event) => match event {
                // Any input pushes the idle deadline back out.
                PowerEvent::Activity => deadline = Instant::now() + IDLE_TIMEOUT,
                // Power button: sleep on demand.
                PowerEvent::SleepNow => {
                    enter_sleep(&mut rtc).await;
                    // Only reached once a Power press has ended the sleep.
                    deadline = Instant::now() + IDLE_TIMEOUT;
                }
                PowerEvent::DisplaySettled
                | PowerEvent::DisplayAsleep
                | PowerEvent::DisplayFailed => {}
            },
            // Idle timeout elapsed with no activity.
            Either::Second(_) => {
                esp_println::println!("power: idle timeout");
                enter_sleep(&mut rtc).await;
                deadline = Instant::now() + IDLE_TIMEOUT;
            }
        }
    }
}

/// Renders the sleep screen, lets the display flush in-flight work and any
/// pending reading progress, rests in standby, then powers the SoC down
/// into deep sleep with the Power button as the wake source.
///
/// Deep sleep is terminal (the chip reboots on wake), so this only returns
/// if the Power button wakes the reader from standby first. Once a sleep
/// begins the app hears no other button, so nothing else ends it; an
/// `Activity` sent just before the sleep began still does.
/// Waiting for `DisplayAsleep` lets the display write pending progress and
/// paint its sleep image first; it answers even when either of those fails.
async fn enter_sleep(rtc: &mut Rtc<'_>) {
    esp_println::println!("power: display sleep");
    SLEEPING.store(true, Ordering::Relaxed);
    DISPLAY_COMMANDS.send(DisplayCommand::Sleep).await;
    let mut deadline = Instant::now() + SLEEP_HANDSHAKE_TIMEOUT;
    let mut standing_by = false;

    loop {
        let event = match select(POWER_EVENTS.receive(), Timer::at(deadline)).await {
            Either::First(event) => event,
            Either::Second(()) if standing_by => {
                esp_println::println!("power: standby over");
                deep_sleep(rtc)
            }
            Either::Second(()) => {
                esp_println::println!("power: display did not answer sleep; sleeping anyway");
                // Whatever the panel shows now, it isn't a finished sleep
                // screen; wake must not trust an older mark.
                crate::wake_mark::set(None);
                deep_sleep(rtc)
            }
        };
        match event {
            PowerEvent::DisplayAsleep => {
                if STANDBY.as_ticks() == 0
                    || !crate::light_sleep::standby_allowed()
                    || Instant::now().as_ticks() > STANDBY_UPTIME_LIMIT.as_ticks()
                {
                    deep_sleep(rtc);
                }
                esp_println::println!("bench: standby secs={}", STANDBY.as_secs());
                crate::light_sleep::set_standby(true);
                standing_by = true;
                deadline = Instant::now() + STANDBY;
            }
            PowerEvent::Activity => {
                if standing_by {
                    esp_println::println!(
                        "bench: standby woken t_ms={}",
                        Instant::now().as_millis()
                    );
                }
                crate::light_sleep::set_standby(false);
                SLEEPING.store(false, Ordering::Relaxed);
                return;
            }
            // Outcomes of renders still in flight when sleep was requested;
            // they say nothing about the sleep transition itself.
            PowerEvent::DisplaySettled | PowerEvent::DisplayFailed | PowerEvent::SleepNow => {}
        }
    }
}

fn deep_sleep(rtc: &mut Rtc<'_>) -> ! {
    esp_println::println!("power: deep sleep");
    let mut button = steal_wake_button();
    #[cfg(feature = "bench-serial")]
    {
        use esp_hal::gpio::RtcPinWithResistors;
        use esp_hal::rtc_cntl::sleep::{RtcioWakeupSource, TimerWakeupSource, WakeupLevel};
        let mut pins: [(&mut dyn RtcPinWithResistors, WakeupLevel); 1] =
            [(&mut button, WakeupLevel::Low)];
        let gpio = RtcioWakeupSource::new(&mut pins);
        let timer = TimerWakeupSource::new(core::time::Duration::from_secs(2));
        esp_println::println!("bench: serial_sleep timer_wake_ms=2000 gpio=3");
        crate::crumbs::stop_watchdog();
        rtc.sleep_deep(&[&gpio, &timer])
    }
    #[cfg(not(feature = "bench-serial"))]
    hal_ext::rtc::enter_deep_sleep_button(rtc, &mut button)
}

/// Re-materialises the Power button (GPIO3) as a deep-sleep wake source.
///
/// SAFETY: only reached on the terminal deep-sleep path. The input task's
/// `Input<'static>` handle on GPIO3 is about to be torn down by the chip reset
/// that ends deep sleep, so this second handle never coexists with a live one.
#[allow(unsafe_code)]
fn steal_wake_button() -> GPIO3<'static> {
    unsafe { GPIO3::steal() }
}

/// Bench builds only: logs how much of the uptime went to light sleep, and
/// the presses made since the last report.
#[cfg(feature = "bench-serial")]
#[embassy_executor::task]
pub async fn report_light_sleep() {
    loop {
        Timer::after_secs(30).await;
        let (slept_us, sleeps) = crate::light_sleep::stats();
        esp_println::println!(
            "bench: light_sleep slept_ms={} sleeps={} uptime_ms={}",
            slept_us / 1000,
            sleeps,
            Instant::now().as_millis(),
        );
        crate::light_sleep::trace::print_pending();
        crate::crumbs::print_previous();
    }
}
