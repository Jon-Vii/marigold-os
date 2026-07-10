//! Thin wrappers around `esp_hal::rtc_cntl::Rtc` for the sleep modes the X4
//! firmware uses. Deep sleep is *terminal* — the chip reboots on wake and
//! re-enters `main()`, so that path returns `!`.

use core::time::Duration;
use esp_hal::gpio::RtcPinWithResistors;
use esp_hal::rtc_cntl::sleep::{RtcioWakeupSource, TimerWakeupSource, WakeupLevel};
use esp_hal::rtc_cntl::Rtc;
use esp_hal::system::SleepSource;

/// Whether this boot is a deep-sleep wake triggered by the armed RTC GPIO —
/// on this firmware, the Power button `enter_deep_sleep_button` arms.
///
/// Deep sleep is terminal (the chip resets into `main`), so the wake cause
/// is only readable here, at boot. The check is strict on purpose:
/// `wakeup_cause()` reports `Gpio` only when the reset reason is the
/// deep-sleep reset *and* the GPIO trigger fired, so battery pulls, crashes,
/// watchdog resets, and software resets (OTA staging) all report other
/// causes and read as `false`. Callers use this to trust panel contents the
/// sleep path drew before powering down.
pub fn woke_from_deep_sleep_gpio() -> bool {
    // SleepSource carries no PartialEq upstream; match instead of `==`.
    matches!(esp_hal::rtc_cntl::wakeup_cause(), SleepSource::Gpio)
}

/// Enters deep sleep with `wake_pin` (the active-low Power button) as the wake
/// source. The chip draws ~10–15 µA until the button is pressed, then resets
/// and reboots from `main`. Returns `!` because waking is a fresh boot, not a
/// resume — the executor and all in-RAM state are gone.
///
/// `WakeupLevel::Low` matches the button's wiring: the pin idles high through
/// its pull-up and is driven low while pressed. The C3 wake path re-enables
/// that pull-up internally, so a released button can't spuriously hold the line.
///
/// The caller owns the pin so this crate stays `forbid(unsafe_code)`; on the C3
/// the wake source must be one of the RTC GPIOs (0–5), which the Power button's
/// GPIO3 satisfies.
pub fn enter_deep_sleep_button(rtc: &mut Rtc<'_>, wake_pin: &mut dyn RtcPinWithResistors) -> ! {
    let mut wake_pins: [(&mut dyn RtcPinWithResistors, WakeupLevel); 1] =
        [(wake_pin, WakeupLevel::Low)];
    let wakeup = RtcioWakeupSource::new(&mut wake_pins);
    rtc.sleep_deep(&[&wakeup])
}

/// Light sleep with a short RTC timer wake. Keeps DRAM context, used during
/// Wi-Fi sync windows.
pub fn enter_light_sleep_timer(mut rtc: Rtc, duration: Duration) {
    let wakeup = TimerWakeupSource::new(duration);
    rtc.sleep_light(&[&wakeup]);
}
