use crate::{DisplayCommand, PowerEvent, DISPLAY_COMMANDS, POWER_EVENTS};
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::{GPIO3, LPWR};
use esp_hal::rtc_cntl::Rtc;

/// Idle time with no input before the device puts itself into deep sleep.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

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
                    // Only reached if a late button press aborted the handshake.
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
/// pending reading progress, then powers the SoC down into deep sleep with the
/// Power button as the wake source.
///
/// Deep sleep is terminal — the chip reboots on wake — so this only returns if
/// a button press arrives during the display handshake and aborts the
/// transition. Waiting for `DisplayAsleep` before cutting power guarantees the
/// e-ink panel has settled on its sleep image and progress is safely on the SD
/// card.
async fn enter_sleep(rtc: &mut Rtc<'_>) {
    esp_println::println!("power: display sleep");
    DISPLAY_COMMANDS.send(DisplayCommand::Sleep).await;

    loop {
        match POWER_EVENTS.receive().await {
            PowerEvent::DisplayAsleep => {
                esp_println::println!("power: deep sleep");
                let mut button = steal_wake_button();
                hal_ext::rtc::enter_deep_sleep_button(rtc, &mut button);
            }
            PowerEvent::DisplayFailed => {
                esp_println::println!("power: display sleep failed; staying awake");
                return;
            }
            PowerEvent::Activity => return,
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
