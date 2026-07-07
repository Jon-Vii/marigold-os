//! UC8253 flush backend — Xteink X3. Ported from CrossPoint's production
//! `Uc8253X3Driver` (MIT), BW path only (no grayscale reader here).
//!
//! Plane model: DTM1 holds the "old" frame, DTM2 the "new". A `Fast` turn
//! diffs DTM2 against DTM1 with the turbo bank; `Full` writes a white-DTM1
//! baseline then a settle pass; `FastClean` scrubs every pixel to target
//! with the absolute half bank. As on the X4, `prestage_previous` stages
//! the just-shown frame into the old plane (DTM1) off the page-turn
//! critical path, so the next `Fast` finds its diff base already loaded —
//! `flush`'s `prev_staged` says whether that happened.
//!
//! UNVERIFIED on hardware (plan Phase 6): the two-phase BUSY timing, panel
//! orientation, and whether the ported waveforms hold at the chosen SPI
//! clock are all first-boot iteration points.

use super::{Epd, SpiError};
use display::epd::uc8253::fill_transformed_band;
use display::epd::uc8253::{
    bank_for, flush_plan, sleep_plan, FlushStep, FrameSource, LutBank, RamPlane, SleepStep,
    CDI_INTERVAL, CMD_DATA_STOP, CMD_DEEP_SLEEP, CMD_DISPLAY_REFRESH, CMD_DTM1, CMD_DTM2,
    CMD_LUT_BB, CMD_LUT_BW, CMD_LUT_VCOM, CMD_LUT_WB, CMD_LUT_WW, CMD_POWER_OFF, CMD_POWER_ON,
    CMD_VCOM_DATA_INTERVAL, DEEP_SLEEP_CHECK, INIT_SEQUENCE, PRESTAGE_STEPS,
};
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use display::{BAND_BYTES, BAND_ROWS, HEIGHT, ROW_BYTES};
use embassy_time::Timer;
// riscv32imc has no CAS; portable-atomic provides plain load/store here.
use portable_atomic::{AtomicBool, Ordering};

/// Mirrors the controller's charge-pump state (`_isScreenOn`). The panel is
/// left powered between page turns for speed; only `sleep_panel` powers it
/// down. `init_panel` resets it to off. Single writer (the display task).
static SCREEN_POWERED: AtomicBool = AtomicBool::new(false);

pub(crate) async fn init_panel(epd: &mut Epd) {
    // Bring-up probe: a live UC8253 twitches BUSY across a hardware reset;
    // a line that reads high at every sample never left power-up, which is
    // a board/RST problem, not a command-stream one.
    esp_println::println!("display: x3 busy pre-reset high={:?}", epd.busy_is_high());
    epd.reset().await;
    esp_println::println!("display: x3 busy post-reset high={:?}", epd.busy_is_high());
    // The X3 needs an extra settle after reset beyond the shared pulse.
    Timer::after_millis(50).await;

    for (cmd, data) in INIT_SEQUENCE {
        let _ = epd.command(*cmd, data).await;
    }
    esp_println::println!("display: x3 busy post-init high={:?}", epd.busy_is_high());

    // UC8253 has no auto RAM clear; whiten both planes so the first
    // differential diffs against white rather than power-on garbage.
    let _ = fill_plane(epd, CMD_DTM1, 0xFF).await;
    let _ = epd.command(CMD_DATA_STOP, &[]).await;
    let _ = fill_plane(epd, CMD_DTM2, 0xFF).await;
    let _ = epd.command(CMD_DATA_STOP, &[]).await;

    SCREEN_POWERED.store(false, Ordering::Relaxed);
    esp_println::println!("display: x3 init done");
}

pub(crate) async fn flush(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &Framebuffer,
    tx_band: &mut [u8; BAND_BYTES],
    _screen_on: bool,
    mode: RefreshMode,
    prev_staged: bool,
) -> Result<(), SpiError> {
    let plan = flush_plan(mode, SCREEN_POWERED.load(Ordering::Relaxed), prev_staged);
    esp_println::println!(
        "display: x3 flush requested={:?} effective={:?}",
        plan.requested_mode,
        plan.effective_mode
    );
    execute_steps(epd, fb, prev_fb, tx_band, plan.steps).await
}

/// Stage the just-shown frame into DTM1 ("old" RAM) so the next fast turn's
/// diff base is loaded off the critical path. The X4's `prestage_previous`
/// analogue (RED RAM there, DTM1 here).
pub(crate) async fn prestage_previous(
    epd: &mut Epd,
    fb: &Framebuffer,
    tx_band: &mut [u8; BAND_BYTES],
) -> Result<(), SpiError> {
    execute_steps(epd, fb, fb, tx_band, PRESTAGE_STEPS).await
}

pub(crate) async fn sleep_panel(epd: &mut Epd) -> Result<(), SpiError> {
    for step in sleep_plan(SCREEN_POWERED.load(Ordering::Relaxed)) {
        match step {
            SleepStep::PowerOff => power_off(epd).await?,
            SleepStep::DeepSleep => epd.command(CMD_DEEP_SLEEP, &[DEEP_SLEEP_CHECK]).await?,
        }
    }
    Ok(())
}

async fn execute_steps(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &Framebuffer,
    tx_band: &mut [u8; BAND_BYTES],
    steps: &[FlushStep],
) -> Result<(), SpiError> {
    for step in steps {
        match *step {
            FlushStep::LoadBank(mode) => {
                let (bank, cdi) = bank_for(mode);
                load_bank(epd, cdi, bank).await?;
            }
            FlushStep::WritePlane { plane, source } => {
                let command = match plane {
                    RamPlane::Old => CMD_DTM1,
                    RamPlane::New => CMD_DTM2,
                };
                match source {
                    FrameSource::White => fill_plane(epd, command, 0xFF).await?,
                    FrameSource::Current => send_plane(epd, command, fb, tx_band).await?,
                    FrameSource::Previous => send_plane(epd, command, prev_fb, tx_band).await?,
                }
            }
            FlushStep::DataStop => epd.command(CMD_DATA_STOP, &[]).await?,
            FlushStep::PowerOn => power_on(epd).await?,
            FlushStep::DisplayRefresh => display_refresh(epd).await?,
            FlushStep::DelayMs(ms) => Timer::after_millis(u64::from(ms)).await,
        }
    }
    Ok(())
}

async fn power_on(epd: &mut Epd) -> Result<(), SpiError> {
    epd.command(CMD_POWER_ON, &[]).await?;
    let (low, ms) = epd.wait_two_phase().await;
    esp_println::println!(
        "display: x3 PON busy_low={} {}ms level_high={:?}",
        low,
        ms,
        epd.busy_is_high()
    );
    SCREEN_POWERED.store(true, Ordering::Relaxed);
    Ok(())
}

async fn display_refresh(epd: &mut Epd) -> Result<(), SpiError> {
    epd.command(CMD_DISPLAY_REFRESH, &[]).await?;
    let (low, ms) = epd.wait_two_phase().await;
    esp_println::println!(
        "display: x3 DRF busy_low={} {}ms level_high={:?}",
        low,
        ms,
        epd.busy_is_high()
    );
    Ok(())
}

async fn power_off(epd: &mut Epd) -> Result<(), SpiError> {
    epd.command(CMD_POWER_OFF, &[]).await?;
    let (low, ms) = epd.wait_two_phase().await;
    esp_println::println!("display: x3 POF busy_low={} {}ms", low, ms);
    SCREEN_POWERED.store(false, Ordering::Relaxed);
    Ok(())
}

async fn load_bank(epd: &mut Epd, cdi0: u8, bank: &LutBank) -> Result<(), SpiError> {
    epd.command(CMD_VCOM_DATA_INTERVAL, &[cdi0, CDI_INTERVAL])
        .await?;
    epd.command(CMD_LUT_VCOM, bank.vcom).await?;
    epd.command(CMD_LUT_WW, bank.ww).await?;
    epd.command(CMD_LUT_BW, bank.bw).await?;
    epd.command(CMD_LUT_WB, bank.wb).await?;
    epd.command(CMD_LUT_BB, bank.bb).await?;
    Ok(())
}

/// Stream one framebuffer into a RAM plane, band by band, in the panel's
/// row order (the shared band transform applies the X3's vertical flip).
///
/// Deliberately does NOT send DATA_STOP: the hardware-proven reference
/// never puts one between a DTM2 write and the refresh that displays it —
/// only after DTM1 syncs and white fills. Callers add it where the
/// reference does.
async fn send_plane(
    epd: &mut Epd,
    ram_cmd: u8,
    fb: &Framebuffer,
    tx_band: &mut [u8; BAND_BYTES],
) -> Result<(), SpiError> {
    epd.begin_ram_write(ram_cmd).await?;
    let mut y = 0;
    let mut result = Ok(());
    while y < HEIGHT {
        let len = fill_transformed_band(fb, y, tx_band);
        if let Err(err) = epd.ram_chunk(&tx_band[..len]).await {
            result = Err(err);
            break;
        }
        y += BAND_ROWS;
    }
    epd.end_ram_write();
    result
}

/// Fill a RAM plane with a constant byte (0xFF = white), row by row.
///
/// Deliberately does NOT send DATA_STOP, for the same reason as `send_plane`
/// above: callers add it where the reference driver does.
async fn fill_plane(epd: &mut Epd, ram_cmd: u8, fill: u8) -> Result<(), SpiError> {
    let mut row = [0u8; ROW_BYTES];
    row.fill(fill);
    epd.begin_ram_write(ram_cmd).await?;
    let mut result = Ok(());
    for _ in 0..HEIGHT {
        if let Err(err) = epd.ram_chunk(&row).await {
            result = Err(err);
            break;
        }
    }
    epd.end_ram_write();
    result
}
