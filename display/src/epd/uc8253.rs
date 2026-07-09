//! UC8253 panel controller — Xteink X3 (792x528).
//!
//! Command set, waveform LUT banks, and the framebuffer→panel transform,
//! ported from CrossPoint's production `Uc8253X3Driver` (MIT). The refresh
//! flow that drives these lives in `fw::display_flush::uc8253`; this module
//! is the panel *description* the way `epd::ssd1677` is for the X4.
//!
//! Model, versus the SSD1677: two RAM planes, DTM1 ("old") and DTM2
//! ("new"), instead of BW/RED. A fast turn diffs DTM2 against DTM1; a full
//! writes a white DTM1 baseline. Waveforms are *uploaded* per refresh as
//! five 42-byte LUTs (VCOM + the WW/BW/WB/BB transitions), not selected
//! from OTP — so `RefreshMode` maps to a LUT bank plus a CDI mode byte
//! (differential 0x29 vs absolute 0xA9). Grayscale is intentionally not
//! ported: this firmware has no grayscale reader path.
//!
//! HARDWARE-UNVERIFIED until an X3 is on the bench (see the plan, Phase 6):
//! the orientation flags below and the init/resolution bytes come straight
//! from the reference, but mirrored or offset output is the expected first
//! symptom to chase here.

use super::{fill_transformed_band_impl, RefreshMode, SpiOp};
use crate::{fb::Framebuffer, BAND_BYTES};

// --- UC8253 command set (subset used by the BW path) ---
pub const CMD_PANEL_SETTING: u8 = 0x00;
pub const CMD_POWER_SETTING: u8 = 0x01;
pub const CMD_POWER_OFF: u8 = 0x02;
pub const CMD_POWER_OFF_SEQ: u8 = 0x03;
pub const CMD_POWER_ON: u8 = 0x04;
pub const CMD_BOOSTER_SOFT_START: u8 = 0x06;
pub const CMD_DEEP_SLEEP: u8 = 0x07;
pub const CMD_DTM1: u8 = 0x10;
pub const CMD_DATA_STOP: u8 = 0x11;
pub const CMD_DISPLAY_REFRESH: u8 = 0x12;
pub const CMD_DTM2: u8 = 0x13;
pub const CMD_LUT_VCOM: u8 = 0x20;
pub const CMD_LUT_WW: u8 = 0x21;
pub const CMD_LUT_BW: u8 = 0x22;
pub const CMD_LUT_WB: u8 = 0x23;
pub const CMD_LUT_BB: u8 = 0x24;
pub const CMD_PLL_CONTROL: u8 = 0x30;
pub const CMD_VCOM_DATA_INTERVAL: u8 = 0x50;
pub const CMD_RESOLUTION: u8 = 0x61;
pub const CMD_GATE_SOURCE_START: u8 = 0x65;
pub const CMD_VCOM_DC: u8 = 0x82;
pub const CMD_LV_SELECTION: u8 = 0xE1;

/// Argument to `CMD_DEEP_SLEEP` (check-code the controller requires).
pub const DEEP_SLEEP_CHECK: u8 = 0xA5;

/// CDI (`CMD_VCOM_DATA_INTERVAL`) first byte: differential mode (fast/full
/// diff against DTM1) vs absolute mode (drive to target ignoring DTM1).
pub const CDI_DIFFERENTIAL: u8 = 0x29;
pub const CDI_ABSOLUTE: u8 = 0xA9;
/// CDI second byte, constant across every bank in the reference driver.
pub const CDI_INTERVAL: u8 = 0x07;

/// Bytes of each LUT sent to the controller.
pub const LUT_LEN: usize = 42;

/// Display SPI bus clock. CrossPoint's proven default for the UC8253;
/// papyrix reports pixel corruption above ~20 MHz, so 16 is the safe
/// reference value. Lower it first if the first bench frames are noisy.
pub const SPI_HZ: u32 = 16_000_000;

/// Panel orientation: all three transforms are applied — same as the X4's
/// SSD1677 but with MIRROR_Y added. These values are verified on hardware:
/// a flashed X3 build using MIRROR_X + MIRROR_Y + REVERSE_BITS renders
/// correctly. (The CrossPoint reference's `sendPlaneFlipped` alone —
/// MIRROR_Y only, no X mirror or bit reversal — produced a horizontally
/// mirrored, bit-reversed image on the actual panel.)
pub const MIRROR_X: bool = true;
pub const MIRROR_Y: bool = true;
pub const REVERSE_BITS: bool = true;

pub fn fill_transformed_band(fb: &Framebuffer, band_y: usize, out: &mut [u8; BAND_BYTES]) -> usize {
    fill_transformed_band_impl::<MIRROR_X, MIRROR_Y, REVERSE_BITS>(fb, band_y, out)
}

/// One waveform bank: VCOM plus the four transition LUTs, each `LUT_LEN`
/// bytes. `WW`=white→white, `BW`=black→white, `WB`=white→black, `BB`=
/// black→black in the controller's old→new convention.
pub struct LutBank {
    pub vcom: &'static [u8; LUT_LEN],
    pub ww: &'static [u8; LUT_LEN],
    pub bw: &'static [u8; LUT_LEN],
    pub wb: &'static [u8; LUT_LEN],
    pub bb: &'static [u8; LUT_LEN],
}

/// The controller init byte stream (`fw::display_flush::uc8253` replays it
/// after reset, same as `ssd1677::INIT_SEQUENCE`). Reset happens outside this
/// table (the driver issues it before replaying), so every entry is a plain
/// `SpiOp::Command`. RAM-plane clears are done separately by the driver,
/// which has the framebuffer geometry.
pub static INIT_SEQUENCE: &[SpiOp] = &[
    SpiOp::Command {
        cmd: CMD_PANEL_SETTING,
        data: &[0x3F, 0x0A],
    },
    // Resolution register: HRES 0x0318 (792), VRES 0x0258 (600). The panel
    // shows 528 rows; the controller's gate driver spans 600 and the extra
    // lines fall off-screen. Verbatim from the reference — do not "correct"
    // 600 to 528 without hardware to prove it.
    SpiOp::Command {
        cmd: CMD_RESOLUTION,
        data: &[0x03, 0x18, 0x02, 0x58],
    },
    SpiOp::Command {
        cmd: CMD_GATE_SOURCE_START,
        data: &[0x00, 0x00, 0x00, 0x00],
    },
    SpiOp::Command {
        cmd: CMD_POWER_OFF_SEQ,
        data: &[0x20],
    },
    SpiOp::Command {
        cmd: CMD_POWER_SETTING,
        data: &[0x07, 0x17, 0x3F, 0x3F, 0x17],
    },
    SpiOp::Command {
        cmd: CMD_VCOM_DC,
        data: &[0x24],
    },
    SpiOp::Command {
        cmd: CMD_BOOSTER_SOFT_START,
        data: &[0x25, 0x25, 0x3C, 0x37],
    },
    SpiOp::Command {
        cmd: CMD_PLL_CONTROL,
        data: &[0x09],
    },
    SpiOp::Command {
        cmd: CMD_LV_SELECTION,
        data: &[0x02],
    },
];

/// The four BW banks the refresh flow uses. `full` writes a quality frame
/// from a white DTM1 baseline; `fast` is the turbo differential page-turn;
/// `half` is the scrub bank (WW==BW, WB==BB → drive every pixel to target
/// ignoring DTM1) that realizes our `FastClean`; `normal` is the OEM
/// differential loader used for post-full settle passes.
pub fn bank_for(mode: RefreshMode) -> (&'static LutBank, u8) {
    match mode {
        RefreshMode::Full => (&FULL, CDI_DIFFERENTIAL),
        RefreshMode::Fast => (&FAST, CDI_DIFFERENTIAL),
        RefreshMode::FastClean => (&HALF, CDI_ABSOLUTE),
        // PowerDown never reaches a bank load; sleep_panel handles it.
        RefreshMode::PowerDown => (&NORMAL, CDI_ABSOLUTE),
    }
}

/// Controller RAM plane selected by a flush operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RamPlane {
    Old,
    New,
}

/// Bytes supplied to a RAM-plane write by the platform executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameSource {
    White,
    Current,
    Previous,
}

/// One controller-level operation in a UC8253 refresh.
///
/// This is the single source of truth for refresh sequencing. The firmware
/// executor turns these operations into async SPI transfers, while the host
/// panel model consumes the same operations to validate the transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushStep {
    LoadBank(RefreshMode),
    WritePlane {
        plane: RamPlane,
        source: FrameSource,
    },
    DataStop,
    PowerOn,
    DisplayRefresh,
    DelayMs(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushPlan {
    pub requested_mode: RefreshMode,
    pub effective_mode: RefreshMode,
    pub steps: &'static [FlushStep],
}

/// Settle delay after a non-fast refresh, matching the reference's 200 ms.
const SETTLE_MS: u16 = 200;

const FULL_STEPS: &[FlushStep] = &[
    FlushStep::LoadBank(RefreshMode::Full),
    FlushStep::WritePlane {
        plane: RamPlane::Old,
        source: FrameSource::White,
    },
    FlushStep::DataStop,
    FlushStep::WritePlane {
        plane: RamPlane::New,
        source: FrameSource::Current,
    },
    FlushStep::PowerOn,
    FlushStep::DisplayRefresh,
    FlushStep::DelayMs(SETTLE_MS),
    FlushStep::WritePlane {
        plane: RamPlane::Old,
        source: FrameSource::Current,
    },
    FlushStep::DataStop,
    FlushStep::LoadBank(RefreshMode::Fast),
    FlushStep::WritePlane {
        plane: RamPlane::New,
        source: FrameSource::Current,
    },
    FlushStep::DisplayRefresh,
    FlushStep::WritePlane {
        plane: RamPlane::Old,
        source: FrameSource::Current,
    },
    FlushStep::DataStop,
];

const FAST_STAGED_STEPS: &[FlushStep] = &[
    FlushStep::LoadBank(RefreshMode::Fast),
    FlushStep::WritePlane {
        plane: RamPlane::New,
        source: FrameSource::Current,
    },
    FlushStep::DisplayRefresh,
];

const FAST_UNSTAGED_STEPS: &[FlushStep] = &[
    FlushStep::LoadBank(RefreshMode::Fast),
    FlushStep::WritePlane {
        plane: RamPlane::Old,
        source: FrameSource::Previous,
    },
    FlushStep::DataStop,
    FlushStep::WritePlane {
        plane: RamPlane::New,
        source: FrameSource::Current,
    },
    FlushStep::DisplayRefresh,
];

const CLEAN_POWERED_STEPS: &[FlushStep] = &[
    FlushStep::LoadBank(RefreshMode::FastClean),
    FlushStep::WritePlane {
        plane: RamPlane::New,
        source: FrameSource::Current,
    },
    FlushStep::DisplayRefresh,
    FlushStep::DelayMs(SETTLE_MS),
];

const CLEAN_POWER_ON_STEPS: &[FlushStep] = &[
    FlushStep::LoadBank(RefreshMode::FastClean),
    FlushStep::WritePlane {
        plane: RamPlane::New,
        source: FrameSource::Current,
    },
    FlushStep::PowerOn,
    FlushStep::DisplayRefresh,
    FlushStep::DelayMs(SETTLE_MS),
];

/// Select the exact controller operation stream for one requested refresh.
/// A differential Fast request cannot run with the charge pump off because
/// DTM1 is no longer a trustworthy copy of the displayed frame; the proven
/// driver promotes that case to the absolute FastClean waveform.
/// `screen_powered` must reflect whether the charge pump is on *right now*
/// (mirrored by the caller's own `power_on`/`power_off`), not a cached or
/// derived guess: `FAST_STAGED_STEPS`/`FAST_UNSTAGED_STEPS` contain no
/// `PowerOn` step, so a stale `true` here would run `DisplayRefresh` into an
/// unpowered panel instead of being promoted to `FastClean`.
pub const fn flush_plan(
    requested_mode: RefreshMode,
    screen_powered: bool,
    previous_staged: bool,
) -> FlushPlan {
    let effective_mode = if matches!(requested_mode, RefreshMode::Fast) && !screen_powered {
        RefreshMode::FastClean
    } else {
        requested_mode
    };
    let steps = match effective_mode {
        RefreshMode::Full | RefreshMode::PowerDown => FULL_STEPS,
        RefreshMode::Fast if previous_staged => FAST_STAGED_STEPS,
        RefreshMode::Fast => FAST_UNSTAGED_STEPS,
        RefreshMode::FastClean if screen_powered => CLEAN_POWERED_STEPS,
        RefreshMode::FastClean => CLEAN_POWER_ON_STEPS,
    };
    FlushPlan {
        requested_mode,
        effective_mode,
        steps,
    }
}

pub const PRESTAGE_STEPS: &[FlushStep] = &[
    FlushStep::WritePlane {
        plane: RamPlane::Old,
        source: FrameSource::Current,
    },
    FlushStep::DataStop,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepStep {
    PowerOff,
    DeepSleep,
}

const SLEEP_POWERED_STEPS: &[SleepStep] = &[SleepStep::PowerOff, SleepStep::DeepSleep];
const SLEEP_UNPOWERED_STEPS: &[SleepStep] = &[SleepStep::DeepSleep];

pub const fn sleep_plan(screen_powered: bool) -> &'static [SleepStep] {
    if screen_powered {
        SLEEP_POWERED_STEPS
    } else {
        SLEEP_UNPOWERED_STEPS
    }
}

pub static NORMAL: LutBank = LutBank {
    vcom: &VCOM_NORMAL,
    ww: &WW_NORMAL,
    bw: &BW_NORMAL,
    wb: &WB_NORMAL,
    bb: &BB_NORMAL,
};
pub static HALF: LutBank = LutBank {
    vcom: &VCOM_HALF,
    ww: &WW_HALF,
    bw: &BW_HALF,
    wb: &WB_HALF,
    bb: &BB_HALF,
};
pub static FAST: LutBank = LutBank {
    vcom: &VCOM_FAST,
    ww: &WW_FAST,
    bw: &BW_FAST,
    wb: &WB_FAST,
    bb: &BB_FAST,
};
pub static FULL: LutBank = LutBank {
    vcom: &VCOM_FULL,
    ww: &WW_FULL,
    bw: &BW_FULL,
    wb: &WB_FULL,
    bb: &BB_FULL,
};

// Waveform tables, verbatim from CrossPoint's Uc8253X3Luts.h (community-sdk
// `main` lineage). Grayscale banks (_gc, _aa_pre_bw_mid) are omitted. Do not
// reformat: the 15/15/12 grouping mirrors the source for diff-ability.
#[rustfmt::skip]
mod tables {
    use super::LUT_LEN;

    pub(super) static VCOM_NORMAL: [u8; LUT_LEN] = [
        0x00, 0x06, 0x01, 0x06, 0x06, 0x01, 0x00, 0x04, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static WW_NORMAL: [u8; LUT_LEN] = [
        0x20, 0x06, 0x01, 0x06, 0x06, 0x01, 0x00, 0x04, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static BW_NORMAL: [u8; LUT_LEN] = [
        0xAA, 0x06, 0x01, 0x06, 0x06, 0x01, 0xA0, 0x04, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static WB_NORMAL: [u8; LUT_LEN] = [
        0x55, 0x06, 0x01, 0x06, 0x06, 0x01, 0x50, 0x04, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static BB_NORMAL: [u8; LUT_LEN] = [
        0x00, 0x06, 0x01, 0x06, 0x06, 0x01, 0x04, 0x04, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

    pub(super) static VCOM_HALF: [u8; LUT_LEN] = [
        0x00, 0x06, 0x01, 0x06, 0x06, 0x01, 0x00, 0x04, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static WW_HALF: [u8; LUT_LEN] = [
        0xAA, 0x06, 0x01, 0x06, 0x06, 0x01, 0xA0, 0x04, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static BW_HALF: [u8; LUT_LEN] = [
        0xAA, 0x06, 0x01, 0x06, 0x06, 0x01, 0xA0, 0x04, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static WB_HALF: [u8; LUT_LEN] = [
        0x55, 0x06, 0x01, 0x06, 0x06, 0x01, 0x50, 0x04, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static BB_HALF: [u8; LUT_LEN] = [
        0x55, 0x06, 0x01, 0x06, 0x06, 0x01, 0x50, 0x04, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

    pub(super) static VCOM_FAST: [u8; LUT_LEN] = [
        0x00, 0x04, 0x02, 0x04, 0x04, 0x01, 0x00, 0x04, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static WW_FAST: [u8; LUT_LEN] = [
        0x20, 0x04, 0x02, 0x04, 0x04, 0x01, 0x00, 0x04, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static BW_FAST: [u8; LUT_LEN] = [
        0xAA, 0x04, 0x02, 0x04, 0x04, 0x01, 0x80, 0x04, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static WB_FAST: [u8; LUT_LEN] = [
        0x55, 0x04, 0x02, 0x04, 0x04, 0x01, 0x40, 0x04, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static BB_FAST: [u8; LUT_LEN] = [
        0x10, 0x04, 0x02, 0x04, 0x04, 0x01, 0x00, 0x04, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

    pub(super) static VCOM_FULL: [u8; LUT_LEN] = [
        0x00, 0x18, 0x04, 0x0E, 0x0A, 0x01, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static WW_FULL: [u8; LUT_LEN] = [
        0x4A, 0x18, 0x04, 0x0E, 0x0A, 0x01, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static BW_FULL: [u8; LUT_LEN] = [
        0x0A, 0x18, 0x04, 0x0E, 0x0A, 0x01, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static WB_FULL: [u8; LUT_LEN] = [
        0x04, 0x18, 0x04, 0x0E, 0x0A, 0x01, 0x40, 0x0A, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    pub(super) static BB_FULL: [u8; LUT_LEN] = [
        0x84, 0x18, 0x04, 0x0E, 0x0A, 0x01, 0x40, 0x0A, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
}

use tables::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FB_BYTES, HEIGHT, ROW_BYTES};

    #[test]
    fn fast_with_charge_pump_off_is_an_absolute_clean_plan() {
        let plan = flush_plan(RefreshMode::Fast, false, true);
        assert_eq!(plan.effective_mode, RefreshMode::FastClean);
        assert_eq!(plan.steps, CLEAN_POWER_ON_STEPS);
        assert_eq!(
            plan.steps
                .iter()
                .filter(|step| matches!(step, FlushStep::DisplayRefresh))
                .count(),
            1
        );
        assert!(!plan.steps.iter().any(|step| matches!(
            step,
            FlushStep::WritePlane {
                plane: RamPlane::Old,
                ..
            }
        )));
    }

    #[test]
    fn fast_plan_only_writes_previous_plane_when_not_prestaged() {
        let staged = flush_plan(RefreshMode::Fast, true, true);
        let unstaged = flush_plan(RefreshMode::Fast, true, false);
        assert_eq!(staged.steps, FAST_STAGED_STEPS);
        assert_eq!(unstaged.steps, FAST_UNSTAGED_STEPS);
        assert!(!staged.steps.contains(&FlushStep::DataStop));
        assert!(unstaged.steps.contains(&FlushStep::WritePlane {
            plane: RamPlane::Old,
            source: FrameSource::Previous,
        }));
    }

    #[test]
    fn clean_plan_is_absolute_and_never_rewrites_old_plane() {
        let plan = flush_plan(RefreshMode::FastClean, true, true);
        assert_eq!(plan.steps, CLEAN_POWERED_STEPS);
        let (_, cdi) = bank_for(plan.effective_mode);
        assert_eq!(cdi, CDI_ABSOLUTE);
        assert!(!plan.steps.iter().any(|step| matches!(
            step,
            FlushStep::WritePlane {
                plane: RamPlane::Old,
                ..
            }
        )));
    }

    #[test]
    fn full_plan_keeps_the_proven_two_refresh_settle_sequence() {
        let plan = flush_plan(RefreshMode::Full, true, true);
        assert_eq!(plan.steps, FULL_STEPS);
        assert_eq!(
            plan.steps
                .iter()
                .filter(|step| matches!(step, FlushStep::DisplayRefresh))
                .count(),
            2
        );
        assert_eq!(
            plan.steps.last(),
            Some(&FlushStep::DataStop),
            "full settle must leave DTM1 synchronized"
        );
    }

    #[test]
    fn prestage_and_sleep_plans_pin_plane_and_power_ordering() {
        assert_eq!(
            PRESTAGE_STEPS,
            &[
                FlushStep::WritePlane {
                    plane: RamPlane::Old,
                    source: FrameSource::Current,
                },
                FlushStep::DataStop,
            ]
        );
        assert_eq!(
            sleep_plan(true),
            &[SleepStep::PowerOff, SleepStep::DeepSleep]
        );
        assert_eq!(sleep_plan(false), &[SleepStep::DeepSleep]);
    }

    #[test]
    fn transform_maps_asymmetric_corners_and_emits_short_final_band() {
        let mut fb = Framebuffer::new();
        fb.set_pixel(0, HEIGHT - 1, false);
        fb.set_pixel(crate::WIDTH - 1, 0, false);

        let mut band = [0xAA; crate::BAND_BYTES];
        let first_len = fill_transformed_band(&fb, 0, &mut band);
        assert_eq!(first_len, crate::BAND_BYTES);
        assert_eq!(band[ROW_BYTES - 1], 0xFE);

        let final_y = crate::BAND_ROWS * (HEIGHT / crate::BAND_ROWS);
        let final_len = fill_transformed_band(&fb, final_y, &mut band);
        assert_eq!(final_len, (HEIGHT - final_y) * ROW_BYTES);
        assert_eq!(final_len, 48 * ROW_BYTES);
        assert_eq!(band[(HEIGHT - final_y - 1) * ROW_BYTES], 0x7F);
        assert_eq!(FB_BYTES, 52_272);
    }
}
