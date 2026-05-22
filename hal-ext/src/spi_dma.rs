use embassy_time::{Duration, Timer};
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal_async::spi::SpiBus;

pub struct EpdBus<SPI, CS, DC, BUSY, RST> {
    spi: SPI,
    cs: CS,
    dc: DC,
    busy: BUSY,
    rst: RST,
}

impl<SPI, CS, DC, BUSY, RST> EpdBus<SPI, CS, DC, BUSY, RST>
where
    SPI: SpiBus,
    CS: OutputPin,
    DC: OutputPin,
    BUSY: InputPin,
    RST: OutputPin,
{
    pub fn new(spi: SPI, cs: CS, dc: DC, busy: BUSY, rst: RST) -> Self {
        Self {
            spi,
            cs,
            dc,
            busy,
            rst,
        }
    }

    pub async fn reset(&mut self) {
        let _ = self.rst.set_high();
        Timer::after_millis(20).await;
        let _ = self.rst.set_low();
        Timer::after_millis(2).await;
        let _ = self.rst.set_high();
        Timer::after_millis(20).await;
    }

    pub async fn command(&mut self, cmd: u8, data: &[u8]) -> Result<(), SPI::Error> {
        self.select_command();
        let command_result = self.spi.write(&[cmd]).await;
        if command_result.is_err() {
            self.deselect();
            return command_result;
        }

        if !data.is_empty() {
            let _ = self.dc.set_high();
            let data_result = self.spi.write(data).await;
            self.deselect();
            data_result
        } else {
            self.deselect();
            Ok(())
        }
    }

    pub async fn begin_ram_write(&mut self, cmd: u8) -> Result<(), SPI::Error> {
        self.select_command();
        let result = self.spi.write(&[cmd]).await;
        if result.is_ok() {
            let _ = self.dc.set_high();
        } else {
            self.deselect();
        }
        result
    }

    pub async fn ram_chunk(&mut self, data: &[u8]) -> Result<(), SPI::Error> {
        self.spi.write(data).await
    }

    pub fn end_ram_write(&mut self) {
        self.deselect();
    }

    pub async fn wait_ready(&mut self) {
        Timer::after_millis(1).await;
        let mut polls = 0u16;
        while self.busy.is_high().unwrap_or(false) {
            if polls >= 750 {
                break;
            }
            polls += 1;
            Timer::after(Duration::from_millis(20)).await;
        }
    }

    pub fn deselect_display(&mut self) {
        self.deselect();
    }

    pub fn spi_mut(&mut self) -> &mut SPI {
        &mut self.spi
    }

    fn select_command(&mut self) {
        let _ = self.dc.set_low();
        let _ = self.cs.set_low();
    }

    fn deselect(&mut self) {
        let _ = self.cs.set_high();
    }
}
