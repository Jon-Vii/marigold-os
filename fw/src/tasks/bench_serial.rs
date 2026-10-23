//! Development-only USB navigation through the normal reader input channel.
//! No commands access storage, display hardware, or reader state directly.

use crate::{Button, InputEvent, INPUT_EVENTS};
use core::sync::atomic::{AtomicBool, Ordering};
use embassy_time::{Instant, Timer};

/// Set by the `f` command: the display task prints the framebuffer after
/// its next render, so a bench run can see exactly what reached the panel.
pub static FB_DUMP_ARMED: AtomicBool = AtomicBool::new(false);

/// Prints `fb` as one hex line per native row (`w` for an all-white row),
/// between begin and end markers, pausing between rows so the USB console's
/// small FIFO drains instead of dropping output. `tools/bench/fb_dump.py`
/// turns a capture into a PNG.
pub async fn dump_framebuffer(fb: &display::fb::Framebuffer) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    esp_println::println!(
        "bench: fb_dump begin width={} height={} row_bytes={}",
        display::WIDTH,
        display::HEIGHT,
        display::ROW_BYTES
    );
    let mut line = [0u8; 2 * display::ROW_BYTES];
    for (y, row) in fb.bytes().chunks(display::ROW_BYTES).enumerate() {
        if row.iter().all(|&byte| byte == 0xFF) {
            esp_println::println!("fbrow {} w", y);
            Timer::after_millis(1).await;
            continue;
        }
        Timer::after_millis(3).await;
        for (i, byte) in row.iter().enumerate() {
            line[2 * i] = HEX[usize::from(byte >> 4)];
            line[2 * i + 1] = HEX[usize::from(byte & 0xF)];
        }
        let hex = core::str::from_utf8(&line).unwrap_or("");
        esp_println::println!("fbrow {} {}", y, hex);
    }
    esp_println::println!("bench: fb_dump end");
}
use esp_hal::{peripherals::USB_DEVICE, usb_serial_jtag::UsbSerialJtag};

fn command_button(command: u8) -> Option<Button> {
    match command {
        b'o' => Some(Button::Confirm),
        b'n' => Some(Button::Next),
        b'p' => Some(Button::Previous),
        b'b' => Some(Button::Back),
        b's' => Some(Button::Power),
        _ => None,
    }
}

#[embassy_executor::task]
pub async fn run(usb: USB_DEVICE<'static>) {
    // Use only RX. esp-println retains its existing USB TX logging path.
    let mut serial = UsbSerialJtag::new(usb);
    esp_println::println!("bench: serial_ready commands=onpbsf timer_wake_ms=2000");
    loop {
        // Bound interrupt-executor work even if the host floods USB input.
        for _ in 0..8 {
            let command = match serial.read_byte() {
                Ok(command) => command,
                Err(nb::Error::WouldBlock) => break,
                Err(nb::Error::Other(_)) => {
                    esp_println::println!("bench: serial_error operation=read");
                    break;
                }
            };
            if command.is_ascii_whitespace() {
                continue;
            }
            if command == b'f' {
                FB_DUMP_ARMED.store(true, Ordering::Relaxed);
                esp_println::println!("bench: fb_dump armed t_ms={}", Instant::now().as_millis());
                continue;
            }
            let Some(button) = command_button(command) else {
                esp_println::println!("bench: serial_command byte={} status=unknown", command);
                continue;
            };
            let status = if INPUT_EVENTS.try_send(InputEvent::button(button)).is_err() {
                "queue_full"
            } else {
                crate::note_button_press();
                // InputEvent::button uses the host-test battery sample (77%).
                // Identify injected input explicitly in timing/battery traces.
                esp_println::println!(
                    "bench: input button=Some({:?}) source=serial aux=2000 nav=0 page_raw=0 t_ms={}",
                    button,
                    Instant::now().as_millis()
                );
                "queued"
            };
            esp_println::println!(
                "bench: serial_command command={} button={:?} status={} t_ms={}",
                command as char,
                button,
                status,
                Instant::now().as_millis()
            );
        }
        Timer::after_millis(10).await;
    }
}
