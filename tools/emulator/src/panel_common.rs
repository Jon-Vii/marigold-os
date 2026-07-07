//! Small pieces shared by the two mutually-exclusive panel models
//! (`panel.rs` for the X4/SSD1677, `panel_uc8253.rs` for the X3/UC8253):
//! command-length validation and the transcript string vocabulary they
//! both push into `history`. Only one panel model compiles into a given
//! build, but keeping their shared format strings and validation in one
//! place avoids the two copies drifting apart from each other.

/// Reject a command whose data doesn't match the controller's fixed
/// argument length for that command.
pub fn expect_len(cmd: u8, data: &[u8], expected: usize) -> Result<(), String> {
    if data.len() == expected {
        Ok(())
    } else {
        Err(format!(
            "command 0x{cmd:02X} expected {expected} data bytes, got {}",
            data.len()
        ))
    }
}

/// Transcript entry for a plain command write.
pub fn cmd_history_entry(cmd: u8, data: &[u8]) -> String {
    format!("cmd 0x{cmd:02X} {:02X?}", data)
}

/// Transcript entry for a completed RAM-plane write.
pub fn ram_history_entry(cmd: u8, width: usize, height: usize) -> String {
    format!("ram 0x{cmd:02X} {width}x{height}")
}

pub const HISTORY_RESET: &str = "reset";
pub const HISTORY_DEEP_SLEEP: &str = "deep_sleep";
