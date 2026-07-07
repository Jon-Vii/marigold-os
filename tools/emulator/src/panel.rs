use crate::panel_common::{
    cmd_history_entry, expect_len, ram_history_entry, HISTORY_DEEP_SLEEP, HISTORY_RESET,
};
use display::epd::{
    fill_transformed_band, ram_x_counter, ram_x_range, ram_y_counter, ram_y_range, RefreshMode,
    SpiOp, CMD_DEEP_SLEEP, CMD_DISPLAY_UPDATE_CTRL1, CMD_DISPLAY_UPDATE_CTRL2,
    CMD_MASTER_ACTIVATION, CMD_SET_RAM_X_COUNTER, CMD_SET_RAM_X_RANGE, CMD_SET_RAM_Y_COUNTER,
    CMD_SET_RAM_Y_RANGE, CMD_WRITE_RAM_BW, CMD_WRITE_RAM_RED, INIT_SEQUENCE,
};
use display::fb::Framebuffer;
use display::{BAND_BYTES, FB_BYTES, HEIGHT, ROW_BYTES, WIDTH};

#[derive(Debug)]
pub struct PanelModel {
    bw_ram: [u8; FB_BYTES],
    red_ram: [u8; FB_BYTES],
    deep_sleep: bool,
    initialized: bool,
    x_range: [u8; 4],
    y_range: [u8; 4],
    x_counter: [u8; 2],
    y_counter: [u8; 2],
    update_ctrl1: [u8; 2],
    update_ctrl2: u8,
    last_refresh: Option<RefreshMode>,
    history: Vec<String>,
}

impl PanelModel {
    pub fn new() -> Self {
        Self {
            bw_ram: [0xFF; FB_BYTES],
            red_ram: [0xFF; FB_BYTES],
            deep_sleep: false,
            initialized: false,
            x_range: [0; 4],
            y_range: [0; 4],
            x_counter: [0; 2],
            y_counter: [0; 2],
            update_ctrl1: [0; 2],
            update_ctrl2: 0,
            last_refresh: None,
            history: Vec::new(),
        }
    }

    pub fn init_sequence(&mut self) -> Result<(), String> {
        for op in INIT_SEQUENCE {
            match *op {
                SpiOp::Reset => self.reset(),
                SpiOp::WaitBusy => self.wait_busy(),
                SpiOp::Command { cmd, data } => self.command(cmd, data)?,
            }
        }
        self.initialized = true;
        Ok(())
    }

    pub fn command(&mut self, cmd: u8, data: &[u8]) -> Result<(), String> {
        if self.deep_sleep && cmd != display::epd::CMD_SW_RESET {
            return Err(format!("command 0x{cmd:02X} while panel is asleep"));
        }
        match cmd {
            CMD_SET_RAM_X_RANGE => self.copy_exact_4(cmd, data, true)?,
            CMD_SET_RAM_Y_RANGE => self.copy_exact_4(cmd, data, false)?,
            CMD_SET_RAM_X_COUNTER => {
                expect_len(cmd, data, 2)?;
                self.x_counter.copy_from_slice(data);
            }
            CMD_SET_RAM_Y_COUNTER => {
                expect_len(cmd, data, 2)?;
                self.y_counter.copy_from_slice(data);
            }
            CMD_DISPLAY_UPDATE_CTRL1 => {
                expect_len(cmd, data, 2)?;
                self.update_ctrl1.copy_from_slice(data);
            }
            CMD_DISPLAY_UPDATE_CTRL2 => {
                expect_len(cmd, data, 1)?;
                self.update_ctrl2 = data[0];
            }
            CMD_MASTER_ACTIVATION => expect_len(cmd, data, 0)?,
            CMD_DEEP_SLEEP => {
                expect_len(cmd, data, 1)?;
                self.deep_sleep = true;
            }
            _ => {}
        }
        self.history.push(cmd_history_entry(cmd, data));
        Ok(())
    }

    pub fn write_framebuffer_bw(&mut self, fb: &Framebuffer) -> Result<(), String> {
        self.write_framebuffer(CMD_WRITE_RAM_BW, fb)
    }

    #[allow(dead_code)]
    pub fn write_framebuffer_red(&mut self, fb: &Framebuffer) -> Result<(), String> {
        self.write_framebuffer(CMD_WRITE_RAM_RED, fb)
    }

    pub fn flush(
        &mut self,
        current: &Framebuffer,
        previous: &Framebuffer,
        mode: RefreshMode,
        previous_staged: bool,
    ) -> Result<RefreshMode, String> {
        self.write_framebuffer_bw(current)?;
        if mode == RefreshMode::Fast {
            if !previous_staged {
                self.write_framebuffer_red(previous)?;
            }
        } else {
            self.write_framebuffer_red(current)?;
        }
        self.refresh(mode)?;
        Ok(mode)
    }

    pub fn prestage_previous(&mut self, fb: &Framebuffer) -> Result<(), String> {
        self.write_framebuffer_red(fb)
    }

    pub fn refresh(&mut self, mode: RefreshMode) -> Result<(), String> {
        if mode == RefreshMode::FastClean {
            self.command(
                display::epd::CMD_WRITE_TEMPERATURE,
                &display::epd::FAST_CLEAN_TEMPERATURE,
            )?;
        }
        self.command(CMD_DISPLAY_UPDATE_CTRL1, &display::epd::update_control_1(mode))?;
        self.command(
            CMD_DISPLAY_UPDATE_CTRL2,
            &[display::epd::update_control_2(mode, true, false)],
        )?;
        self.command(CMD_MASTER_ACTIVATION, &[])?;
        if mode == RefreshMode::FastClean {
            // Mirror the firmware's post-clean sensor temperature re-load.
            self.command(
                CMD_DISPLAY_UPDATE_CTRL2,
                &[display::epd::UPDATE_SEQUENCE_LOAD_TEMP],
            )?;
            self.command(CMD_MASTER_ACTIVATION, &[])?;
        }
        self.last_refresh = Some(mode);
        self.history.push(format!("refresh {mode:?}"));
        Ok(())
    }

    pub fn deep_sleep(&mut self) -> Result<(), String> {
        self.command(
            CMD_DISPLAY_UPDATE_CTRL2,
            &[display::epd::update_control_2(RefreshMode::PowerDown, true, false)],
        )?;
        self.command(CMD_MASTER_ACTIVATION, &[])?;
        self.command(CMD_DEEP_SLEEP, &[0x01])?;
        self.history.push(HISTORY_DEEP_SLEEP.into());
        Ok(())
    }

    pub fn last_refresh(&self) -> Option<RefreshMode> {
        self.last_refresh
    }

    pub fn is_deep_sleep(&self) -> bool {
        self.deep_sleep
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    fn write_framebuffer(&mut self, ram_command: u8, fb: &Framebuffer) -> Result<(), String> {
        if self.deep_sleep {
            return Err("RAM write while panel is asleep".into());
        }
        if !self.initialized {
            return Err("RAM write before init sequence".into());
        }
        self.command(CMD_SET_RAM_X_RANGE, &ram_x_range(display::Rect::FULL))?;
        self.command(CMD_SET_RAM_Y_RANGE, &ram_y_range(display::Rect::FULL))?;
        self.command(CMD_SET_RAM_X_COUNTER, &ram_x_counter(display::Rect::FULL))?;
        self.command(CMD_SET_RAM_Y_COUNTER, &ram_y_counter(display::Rect::FULL))?;
        let target = match ram_command {
            CMD_WRITE_RAM_BW => &mut self.bw_ram,
            CMD_WRITE_RAM_RED => &mut self.red_ram,
            _ => return Err(format!("unsupported RAM command 0x{ram_command:02X}")),
        };
        let mut band = [0u8; BAND_BYTES];
        let mut y = 0;
        while y < HEIGHT {
            let len = fill_transformed_band(fb, y, &mut band);
            let start = y * ROW_BYTES;
            target[start..start + len].copy_from_slice(&band[..len]);
            y += display::BAND_ROWS;
        }
        self.history.push(ram_history_entry(ram_command, WIDTH, HEIGHT));
        Ok(())
    }

    fn reset(&mut self) {
        self.deep_sleep = false;
        self.initialized = false;
        self.history.push(HISTORY_RESET.into());
    }

    fn wait_busy(&mut self) {
        self.history.push("wait_busy".into());
    }

    fn copy_exact_4(&mut self, cmd: u8, data: &[u8], x: bool) -> Result<(), String> {
        expect_len(cmd, data, 4)?;
        if x {
            self.x_range.copy_from_slice(data);
        } else {
            self.y_range.copy_from_slice(data);
        }
        Ok(())
    }
}

impl Default for PanelModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_sequence_is_accepted() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        assert!(panel.initialized);
    }

    #[test]
    fn full_framebuffer_bw_write_is_accepted() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        panel.write_framebuffer_bw(&Framebuffer::new()).unwrap();
    }

    #[test]
    fn invalid_command_lengths_are_rejected() {
        let mut panel = PanelModel::new();
        let err = panel.command(CMD_SET_RAM_X_RANGE, &[0, 1]).unwrap_err();
        assert!(err.contains("expected 4"));
    }

    #[test]
    fn refresh_records_mode() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        panel.refresh(RefreshMode::Fast).unwrap();
        assert_eq!(panel.last_refresh(), Some(RefreshMode::Fast));
    }

    #[test]
    fn deep_sleep_blocks_ram_writes() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        panel.deep_sleep().unwrap();
        assert!(panel.write_framebuffer_bw(&Framebuffer::new()).is_err());
    }
}
