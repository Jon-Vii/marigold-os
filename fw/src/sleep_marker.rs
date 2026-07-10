//! Deep-sleep handoff marker in RTC fast RAM.
//!
//! Deep sleep is terminal — waking reboots the chip — so a boot can only
//! consult the RTC wake cause and RTC memory. The wake cause alone proves
//! *how* the chip woke, not *what* the panel shows: the sleep handshake
//! still releases the power task to cut power when the sleep-frame flush or
//! panel-sleep command fails, because staying awake on a wedged SPI bus
//! would drain the battery. This marker records whether that final flush
//! actually settled, so the next boot trusts the panel contents only when
//! both hold: woke by the armed GPIO, and the sleep frame really landed.

use core::sync::atomic::Ordering;
use portable_atomic::AtomicU32;

/// Written only by the sleep handshake. Anything else — first-boot zeroing,
/// a reset racing the persistent-RAM zero-init, garbage after a brownout —
/// misses the magic and reads as "not settled", keeping the full waveform.
const SLEEP_IMAGE_SETTLED: u32 = 0xC0DE_51EE;

// `persistent`: zeroed once on the first power-on, then left untouched by
// the runtime across deep sleep and every reset — that retention is what
// carries the value over the deep-sleep reboot.
#[allow(unsafe_code)] // #[ram] expands to the unsafe link_section attribute.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static SLEEP_IMAGE: AtomicU32 = AtomicU32::new(0);

/// Records whether the panel was left showing a fully settled sleep frame.
/// The display task calls this just before `DisplayAsleep` releases the
/// power task to cut power; `false` on any flush or panel-sleep failure.
pub fn record_sleep_image(settled: bool) {
    let value = if settled { SLEEP_IMAGE_SETTLED } else { 0 };
    SLEEP_IMAGE.store(value, Ordering::Relaxed);
}

/// Consumes the marker: whether the previous shutdown left a settled sleep
/// frame on the panel. Clears it so one recorded sleep image can never
/// vouch for more than one boot.
pub fn take_sleep_image_settled() -> bool {
    SLEEP_IMAGE.swap(0, Ordering::Relaxed) == SLEEP_IMAGE_SETTLED
}
