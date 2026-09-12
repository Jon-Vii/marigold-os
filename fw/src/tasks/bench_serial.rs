//! Development-only USB navigation through the normal reader input channel.
//! No commands access storage, display hardware, or reader state directly.

use crate::{Button, InputEvent, INPUT_EVENTS};
use embassy_time::{Instant, Timer};
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
    esp_println::println!("bench: serial_ready commands=onpbs timer_wake_ms=2000");
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
            let Some(button) = command_button(command) else {
                esp_println::println!("bench: serial_command byte={} status=unknown", command);
                continue;
            };
            let status = if INPUT_EVENTS.try_send(InputEvent::button(button)).is_err() {
                "queue_full"
            } else {
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
