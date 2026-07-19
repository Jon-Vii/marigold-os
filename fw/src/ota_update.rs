//! Boot-time firmware self-update from the SD card.
//!
//! If `/FWUPDATE.BIN` is present at boot it is validated, written into the
//! *inactive* OTA slot, deleted (so the next boot cannot re-apply it), selected
//! by flipping `otadata`, and the device resets into the new firmware. This
//! is the recovery/update path that keeps flashing onto a locked unit from
//! being a one-way trip — the same scheme as the FreeInk SDK's `RecoveryBoot`
//! and CrossPoint's `FirmwareFlasher`/`OtaBootSwitch`, ported to Rust.
//!
//! Only the inactive slot and the inactive `otadata` sector are written, so a
//! failure here never touches the running firmware: the bootloader keeps
//! selecting the current slot until a complete, valid image flips `otadata`.
//! The image format, seq CRC, and slot-switch math live in [`proto::ota`] and
//! are host-tested; this module is the flash I/O around them.
//!
//! Untested on hardware as of this writing. Validate on the unlocked unit first
//! (espflash's bootloader is ESP-IDF and honours `otadata` too), then a locked
//! one. Flash writes freeze other tasks via a critical section; run this at
//! boot while the radio is idle.

use embedded_sdmmc::{BlockDevice, File, Mode, TimeSource};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;
use proto::ota::{self, ImageError, SelectEntry, SELECT_ENTRY_LEN};

use crate::sd_session::SdRoot;

/// One-shot trigger file at the card root. 8.3-safe so it opens without long
/// filename support, and distinct from the `update.bin` a user may keep on the
/// card as a permanent recovery image.
///
/// Device-specific so a card is safe to move between an X4 and an X3: each
/// build only picks up an image named for its own panel, so an X4 image
/// (`FWUPDATE.BIN`) is invisible to an X3 and vice versa. Flashing the wrong
/// build wouldn't brick (same SoC and partition table) but would drive the
/// wrong panel and battery gauge — a black screen, not a recoverable state.
#[cfg(not(feature = "device-x3"))]
const TRIGGER_FILE: &str = "FWUPDATE.BIN";
#[cfg(feature = "device-x3")]
const TRIGGER_FILE: &str = "FWUPDX3.BIN";

/// Legacy boot triggers are one-shot control files, never picker sources.
/// Reject both aliases on both boards so a moved SD card cannot expose the
/// other board's trigger as an ordinary selectable image.
pub(crate) fn is_trigger_alias(name: &str) -> bool {
    ota::is_legacy_trigger_alias(name)
}

const UPDATE_STATE_DIR: &str = "XTEINK";
const SELECTED_UPDATE_FILE: &str = "FWPEND.BIN";
const SELECTED_UPDATE_MAGIC: [u8; 4] = *b"MGUP";
const SELECTED_UPDATE_RECORD_LEN: usize = 22;

// Absolute flash offsets — must match `partitions.csv`.
const OTADATA_OFFSET: u32 = 0x0000_e000;
const OTADATA_SECTOR_STRIDE: u32 = 0x0000_1000; // one 4 KiB sector per entry
const OTA_SLOT_OFFSET: [u32; 2] = [0x0001_0000, 0x0065_0000];
const OTA_SLOT_SIZE: u32 = 0x0064_0000;
const OTA_COUNT: u32 = 2;

const SECTOR: usize = 4096;

// Variants (and their payloads) exist to be logged over serial on the failure
// path; dead-code analysis ignores the derived Debug use, hence the allow.
#[allow(dead_code)]
#[derive(Debug)]
pub enum UpdateError {
    /// No trigger file present — the normal case, not really an error.
    NoTrigger,
    ReadFile,
    Invalid(ImageError),
    Flash,
    /// The one-shot trigger/selection could not be consumed before commit.
    Consume,
}

/// Adapts an open SD file to [`ota::ImageSource`] for the validation pass.
struct SdFile<'f, D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>(
    &'f File<'f, D, T, MD, MF, MV>,
);

impl<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>
    ota::ImageSource for SdFile<'_, D, T, MD, MF, MV>
{
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()> {
        read_file_exact(self.0, buf)
    }
}

fn read_file_exact<
    D: BlockDevice,
    T: TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    file: &File<'_, D, T, MD, MF, MV>,
    buf: &mut [u8],
) -> Result<(), ()> {
    let mut done = 0;
    while done < buf.len() {
        match file.read(&mut buf[done..]) {
            Ok(0) => return Err(()),
            Ok(n) => done += n,
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

/// Check for a pending SD update and apply it. Returns `true` if an update was
/// flashed and the caller should now `software_reset()` into it. On any failure
/// the trigger file is removed so a corrupt image can't wedge every boot, and
/// the running firmware is left untouched.
pub fn apply_pending_update(root: &SdRoot) -> bool {
    let selected = read_selected_update(root);
    let source_name = selected.as_deref().unwrap_or(TRIGGER_FILE);
    let picker_selected = selected.is_some();
    match try_apply(root, source_name, picker_selected) {
        Ok(()) => {
            esp_println::println!("ota: update applied; resetting");
            true
        }
        Err(UpdateError::NoTrigger) => {
            if picker_selected {
                let _ = consume_pending_source(root, true);
            }
            false
        }
        Err(e) => {
            esp_println::println!("ota: update failed: {:?}", e);
            // A bad selection must not re-run forever. Picker-selected source
            // images stay on the card so the user can keep a firmware shelf;
            // the legacy one-shot trigger retains its historical deletion.
            if !consume_pending_source(root, picker_selected) {
                esp_println::println!("ota: pending source cleanup failed");
            }
            false
        }
    }
}

fn try_apply(root: &SdRoot, source_name: &str, picker_selected: bool) -> Result<(), UpdateError> {
    let file = root
        .open_file_in_dir(source_name, Mode::ReadOnly)
        .map_err(|_| UpdateError::NoTrigger)?;
    let len = file.length() as usize;
    esp_println::println!("ota: {} selected, {} bytes", source_name, len);

    // Pass 1: prove the whole image before touching flash. The firmware shelf
    // intentionally accepts app images from other reader projects; the user
    // chooses the target while the validator enforces integrity and fit.
    ota::validate_image(&mut SdFile(&file), len, Some(OTA_SLOT_SIZE as usize))
        .map_err(UpdateError::Invalid)?;
    file.seek_from_start(0).map_err(|_| UpdateError::ReadFile)?;

    let mut flash = flash_storage();

    // Destination is the slot we are *not* running from.
    let (s0, s1) = read_otadata(&mut flash)?;
    let active = ota::active_app_slot(&s0, &s1, OTA_COUNT).unwrap_or(0);
    let dest = (active + 1) % OTA_COUNT;
    esp_println::println!("ota: active slot {}, writing slot {}", active, dest);

    // Pass 2: erase + stream the image into the inactive slot.
    write_image(&mut flash, OTA_SLOT_OFFSET[dest as usize], &file, len)?;
    drop(file);

    // Consume the one-shot control record before committing the OTA switch.
    // If this fails the freshly-written inactive image is harmless: otadata
    // still selects the running firmware, which can boot normally and retry.
    if !consume_pending_source(root, picker_selected) {
        esp_println::println!("ota: refusing slot switch; pending source remains live");
        return Err(UpdateError::Consume);
    }

    // Point otadata at the freshly written slot (re-read: the write above did
    // not touch otadata, but re-reading keeps the switch self-consistent). The
    // one-shot source is already consumed, so a reset after this write cannot
    // schedule the same image again.
    let (s0, s1) = read_otadata(&mut flash)?;
    let switch = ota::plan_switch(&s0, &s1, dest, OTA_COUNT);
    write_select_entry(&mut flash, switch.target_sector, &switch.entry)?;
    esp_println::println!(
        "ota: otadata sector {} -> seq {}",
        switch.target_sector,
        switch.entry.ota_seq
    );

    Ok(())
}

/// Persist the picker selection as an 8.3 root alias. The source image itself
/// is not renamed or copied, so users can keep several firmware images on one
/// card and switch among them without rewriting multi-megabyte files.
pub(crate) fn stage_selected_update(root: &SdRoot, open_name: &str) -> bool {
    if is_trigger_alias(open_name)
        || open_name.is_empty()
        || open_name.len() > 12
        || !open_name.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return false;
    }
    let xteink = match root.open_dir(UPDATE_STATE_DIR) {
        Ok(dir) => dir,
        Err(_) => {
            if root.make_dir_in_dir(UPDATE_STATE_DIR).is_err() {
                return false;
            }
            let Ok(dir) = root.open_dir(UPDATE_STATE_DIR) else {
                return false;
            };
            dir
        }
    };
    let mut record = [0u8; SELECTED_UPDATE_RECORD_LEN];
    record[..4].copy_from_slice(&SELECTED_UPDATE_MAGIC);
    record[4] = 1;
    record[5] = open_name.len() as u8;
    record[6..6 + open_name.len()].copy_from_slice(open_name.as_bytes());
    let checksum = selected_update_checksum(&record[..18]);
    record[18..22].copy_from_slice(&checksum.to_le_bytes());
    let Ok(file) = xteink.open_file_in_dir(SELECTED_UPDATE_FILE, Mode::ReadWriteCreateOrTruncate)
    else {
        return false;
    };
    file.write(&record).is_ok()
}

fn read_selected_update(root: &SdRoot) -> Option<heapless::String<12>> {
    let xteink = root.open_dir(UPDATE_STATE_DIR).ok()?;
    let file = xteink
        .open_file_in_dir(SELECTED_UPDATE_FILE, Mode::ReadOnly)
        .ok()?;
    let mut record = [0u8; SELECTED_UPDATE_RECORD_LEN];
    read_file_exact(&file, &mut record).ok()?;
    if record[..4] != SELECTED_UPDATE_MAGIC || record[4] != 1 {
        return None;
    }
    let len = record[5] as usize;
    if len == 0 || len > 12 {
        return None;
    }
    let stored = u32::from_le_bytes([record[18], record[19], record[20], record[21]]);
    if selected_update_checksum(&record[..18]) != stored {
        return None;
    }
    let name = core::str::from_utf8(&record[6..6 + len]).ok()?;
    let mut out = heapless::String::new();
    out.push_str(name).ok()?;
    Some(out)
}

fn clear_selected_update(root: &SdRoot) -> bool {
    let Ok(xteink) = root.open_dir(UPDATE_STATE_DIR) else {
        return false;
    };
    crate::sd_session::remove_file_reclaiming_clusters(&xteink, SELECTED_UPDATE_FILE)
}

fn clear_trigger_update(root: &SdRoot) -> bool {
    crate::sd_session::remove_file_reclaiming_clusters(root, TRIGGER_FILE)
}

fn consume_pending_source(root: &SdRoot, picker_selected: bool) -> bool {
    if picker_selected {
        clear_selected_update(root)
    } else {
        clear_trigger_update(root)
    }
}

fn selected_update_checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

/// On-device validation of the flash + otadata path when no SD card reader is
/// available to place `FWUPDATE.BIN`. On the first boot (running from slot 0)
/// it copies the running image into the inactive slot and switches otadata to
/// it, so the next boot runs from the other slot — exercising esp-storage
/// erase/write, the seq CRC, the otadata switch, and the bootloader honouring
/// it, all without an SD file. One-shot: once running from the far slot it
/// no-ops. Compiled only under the `ota-selftest` feature.
#[cfg(feature = "ota-selftest")]
pub fn run_selftest() -> bool {
    // 3 MiB comfortably covers the ~2.5 MiB app image; the copy is self-
    // delimiting (the bootloader reads the header/segments and ignores the
    // trailing bytes), so an over-copy is harmless.
    const COPY_LEN: u32 = 0x0030_0000;

    let mut flash = flash_storage();
    let (s0, s1) = match read_otadata(&mut flash) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let active = ota::active_app_slot(&s0, &s1, OTA_COUNT).unwrap_or(0);
    if active != 0 {
        esp_println::println!("selftest: already running from slot {}; done", active);
        return false;
    }

    let src = OTA_SLOT_OFFSET[0];
    let dst = OTA_SLOT_OFFSET[1];
    esp_println::println!("selftest: copy slot 0 -> slot 1 ({} bytes)", COPY_LEN);
    if flash.erase(dst, dst + COPY_LEN).is_err() {
        esp_println::println!("selftest: erase failed");
        return false;
    }
    let mut buf = [0u8; SECTOR];
    let mut off = 0u32;
    while off < COPY_LEN {
        if flash.read(src + off, &mut buf).is_err() {
            esp_println::println!("selftest: read failed @{:#x}", off);
            return false;
        }
        if flash.write(dst + off, &buf).is_err() {
            esp_println::println!("selftest: write failed @{:#x}", off);
            return false;
        }
        off += SECTOR as u32;
    }

    let (s0, s1) = match read_otadata(&mut flash) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let switch = ota::plan_switch(&s0, &s1, 1, OTA_COUNT);
    if write_select_entry(&mut flash, switch.target_sector, &switch.entry).is_err() {
        esp_println::println!("selftest: otadata write failed");
        return false;
    }
    esp_println::println!(
        "selftest: otadata sector {} -> seq {} (slot 1)",
        switch.target_sector,
        switch.entry.ota_seq
    );
    true
}

/// True when the recovery combo — `Back` (front ladder) + `Up` (side ladder) —
/// is held, given the two calibrated ADC readings in millivolts. The bands
/// mirror `tasks::input`'s NAV/PAGE tables; they're on separate pins, so the
/// combo is unambiguous.
pub fn recovery_combo_held(nav_mv: u16, page_mv: u16) -> bool {
    (2400..=2700).contains(&nav_mv) && (1500..=1800).contains(&page_mv)
}

/// Boot-time escape hatch (the FreeInk SDK `RecoveryBoot` pattern): when the
/// combo is held at reset and we are running from a slot other than 0, repoint
/// `otadata` at slot 0 and return `true` so the caller resets into it. Slot 0
/// is the recovery anchor — the firmware first installed there — so this backs
/// out of an update in the far slot that boots but misbehaves.
///
/// No-op (returns `false`) when already effectively on slot 0, or when slot 0
/// doesn't hold a valid image (so the combo can't switch into an empty slot).
/// The stock bootloader can't read buttons, so this is the earliest point a
/// held combo can be honoured — it must run before the main app takes over.
pub fn recover_to_slot0() -> bool {
    let mut flash = flash_storage();
    let (s0, s1) = match read_otadata(&mut flash) {
        Ok(v) => v,
        Err(_) => return false,
    };
    // Only act when running from a non-zero slot. `None` (erased otadata) means
    // the bootloader already defaults to slot 0, so there's nothing to undo.
    if ota::active_app_slot(&s0, &s1, OTA_COUNT) != Some(1) {
        return false;
    }
    // Refuse to switch into a slot 0 that isn't a bootable image.
    let mut head = [0u8; 4];
    if flash.read(OTA_SLOT_OFFSET[0], &mut head).is_err() || head[0] != ota::IMAGE_MAGIC {
        esp_println::println!("recovery: slot 0 has no valid image; ignoring combo");
        return false;
    }
    let switch = ota::plan_switch(&s0, &s1, 0, OTA_COUNT);
    if write_select_entry(&mut flash, switch.target_sector, &switch.entry).is_err() {
        esp_println::println!("recovery: otadata write failed");
        return false;
    }
    esp_println::println!(
        "recovery: combo held; otadata sector {} -> seq {} (slot 0)",
        switch.target_sector,
        switch.entry.ota_seq
    );
    true
}

/// Acknowledge a freshly OTA-booted app before the next deep-sleep reset can
/// make rollback-enabled bootloaders return to the previous firmware.
pub fn mark_running_slot_valid() {
    let mut flash = flash_storage();
    let (s0, s1) = match read_otadata(&mut flash) {
        Ok(v) => v,
        Err(_) => return,
    };
    let Some(valid) = ota::plan_mark_app_valid(&s0, &s1) else {
        return;
    };
    if write_select_entry(&mut flash, valid.target_sector, &valid.entry).is_err() {
        esp_println::println!("ota: mark-valid failed");
        return;
    }
    esp_println::println!(
        "ota: marked slot {} valid (seq {})",
        (valid.entry.ota_seq - 1) % OTA_COUNT,
        valid.entry.ota_seq
    );
}

#[allow(unsafe_code)]
fn flash_storage() -> FlashStorage<'static> {
    // SAFETY: OTA update/recovery runs at boot before application tasks use
    // flash directly. This preserves the old `FlashStorage::new()` singleton
    // behavior under esp-storage's explicit peripheral ownership API.
    FlashStorage::new(unsafe { esp_hal::peripherals::FLASH::steal() })
}

fn read_otadata(
    flash: &mut FlashStorage,
) -> Result<([u8; SELECT_ENTRY_LEN], [u8; SELECT_ENTRY_LEN]), UpdateError> {
    let mut s0 = [0u8; SELECT_ENTRY_LEN];
    let mut s1 = [0u8; SELECT_ENTRY_LEN];
    flash
        .read(OTADATA_OFFSET, &mut s0)
        .map_err(|_| UpdateError::Flash)?;
    flash
        .read(OTADATA_OFFSET + OTADATA_SECTOR_STRIDE, &mut s1)
        .map_err(|_| UpdateError::Flash)?;
    Ok((s0, s1))
}

fn write_image<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>(
    flash: &mut FlashStorage,
    dest_offset: u32,
    file: &File<'_, D, T, MD, MF, MV>,
    len: usize,
) -> Result<(), UpdateError> {
    // Erase only the sectors we will write, rounded up to the 4 KiB boundary.
    let erase_len = ((len as u32) + SECTOR as u32 - 1) & !(SECTOR as u32 - 1);
    flash
        .erase(dest_offset, dest_offset + erase_len)
        .map_err(|_| UpdateError::Flash)?;

    let mut buf = [0u8; SECTOR];
    let mut written: u32 = 0;
    while (written as usize) < len {
        let want = core::cmp::min(SECTOR, len - written as usize);
        read_file_exact(file, &mut buf[..want]).map_err(|_| UpdateError::ReadFile)?;
        // NorFlash writes must be a multiple of WRITE_SIZE (4); pad the final
        // partial word with 0xFF (the erased state), leaving flash unchanged
        // past the real image bytes.
        let wlen = (want + 3) & !3;
        for b in &mut buf[want..wlen] {
            *b = 0xFF;
        }
        flash
            .write(dest_offset + written, &buf[..wlen])
            .map_err(|_| UpdateError::Flash)?;
        written += want as u32;
    }
    Ok(())
}

fn write_select_entry(
    flash: &mut FlashStorage,
    sector: usize,
    entry: &SelectEntry,
) -> Result<(), UpdateError> {
    let offset = OTADATA_OFFSET + sector as u32 * OTADATA_SECTOR_STRIDE;
    flash
        .erase(offset, offset + OTADATA_SECTOR_STRIDE)
        .map_err(|_| UpdateError::Flash)?;
    flash
        .write(offset, &entry.to_bytes())
        .map_err(|_| UpdateError::Flash)?;
    Ok(())
}
