//! Boot-time firmware self-update from the SD card.
//!
//! If `/FWUPDATE.BIN` is present at boot it is validated, written into the
//! *inactive* OTA slot, selected by flipping `otadata`, deleted (so the next
//! boot doesn't re-apply it), and the device resets into the new firmware. This
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
const TRIGGER_FILE: &str = "FWUPDATE.BIN";

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

fn read_file_exact<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>(
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
    match try_apply(root) {
        Ok(()) => {
            esp_println::println!("ota: update applied; resetting");
            true
        }
        Err(UpdateError::NoTrigger) => false,
        Err(e) => {
            esp_println::println!("ota: update failed: {:?}", e);
            // A bad trigger file must not re-run forever.
            let _ = root.delete_file_in_dir(TRIGGER_FILE);
            false
        }
    }
}

fn try_apply(root: &SdRoot) -> Result<(), UpdateError> {
    let file = root
        .open_file_in_dir(TRIGGER_FILE, Mode::ReadOnly)
        .map_err(|_| UpdateError::NoTrigger)?;
    let len = file.length() as usize;
    esp_println::println!("ota: {} found, {} bytes", TRIGGER_FILE, len);

    // Pass 1: prove the whole image before touching flash.
    ota::validate_image(&mut SdFile(&file), len, Some(OTA_SLOT_SIZE as usize))
        .map_err(UpdateError::Invalid)?;
    file.seek_from_start(0).map_err(|_| UpdateError::ReadFile)?;

    let mut flash = FlashStorage::new();

    // Destination is the slot we are *not* running from.
    let (s0, s1) = read_otadata(&mut flash)?;
    let active = ota::active_app_slot(&s0, &s1, OTA_COUNT).unwrap_or(0);
    let dest = (active + 1) % OTA_COUNT;
    esp_println::println!("ota: active slot {}, writing slot {}", active, dest);

    // Pass 2: erase + stream the image into the inactive slot.
    write_image(&mut flash, OTA_SLOT_OFFSET[dest as usize], &file, len)?;

    // Point otadata at the freshly written slot (re-read: the write above did
    // not touch otadata, but re-reading keeps the switch self-consistent).
    let (s0, s1) = read_otadata(&mut flash)?;
    let switch = ota::plan_switch(&s0, &s1, dest, OTA_COUNT);
    write_select_entry(&mut flash, switch.target_sector, &switch.entry)?;
    esp_println::println!(
        "ota: otadata sector {} -> seq {}",
        switch.target_sector,
        switch.entry.ota_seq
    );

    // One-shot: drop the trigger so the next boot runs the new firmware once.
    let _ = root.delete_file_in_dir(TRIGGER_FILE);
    Ok(())
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

    let mut flash = FlashStorage::new();
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
