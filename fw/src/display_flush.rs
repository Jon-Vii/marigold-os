use display::epd::{
    fill_transformed_band, ram_x_counter, ram_x_range, ram_y_counter, ram_y_range,
    update_control_1, update_control_2, RefreshMode, SpiOp, CMD_DEEP_SLEEP,
    CMD_DISPLAY_UPDATE_CTRL1, CMD_DISPLAY_UPDATE_CTRL2, CMD_MASTER_ACTIVATION,
    CMD_SET_RAM_X_COUNTER, CMD_SET_RAM_X_RANGE, CMD_SET_RAM_Y_COUNTER, CMD_SET_RAM_Y_RANGE,
    CMD_WRITE_RAM_BW, CMD_WRITE_RAM_RED, INIT_SEQUENCE,
};
use display::fb::Framebuffer;
use display::{Rect, BAND_BYTES, BAND_ROWS, HEIGHT};
use embassy_time::Instant;
use esp_hal::gpio::{Input, Output};
use esp_hal::peripherals::SPI2;
use esp_hal::spi::master::SpiDmaBus;
use esp_hal::spi::FullDuplexMode;
use esp_hal::Async;

pub(crate) type Epd = hal_ext::spi_dma::EpdBus<
    SpiDmaBus<'static, SPI2, FullDuplexMode, Async>,
    Output<'static>,
    Output<'static>,
    Input<'static>,
    Output<'static>,
>;

pub(crate) async fn init_panel(epd: &mut Epd) {
    for op in INIT_SEQUENCE {
        match *op {
            SpiOp::Reset => epd.reset().await,
            SpiOp::WaitBusy => epd.wait_ready().await,
            SpiOp::Command { cmd, data } => {
                epd.command(cmd, data).await.unwrap();
            }
        }
    }
}

pub(crate) async fn flush(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &Framebuffer,
    tx_band: &mut [u8; BAND_BYTES],
    screen_on: bool,
    mode: RefreshMode,
    red_holds_prev: bool,
) -> Result<
    (),
    <SpiDmaBus<'static, SPI2, FullDuplexMode, Async> as embedded_hal_async::spi::ErrorType>::Error,
> {
    let bw_start = Instant::now();
    write_ram(epd, CMD_WRITE_RAM_BW, fb, tx_band).await?;
    esp_println::println!(
        "display: write BW RAM {:?} {} ms",
        mode,
        bw_start.elapsed().as_millis()
    );
    if mode == RefreshMode::Fast {
        if red_holds_prev {
            // The previous frame was prestaged into RED RAM right after the
            // last refresh settled, so this page turn streams only BW.
            esp_println::println!("display: RED RAM already holds previous");
        } else {
            let red_start = Instant::now();
            write_ram(epd, CMD_WRITE_RAM_RED, prev_fb, tx_band).await?;
            esp_println::println!(
                "display: write RED RAM previous {} ms",
                red_start.elapsed().as_millis()
            );
        }
    } else {
        let red_start = Instant::now();
        write_ram(epd, CMD_WRITE_RAM_RED, fb, tx_band).await?;
        esp_println::println!(
            "display: write RED RAM current {} ms",
            red_start.elapsed().as_millis()
        );
    }

    esp_println::println!("display: refresh activate");
    epd.command(CMD_DISPLAY_UPDATE_CTRL1, &update_control_1(mode))
        .await?;
    epd.command(
        CMD_DISPLAY_UPDATE_CTRL2,
        &[update_control_2(mode, screen_on, false)],
    )
    .await?;
    epd.command(CMD_MASTER_ACTIVATION, &[]).await?;
    let start = Instant::now();
    epd.wait_ready().await;
    let elapsed = start.elapsed();
    esp_println::println!("display: refresh busy {} ms", elapsed.as_millis());
    Ok(())
}

/// Stage `fb` (the frame just shown) into RED RAM while the panel is idle,
/// so the next fast refresh can skip its previous-frame write entirely.
/// Runs off the page-turn critical path, right after a refresh settles.
pub(crate) async fn prestage_red(
    epd: &mut Epd,
    fb: &Framebuffer,
    tx_band: &mut [u8; BAND_BYTES],
) -> Result<
    (),
    <SpiDmaBus<'static, SPI2, FullDuplexMode, Async> as embedded_hal_async::spi::ErrorType>::Error,
> {
    write_ram(epd, CMD_WRITE_RAM_RED, fb, tx_band).await
}

pub(crate) async fn sleep_panel(
    epd: &mut Epd,
) -> Result<
    (),
    <SpiDmaBus<'static, SPI2, FullDuplexMode, Async> as embedded_hal_async::spi::ErrorType>::Error,
> {
    esp_println::println!("display: sleep start");
    epd.command(
        CMD_DISPLAY_UPDATE_CTRL2,
        &[update_control_2(RefreshMode::PowerDown, true, false)],
    )
    .await?;
    epd.command(CMD_MASTER_ACTIVATION, &[]).await?;
    epd.wait_ready().await;
    esp_println::println!("display: sleep deep");
    epd.command(CMD_DEEP_SLEEP, &[0x01]).await
}

async fn write_ram(
    epd: &mut Epd,
    ram_command: u8,
    fb: &Framebuffer,
    tx_band: &mut [u8; BAND_BYTES],
) -> Result<
    (),
    <SpiDmaBus<'static, SPI2, FullDuplexMode, Async> as embedded_hal_async::spi::ErrorType>::Error,
> {
    let rect = Rect::FULL;
    epd.command(CMD_SET_RAM_X_RANGE, &ram_x_range(rect)).await?;
    epd.command(CMD_SET_RAM_Y_RANGE, &ram_y_range(rect)).await?;
    epd.command(CMD_SET_RAM_X_COUNTER, &ram_x_counter(rect))
        .await?;
    epd.command(CMD_SET_RAM_Y_COUNTER, &ram_y_counter(rect))
        .await?;

    epd.begin_ram_write(ram_command).await?;
    let mut y = 0;
    let mut result = Ok(());
    while y < HEIGHT {
        let len = fill_transformed_band(fb, y, tx_band);
        // One DMA transfer per band: SpiDmaBus chunks internally against
        // its 8000-byte buffer, which dma_buffers!(8000) sized to match.
        if let Err(err) = epd.ram_chunk(&tx_band[..len]).await {
            result = Err(err);
            break;
        }
        y += BAND_ROWS;
    }
    epd.end_ram_write();
    result
}
