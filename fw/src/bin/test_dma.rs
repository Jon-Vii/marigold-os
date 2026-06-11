#![no_std]
#![no_main]
#![allow(clippy::empty_loop)]
#![allow(clippy::manual_div_ceil)] // False positive inside esp_hal::dma_buffers!.

use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::RateExtU32;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = esp_hal::dma_buffers!(8000);
    let dma_rx_buf = esp_hal::dma::DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap();
    let dma_tx_buf = esp_hal::dma::DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();

    let _spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(10_u32.MHz())
            .with_mode(esp_hal::spi::Mode::_0),
    )
    .expect("SPI2 config")
    .with_sck(peripherals.GPIO8)
    .with_mosi(peripherals.GPIO10)
    .with_dma(peripherals.DMA_CH0)
    .with_buffers(dma_rx_buf, dma_tx_buf);

    loop {}
}
