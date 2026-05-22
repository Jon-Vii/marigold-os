use crate::{DisplayCommand, PowerEvent, DISPLAY_COMMANDS, POWER_EVENTS};
use core::time::Duration;
use embassy_futures::select::{select, Either};
use embassy_time::Timer;
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::Rtc;

const IDLE_AFTER_REFRESH_MS: u64 = 300_000;
const RTC_WAKE_SECS: u64 = 600;

#[embassy_executor::task]
pub async fn run(lpwr: LPWR) {
    esp_println::println!("power: started");
    let rtc = Rtc::new(lpwr);

    loop {
        match POWER_EVENTS.receive().await {
            PowerEvent::Activity => {}
            PowerEvent::DisplaySettled => {
                if idle_window_expired().await && request_display_sleep().await {
                    hal_ext::rtc::enter_deep_sleep_timer(rtc, Duration::from_secs(RTC_WAKE_SECS));
                }
            }
            PowerEvent::DisplayAsleep => {}
            PowerEvent::SleepNow => {
                let _ = request_display_sleep().await;
            }
        }
    }
}

async fn request_display_sleep() -> bool {
    esp_println::println!("power: display sleep");
    DISPLAY_COMMANDS.send(DisplayCommand::Sleep).await;

    loop {
        match POWER_EVENTS.receive().await {
            PowerEvent::DisplayAsleep => {
                esp_println::println!("power: deep sleep");
                return true;
            }
            PowerEvent::Activity => return false,
            PowerEvent::DisplaySettled | PowerEvent::SleepNow => {}
        }
    }
}

async fn idle_window_expired() -> bool {
    match select(
        Timer::after_millis(IDLE_AFTER_REFRESH_MS),
        POWER_EVENTS.receive(),
    )
    .await
    {
        Either::First(_) => true,
        Either::Second(_) => false,
    }
}
