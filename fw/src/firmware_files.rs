//! Root-level firmware image catalog for the Settings picker.

use crate::display_flush::Epd;
use crate::reader_store::{ReaderStore, MAX_FIRMWARE_FILES};
use crate::sd_session;
use core::fmt::Write;
use embedded_sdmmc::LfnBuffer;
use esp_hal::gpio::Output;
use heapless::String;

/// Scan the card root for `.BIN` files. The readable long name is shown in the
/// UI while the 8.3 alias is retained for the no-allocation boot updater.
pub(crate) fn scan(epd: &mut Epd, sd_cs: &mut Output<'static>, store: &mut ReaderStore) -> u16 {
    store.clear_firmware_files();
    let _ = sd_session::with_root(epd, sd_cs, |root| {
        let mut lfn_storage = [0u8; 192];
        let mut lfn_buffer = LfnBuffer::new(&mut lfn_storage);
        let _ = root.iterate_dir_lfn(&mut lfn_buffer, |entry, long_name| {
            if store.firmware_files().len() >= MAX_FIRMWARE_FILES
                || entry.attributes.is_directory()
                || entry.attributes.is_volume()
            {
                return;
            }

            let mut open_name = String::<16>::new();
            let _ = write!(open_name, "{}", entry.name);
            let display_name = long_name.unwrap_or(open_name.as_str());
            if !is_bin_name(display_name)
                || crate::ota_update::is_trigger_alias(display_name)
                || crate::ota_update::is_trigger_alias(open_name.as_str())
            {
                return;
            }
            store.push_firmware_file(display_name, open_name.as_str(), entry.size);
        });
    });
    store.firmware_files().len().min(u16::MAX as usize) as u16
}

pub(crate) fn stage(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    store: &ReaderStore,
    index: usize,
) -> bool {
    let Some(entry) = store.firmware_file(index) else {
        return false;
    };
    sd_session::with_root(epd, sd_cs, |root| {
        crate::ota_update::stage_selected_update(root, entry.open_name.as_str())
    })
    .ok()
    .unwrap_or(false)
}

fn is_bin_name(name: &str) -> bool {
    name.rsplit_once('.')
        .map(|(_, extension)| extension.eq_ignore_ascii_case("bin"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_filter_is_case_insensitive_and_extension_anchored() {
        assert!(is_bin_name("marigold-x4.bin"));
        assert!(is_bin_name("CROSSPOINT.BIN"));
        assert!(!is_bin_name("firmware.bin.txt"));
        assert!(!is_bin_name("bin"));
    }

    #[test]
    fn legacy_update_triggers_are_never_picker_images() {
        assert!(crate::ota_update::is_trigger_alias("FWUPDATE.BIN"));
        assert!(crate::ota_update::is_trigger_alias("fwupdx3.bin"));
        assert!(!crate::ota_update::is_trigger_alias("marigold-x4.bin"));
    }
}
