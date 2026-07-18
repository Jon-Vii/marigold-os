use embassy_time::{Duration, Instant, Timer};
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal_async::digital::Wait;
use embedded_hal_async::spi::SpiBus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusyError {
    NeverAsserted,
    TimedOut,
}

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
    BUSY: InputPin + Wait,
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

    pub async fn wait_ready(&mut self) -> Result<(), BusyError> {
        // Give BUSY time to assert after a command before the level wait;
        // a too-early check would sail straight through a refresh.
        Timer::after_millis(1).await;
        // BUSY is active high. The interrupt-driven level wait returns
        // immediately if the pin is already low, replacing the 20 ms poll
        // loop's wake-ups and exit jitter; the ceiling matches the poll
        // loop's previous ~15 s give-up.
        embassy_time::with_timeout(Duration::from_secs(15), self.busy.wait_for_low())
            .await
            .map_err(|_| BusyError::TimedOut)?
            .map_err(|_| BusyError::TimedOut)
    }

    /// Two-phase BUSY wait for the UC8253 (Xteink X3): BUSY drops LOW while
    /// the controller works and returns HIGH when done, the opposite sense
    /// and shape of the SSD1677's active-high line. Wait for the falling
    /// edge first (bounded to 1 s — a refresh so quick it never shows LOW
    /// has already finished, so proceed), then for the return to HIGH.
    /// Mirrors CrossPoint's `BusyPolarity::X3TwoPhase` poll.
    ///
    /// Returns `(saw_low, elapsed_ms)` so callers can log whether the
    /// controller actually went busy — a command that never drops BUSY was
    /// ignored, which is the difference between "refresh ran invisibly"
    /// and "refresh never happened" during bring-up.
    pub async fn wait_two_phase(&mut self) -> Result<u64, BusyError> {
        let start = Instant::now();
        let saw_low = embassy_time::with_timeout(Duration::from_secs(1), self.busy.wait_for_low())
            .await
            .is_ok();
        if !saw_low {
            return Err(BusyError::NeverAsserted);
        }
        embassy_time::with_timeout(Duration::from_secs(30), self.busy.wait_for_high())
            .await
            .map_err(|_| BusyError::TimedOut)?
            .map_err(|_| BusyError::TimedOut)?;
        Ok(start.elapsed().as_millis())
    }

    /// Raw BUSY level sample, for bring-up probes that need the idle level
    /// rather than an edge wait (`None` if the pin read errors).
    pub fn busy_is_high(&mut self) -> Option<bool> {
        self.busy.is_high().ok()
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
