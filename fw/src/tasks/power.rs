use crate::{DisplayCommand, PowerEvent, DISPLAY_COMMANDS, POWER_EVENTS};
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::{GPIO3, LPWR};
use esp_hal::rtc_cntl::Rtc;

/// Idle time with no input before the device puts itself into deep sleep.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest the sleep handshake waits for the display task. The display
/// always answers a Sleep (it sleeps even after a failed write or refresh),
/// so this only fires if that task is stuck; deep sleep then still happens
/// instead of leaving the device awake until the battery dies.
const SLEEP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

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
/// transition. Waiting for `DisplayAsleep` lets the display write pending
/// progress and paint its sleep image first; it answers even when either of
/// those fails.
async fn enter_sleep(rtc: &mut Rtc<'_>) {
    esp_println::println!("power: display sleep");
    DISPLAY_COMMANDS.send(DisplayCommand::Sleep).await;
    let deadline = Instant::now() + SLEEP_HANDSHAKE_TIMEOUT;

    loop {
        let event = match select(POWER_EVENTS.receive(), Timer::at(deadline)).await {
            Either::First(event) => event,
            Either::Second(()) => {
                esp_println::println!("power: display did not answer sleep; sleeping anyway");
                PowerEvent::DisplayAsleep
            }
        };
        match event {
            PowerEvent::DisplayAsleep => {
                esp_println::println!("power: deep sleep");
                let mut button = steal_wake_button();
                #[cfg(feature = "bench-serial")]
                {
                    use esp_hal::gpio::RtcPinWithResistors;
                    use esp_hal::rtc_cntl::sleep::{
                        RtcioWakeupSource, TimerWakeupSource, WakeupLevel,
                    };
                    let mut pins: [(&mut dyn RtcPinWithResistors, WakeupLevel); 1] =
                        [(&mut button, WakeupLevel::Low)];
                    let gpio = RtcioWakeupSource::new(&mut pins);
                    let timer = TimerWakeupSource::new(core::time::Duration::from_secs(2));
                    esp_println::println!("bench: serial_sleep timer_wake_ms=2000 gpio=3");
                    rtc.sleep_deep(&[&gpio, &timer]);
                }
                #[cfg(not(feature = "bench-serial"))]
                hal_ext::rtc::enter_deep_sleep_button(rtc, &mut button);
            }
            PowerEvent::Activity => return,
            // Outcomes of renders still in flight when sleep was requested;
            // they say nothing about the sleep transition itself.
            PowerEvent::DisplaySettled | PowerEvent::DisplayFailed | PowerEvent::SleepNow => {}
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
