//! Panel flush backends behind the display task's seam.
//!
//! The display task drives four operations — `init_panel`, `flush`,
//! `prestage_previous`, `sleep_panel` — plus the shared `Epd` bus type.
//! Which controller implements them is a compile-time device choice; the
//! task never sees command bytes or RAM-plane names. Both controllers keep
//! a previous-frame plane for differential fast refreshes (SSD1677: RED
//! RAM; UC8253: DTM1), which is what `prestage_previous` and `flush`'s
//! `prev_staged` speak to.

use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::SpiDmaBus;
use esp_hal::Async;

#[cfg(not(feature = "device-x3"))]
mod ssd1677;
#[cfg(feature = "device-x3")]
mod uc8253;

#[cfg(not(feature = "device-x3"))]
pub(crate) use ssd1677::{flush, init_panel, prestage_previous, sleep_panel};
#[cfg(feature = "device-x3")]
pub(crate) use uc8253::{flush, init_panel, prestage_previous, sleep_panel};

pub(crate) type Epd = hal_ext::spi_dma::EpdBus<
    SpiDmaBus<'static, Async>,
    Output<'static>,
    Output<'static>,
    Input<'static>,
    Output<'static>,
>;

pub(crate) type SpiError = <SpiDmaBus<'static, Async> as embedded_hal_async::spi::ErrorType>::Error;

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum PanelError {
    Spi(SpiError),
    Busy(hal_ext::spi_dma::BusyError),
}

impl From<esp_hal::spi::Error> for PanelError {
    fn from(value: esp_hal::spi::Error) -> Self {
        Self::Spi(value)
    }
}

impl From<hal_ext::spi_dma::BusyError> for PanelError {
    fn from(value: hal_ext::spi_dma::BusyError) -> Self {
        Self::Busy(value)
    }
}
