use super::{fill_transformed_band_impl, RefreshMode, SpiOp};
use crate::{fb::Framebuffer, Rect, BAND_BYTES, HEIGHT, WIDTH};

pub const CMD_DRIVER_OUTPUT_CONTROL: u8 = 0x01;
pub const CMD_BOOSTER_SOFT_START: u8 = 0x0C;
pub const CMD_DEEP_SLEEP: u8 = 0x10;
pub const CMD_DATA_ENTRY_MODE: u8 = 0x11;
pub const CMD_SW_RESET: u8 = 0x12;
pub const CMD_TEMP_SENSOR: u8 = 0x18;
pub const CMD_WRITE_TEMPERATURE: u8 = 0x1A;
pub const CMD_MASTER_ACTIVATION: u8 = 0x20;
pub const CMD_DISPLAY_UPDATE_CTRL1: u8 = 0x21;
pub const CMD_DISPLAY_UPDATE_CTRL2: u8 = 0x22;
pub const CMD_WRITE_RAM_BW: u8 = 0x24;
pub const CMD_WRITE_RAM_RED: u8 = 0x26;
pub const CMD_BORDER_WAVEFORM: u8 = 0x3C;
pub const CMD_SET_RAM_X_RANGE: u8 = 0x44;
pub const CMD_SET_RAM_Y_RANGE: u8 = 0x45;
pub const CMD_AUTO_WRITE_BW_RAM: u8 = 0x46;
pub const CMD_AUTO_WRITE_RED_RAM: u8 = 0x47;
pub const CMD_SET_RAM_X_COUNTER: u8 = 0x4E;
pub const CMD_SET_RAM_Y_COUNTER: u8 = 0x4F;
pub const DATA_ENTRY_X_INC_Y_DEC: u8 = 0x01;

/// 90 C written to the temperature register before a FastClean activation.
/// The activation sequence skips the load-temperature bit, so the OTP LUT
/// is picked for this override instead of the sensed temperature. Same
/// trick papyrix uses on this panel for its ~1.5 s clean.
pub const FAST_CLEAN_TEMPERATURE: [u8; 2] = [0x5A, 0x00];

/// Update sequence that only re-loads the temperature register from the
/// internal sensor (enable clock + load temperature). Run after a
/// FastClean settles so later Fast partials return to sensor-accurate
/// OTP waveform timing.
pub const UPDATE_SEQUENCE_LOAD_TEMP: u8 = 0xA0;

pub const MIRROR_X: bool = true;
pub const MIRROR_Y: bool = false;
pub const REVERSE_BITS: bool = true;

/// Display SPI bus clock — the rated ceiling for this panel's fast refresh.
pub const SPI_HZ: u32 = 40_000_000;

pub static INIT_SEQUENCE: &[SpiOp] = &[
    SpiOp::Reset,
    SpiOp::Command {
        cmd: CMD_SW_RESET,
        data: &[],
    },
    SpiOp::WaitBusy,
    SpiOp::Command {
        cmd: CMD_TEMP_SENSOR,
        data: &[0x80],
    },
    SpiOp::Command {
        cmd: CMD_BOOSTER_SOFT_START,
        data: &[0xAE, 0xC7, 0xC3, 0xC0, 0x40],
    },
    SpiOp::Command {
        cmd: CMD_DRIVER_OUTPUT_CONTROL,
        data: &[
            (HEIGHT as u16 - 1) as u8,
            ((HEIGHT as u16 - 1) >> 8) as u8,
            0x02,
        ],
    },
    SpiOp::Command {
        cmd: CMD_BORDER_WAVEFORM,
        data: &[0x01],
    },
    SpiOp::Command {
        cmd: CMD_DATA_ENTRY_MODE,
        data: &[DATA_ENTRY_X_INC_Y_DEC],
    },
    SpiOp::Command {
        cmd: CMD_SET_RAM_X_RANGE,
        data: &ram_x_range(Rect::FULL),
    },
    SpiOp::Command {
        cmd: CMD_SET_RAM_Y_RANGE,
        data: &ram_y_range(Rect::FULL),
    },
    SpiOp::Command {
        cmd: CMD_AUTO_WRITE_BW_RAM,
        data: &[0xF7],
    },
    SpiOp::WaitBusy,
    SpiOp::Command {
        cmd: CMD_AUTO_WRITE_RED_RAM,
        data: &[0xF7],
    },
    SpiOp::WaitBusy,
    SpiOp::Command {
        cmd: CMD_DISPLAY_UPDATE_CTRL1,
        data: &update_control_1(RefreshMode::Full),
    },
    SpiOp::Command {
        cmd: CMD_DISPLAY_UPDATE_CTRL2,
        data: &[0xF7],
    },
];

pub const fn ram_x_range(rect: Rect) -> [u8; 4] {
    let start = rect.x;
    let end = rect.x + rect.w - 1;
    [start as u8, (start >> 8) as u8, end as u8, (end >> 8) as u8]
}

pub const fn ram_y_range(rect: Rect) -> [u8; 4] {
    let top = HEIGHT as u16 - rect.y - rect.h;
    let bottom = top + rect.h - 1;
    [
        bottom as u8,
        (bottom >> 8) as u8,
        top as u8,
        (top >> 8) as u8,
    ]
}

pub const fn ram_x_counter(rect: Rect) -> [u8; 2] {
    [rect.x as u8, (rect.x >> 8) as u8]
}

pub const fn ram_y_counter(rect: Rect) -> [u8; 2] {
    let bottom = HEIGHT as u16 - rect.y - 1;
    [bottom as u8, (bottom >> 8) as u8]
}

pub const fn update_control_2(mode: RefreshMode, screen_is_on: bool, turn_off: bool) -> u8 {
    let mut value = 0;
    if !screen_is_on {
        value |= 0xC0;
    }
    if turn_off {
        value |= 0x03;
    }
    match mode {
        RefreshMode::Full => value | 0x34,
        RefreshMode::Fast => value | 0x1C,
        // Load LUT (display mode 1) + display, deliberately without the
        // 0x20 load-temperature bit so the FAST_CLEAN_TEMPERATURE override
        // written via 0x1A decides which OTP waveform runs.
        RefreshMode::FastClean => value | 0x14,
        RefreshMode::PowerDown => 0x03,
    }
}

pub const fn update_control_1(mode: RefreshMode) -> [u8; 2] {
    match mode {
        RefreshMode::Fast => [0x00, 0x00],
        RefreshMode::Full | RefreshMode::FastClean | RefreshMode::PowerDown => [0x40, 0x00],
    }
}

pub const fn is_byte_aligned(rect: Rect) -> bool {
    rect.x & 7 == 0 && rect.w & 7 == 0 && rect.w > 0 && rect.h > 0 && rect.x < WIDTH as u16
}

pub fn fill_transformed_band(fb: &Framebuffer, band_y: usize, out: &mut [u8; BAND_BYTES]) -> usize {
    fill_transformed_band_impl::<MIRROR_X, MIRROR_Y, REVERSE_BITS>(fb, band_y, out)
}
