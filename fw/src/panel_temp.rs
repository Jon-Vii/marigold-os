//! The X4 panel's temperature, read once at boot from the SSD1677's own
//! sensor, which picks the page-turn waveform (`display::epd::quick_turn_frames`).
//!
//! The controller answers reads on its data line, which the board wires to
//! the SPI bus's MOSI pin, so a read releases that pin and clocks the reply
//! in by hand. That happens before the display SPI bus exists: deep sleep
//! ends in a reboot, so every wake reads the temperature afresh. The read
//! resets the controller first (it may be in deep sleep) and leaves it for
//! the normal panel init, which resets it again.

use core::sync::atomic::{AtomicI8, Ordering};
use display::epd::{
    CMD_DISPLAY_UPDATE_CTRL2, CMD_MASTER_ACTIVATION, CMD_READ_TEMPERATURE, CMD_SW_RESET,
    CMD_TEMP_SENSOR, UPDATE_SEQUENCE_LOAD_TEMP,
};
use esp_hal::delay::Delay;
use esp_hal::gpio::{
    Flex, Input, InputConfig, InputPin, Level, Output, OutputConfig, OutputPin, Pull,
};
use esp_hal::time::Instant;

/// No reading: the pins or the controller didn't answer sensibly.
const UNKNOWN: i8 = i8::MIN;

static PANEL_TEMP_C: AtomicI8 = AtomicI8::new(UNKNOWN);

/// The temperature read at boot, in whole degrees C.
pub fn celsius() -> Option<i8> {
    match PANEL_TEMP_C.load(Ordering::Relaxed) {
        UNKNOWN => None,
        temp => Some(temp),
    }
}

struct Pins<'d> {
    sck: Output<'d>,
    sda: Flex<'d>,
    cs: Output<'d>,
    dc: Output<'d>,
    busy: Input<'d>,
    delay: Delay,
}

impl Pins<'_> {
    /// Half a clock period: 500 kHz, well inside the 2.5 MHz read limit.
    fn tick(&self) {
        self.delay.delay_micros(1);
    }

    fn write_byte(&mut self, byte: u8) {
        for bit in 0..8 {
            if byte & (0x80 >> bit) != 0 {
                self.sda.set_high();
            } else {
                self.sda.set_low();
            }
            self.tick();
            self.sck.set_high();
            self.tick();
            self.sck.set_low();
        }
    }

    fn command(&mut self, cmd: u8, data: &[u8]) {
        self.cs.set_low();
        self.dc.set_low();
        self.tick();
        self.write_byte(cmd);
        self.dc.set_high();
        for &byte in data {
            self.write_byte(byte);
        }
        self.tick();
        self.cs.set_high();
        self.tick();
    }

    /// The controller shifts each bit out on a falling clock edge, so it is
    /// sampled while the clock is low, before the next rising edge.
    fn read(&mut self, cmd: u8, out: &mut [u8]) {
        self.cs.set_low();
        self.dc.set_low();
        self.tick();
        self.write_byte(cmd);
        self.dc.set_high();
        self.sda.set_output_enable(false);
        self.tick();
        self.tick();
        for slot in out.iter_mut() {
            let mut byte = 0u8;
            for _ in 0..8 {
                self.tick();
                byte = (byte << 1) | u8::from(self.sda.is_high());
                self.sck.set_high();
                self.tick();
                self.sck.set_low();
            }
            *slot = byte;
        }
        self.cs.set_high();
        self.sda.set_output_enable(true);
        self.tick();
    }

    fn wait_busy(&self, limit_ms: u64) -> bool {
        let start = Instant::now();
        self.delay.delay_micros(5);
        while self.busy.is_high() {
            if start.elapsed().as_millis() > limit_ms {
                return false;
            }
        }
        true
    }
}

/// Reads the panel temperature and keeps it for [`celsius`]. Takes about
/// 10 ms. Any failure leaves the temperature unknown, which keeps the
/// factory page-turn waveform.
pub fn read_at_boot(
    sck: impl OutputPin,
    sda: impl InputPin + OutputPin,
    cs: impl OutputPin,
    dc: impl OutputPin,
    rst: impl OutputPin,
    busy: impl InputPin,
) {
    let mut sda = Flex::new(sda);
    sda.apply_output_config(&OutputConfig::default());
    sda.apply_input_config(&InputConfig::default().with_pull(Pull::Up));
    sda.set_input_enable(true);
    sda.set_output_enable(true);
    let mut pins = Pins {
        sck: Output::new(sck, Level::Low, OutputConfig::default()),
        sda,
        cs: Output::new(cs, Level::High, OutputConfig::default()),
        dc: Output::new(dc, Level::High, OutputConfig::default()),
        busy: Input::new(busy, InputConfig::default()),
        delay: Delay::new(),
    };
    let mut rst = Output::new(rst, Level::High, OutputConfig::default());
    rst.set_low();
    pins.delay.delay_millis(2);
    rst.set_high();
    pins.delay.delay_millis(2);
    let mut temp = [0u8; 2];
    let answered = pins.wait_busy(100)
        && {
            pins.command(CMD_SW_RESET, &[]);
            pins.wait_busy(100)
        }
        && {
            // Internal sensor, then a sequence that only loads its reading.
            pins.command(CMD_TEMP_SENSOR, &[0x80]);
            pins.command(CMD_DISPLAY_UPDATE_CTRL2, &[UPDATE_SEQUENCE_LOAD_TEMP]);
            pins.command(CMD_MASTER_ACTIVATION, &[]);
            pins.wait_busy(100)
        };
    if answered {
        pins.read(CMD_READ_TEMPERATURE, &mut temp);
    }
    // Whole degrees, two's complement, in the first byte. A line nobody
    // drives reads all ones, which would pass as -1 C, so that pair is
    // rejected outright.
    let whole = temp[0] as i8;
    let plausible = answered && temp != [0xFF, 0xFF] && (-30..=70).contains(&whole);
    if plausible {
        PANEL_TEMP_C.store(whole, Ordering::Relaxed);
    }
    esp_println::println!(
        "display: panel temperature raw={:02x}{:02x} celsius={:?}",
        temp[0],
        temp[1],
        celsius()
    );
}
