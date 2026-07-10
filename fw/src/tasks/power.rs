use crate::{AppView, DisplayCommand, PowerEvent, DISPLAY_COMMANDS, POWER_EVENTS};
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::{GPIO3, LPWR};
use esp_hal::rtc_cntl::Rtc;

/// Idle time with no input before the device puts itself into deep sleep,
/// tiered by the view the last input left the app in. Reading keeps the
/// long leash for slow readers; the shell views sleep much sooner — the
/// seeded deep-sleep wake is a ~1.5 s one-flicker refresh, so an early
/// sleep costs the user little and saves the 160 MHz / 66 Hz-polling idle
/// tail (order 10–20 mA) on every walk-away. Wireless keeps the long
/// leash too: a running sync session is legitimately button-free.
const READING_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const SHELL_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

const fn idle_timeout(view: AppView) -> Duration {
    match view {
        AppView::Reading | AppView::Wireless => READING_IDLE_TIMEOUT,
        AppView::Home | AppView::Library | AppView::Chapters | AppView::Settings => {
            SHELL_IDLE_TIMEOUT
        }
    }
}

#[embassy_executor::task]
pub async fn run(lpwr: LPWR<'static>) {
    esp_println::println!("power: started");
    let mut rtc = Rtc::new(lpwr);
    // Boot lands on the Home shell, so start on the short leash.
    let mut idle = idle_timeout(AppView::Home);
    let mut deadline = Instant::now() + idle;

    loop {
        match select(POWER_EVENTS.receive(), Timer::at(deadline)).await {
            Either::First(event) => match event {
                // Any input pushes the idle deadline back out, at the leash
                // of the view the input landed in.
                PowerEvent::Activity(view) => {
                    idle = idle_timeout(view);
                    deadline = Instant::now() + idle;
                }
                // Power button: sleep on demand.
                PowerEvent::SleepNow => {
                    // Only reached if a late button press aborted the
                    // handshake; resume on that press's idle tier.
                    idle = enter_sleep(&mut rtc).await;
                    deadline = Instant::now() + idle;
                }
                PowerEvent::DisplaySettled | PowerEvent::DisplayAsleep => {}
            },
            // Idle timeout elapsed with no activity.
            Either::Second(_) => {
                esp_println::println!("power: idle timeout");
                idle = enter_sleep(&mut rtc).await;
                deadline = Instant::now() + idle;
            }
        }
    }
}

/// Renders the sleep screen, lets the display flush in-flight work and any
/// pending reading progress, then powers the SoC down into deep sleep with the
/// Power button as the wake source.
///
/// Deep sleep is terminal — the chip reboots on wake — so this only returns if
/// a button press arrives during the display handshake and aborts the
/// transition, and the returned value is the idle tier of the view that press
/// landed in, so the resumed idle clock ticks at the cancelling view's leash.
/// Waiting for `DisplayAsleep` before cutting power guarantees the e-ink panel
/// has settled on its sleep image and progress is safely on the SD card.
async fn enter_sleep(rtc: &mut Rtc<'_>) -> Duration {
    esp_println::println!("power: display sleep");
    DISPLAY_COMMANDS.send(DisplayCommand::Sleep).await;

    loop {
        match POWER_EVENTS.receive().await {
            PowerEvent::DisplayAsleep => {
                esp_println::println!("power: deep sleep");
                let mut button = steal_wake_button();
                hal_ext::rtc::enter_deep_sleep_button(rtc, &mut button);
            }
            PowerEvent::Activity(view) => return idle_timeout(view),
            PowerEvent::DisplaySettled | PowerEvent::SleepNow => {}
        }
    }
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
