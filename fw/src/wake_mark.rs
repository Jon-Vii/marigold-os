//! Carries the sleep screen across deep sleep.
//!
//! Deep sleep reboots the chip, so the refresh planner forgets that the
//! panel was left on the sleep screen this firmware drew, and the first
//! paint after waking would run the deep multi-flash waveform a cold boot
//! needs. The mark keeps what a wake needs alive: that the sleep screen is
//! up, and whether the reader slept on a reading page. The display task
//! sets it when a sleep transition finishes and boot consumes it.
//!
//! esp-hal's deep-sleep configuration powers RTC fast memory down, but the
//! RTC store registers stay on. STORE0 is the one nobody else claims:
//! esp-hal, the ROM and the bootloader use STORE1-7 (slow-clock calibration,
//! boot time, clock frequencies, the wake-stub entry and its CRC).

use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::SocResetReason;
use portable_atomic::{AtomicU32, Ordering};

/// What a finished sleep transition left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SleepScreen {
    /// The reader was on a reading page when the sleep began.
    pub reading: bool,
}

/// STORE0 holds this bit and `reading`. A register that was never written,
/// or was cleared, reads as zero.
const MARKED: u32 = 1 << 1;
const READING: u32 = 1 << 0;

/// This boot's mark, read once: `UNREAD` until [`at_boot`] runs.
static BOOT_MARK: AtomicU32 = AtomicU32::new(UNREAD);
/// Never a stored value: a mark has `MARKED` set, and no mark is zero.
const UNREAD: u32 = READING;

/// Records the finished sleep screen now on the panel, or `None` when the
/// panel holds anything else.
pub fn set(screen: Option<SleepScreen>) {
    write(screen.map_or(0, |screen| {
        MARKED | if screen.reading { READING } else { 0 }
    }));
}

/// The sleep screen this boot woke on, if it is a wake from a deep sleep
/// that left one up. The first call clears the register, so a later cold
/// boot or a reset for any other reason never trusts a stale mark.
pub fn at_boot() -> Option<SleepScreen> {
    let mut mark = BOOT_MARK.load(Ordering::Relaxed);
    if mark == UNREAD {
        mark = critical_section::with(|_| {
            let mark = BOOT_MARK.load(Ordering::Relaxed);
            if mark != UNREAD {
                return mark;
            }
            let stored = LPWR::regs().store0().read().bits();
            write(0);
            let woke = stored & MARKED != 0
                && esp_hal::system::reset_reason() == Some(SocResetReason::CoreDeepSleep);
            let mark = if woke { stored } else { 0 };
            BOOT_MARK.store(mark, Ordering::Relaxed);
            mark
        });
    }
    (mark & MARKED != 0).then_some(SleepScreen {
        reading: mark & READING != 0,
    })
}

fn write(value: u32) {
    // SAFETY: STORE0 is a plain 32-bit retention register with no side
    // effects, used by nothing else (see the module comment).
    #[allow(unsafe_code)]
    LPWR::regs().store0().write(|w| unsafe { w.bits(value) });
}
