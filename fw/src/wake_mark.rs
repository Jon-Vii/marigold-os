//! Carries "the panel shows the sleep screen" across deep sleep.
//!
//! Deep sleep reboots the chip, so the refresh planner forgets that the
//! panel was left on the sleep screen this firmware drew, and the first
//! paint after waking would run the deep multi-flash waveform a cold boot
//! needs. The mark keeps that one fact alive: the display task sets it when
//! a sleep transition finishes and boot consumes it to seed the planner.
//!
//! esp-hal's deep-sleep configuration powers RTC fast memory down, but the
//! RTC store registers stay on. STORE0 is the one nobody else claims:
//! esp-hal, the ROM and the bootloader use STORE1-7 (slow-clock calibration,
//! boot time, clock frequencies, the wake-stub entry and its CRC).

use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::SocResetReason;

/// Arbitrary tag; anything else in STORE0 reads as "unknown panel".
const SLEEP_SCREEN_SHOWN: u32 = 0x534C_5021;

/// Records whether the panel now holds the finished sleep screen.
pub fn set(shown: bool) {
    write(if shown { SLEEP_SCREEN_SHOWN } else { 0 });
}

/// True once per boot when this boot is a wake from a deep sleep that left
/// the sleep screen up. Clears the mark, so a later cold boot or a reset
/// for any other reason never trusts a stale one.
pub fn take() -> bool {
    let marked = LPWR::regs().store0().read().bits() == SLEEP_SCREEN_SHOWN;
    write(0);
    marked && esp_hal::system::reset_reason() == Some(SocResetReason::CoreDeepSleep)
}

fn write(value: u32) {
    // SAFETY: STORE0 is a plain 32-bit retention register with no side
    // effects, used by nothing else (see the module comment).
    #[allow(unsafe_code)]
    LPWR::regs().store0().write(|w| unsafe { w.bits(value) });
}
