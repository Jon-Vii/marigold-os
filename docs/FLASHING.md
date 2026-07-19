# Flashing & release images

This firmware ships as a standard ESP32-C3 application image that boots under
the Xteink X4/X3 **stock second-stage bootloader**. That's what makes it
installable the same way the other community firmwares (CrossPoint, CrossInk)
are — including, in principle, on *locked* units.

## Unlocked vs. locked units

Some X4s and X3s — typically the ones bought from third-party sellers (AliExpress) —
ship with **USB flashing disabled in eFuse at the factory**. Units bought
directly from xteink.com are not locked.

To tell which you have: connect over USB (USB-C on X4, the 4-pin pogo cable on
X3) and try to flash (`cargo run` or the web flasher). If the device never
appears as a serial port even after trying another cable/port/browser, assume
it's locked.

The author's own X4 is unlocked, so the locked-device path still needs a real
locked-unit confirmation — see [Status](#status).

## The layout

`partitions.csv` mirrors the stock dual-OTA layout so our app lands where the
stock bootloader expects it:

| Partition | Type | Offset | Size |
|---|---|---|---|
| nvs | data/nvs | `0x9000` | 20 KB |
| otadata | data/ota | `0xe000` | 8 KB |
| app0 | app/ota_0 | `0x10000` | 6.5 MB |
| app1 | app/ota_1 | `0x650000` | 6.5 MB |
| spiffs | data/spiffs | `0xc90000` | 3.4 MB |
| coredump | data/coredump | `0xff0000` | 64 KB |

The app is ~2 MB, so it fits `ota_0` with room to spare. `cargo run` now flashes
against this table (see `.cargo/config.toml`).

### Why the stock bootloader accepts our image

The X4 bootloader gates images on an eFuse block-revision range read from the
app descriptor (`esp_app_desc_t`). We emit that descriptor in `fw/src/main.rs`
(`_ESP_APP_DESC`, magic `0xABCD5432`) at image offset `0x20` — exactly where the
bootloader reads it — with `min_efuse_blk_rev_full = 0` and
`max_efuse_blk_rev_full = 65535`, i.e. "accept any revision". This is the same
gate the other firmwares defeat with a build-time patch; we satisfy it directly
in the descriptor. You can verify placement:

```sh
xxd -s 0x20 -l 4 target/release-images/firmware.bin   # -> 3254 cdab (0xABCD5432 LE)
```

## Building the release images

Release builds require Rust from `rustup`, the firmware target, and `espflash`:

```sh
rustup target add riscv32imc-unknown-none-elf
cargo install espflash
```

```sh
tools/build-release.sh        # X4 (default)
tools/build-release.sh x3     # X3
```

Produces local images in `target/release-images/`:

- **`firmware.bin`** — app image for `ota_0`. Flash to `0x10000`. Updates the
  app in place and leaves the bootloader untouched. This is what the web
  flasher, `esptool write_flash 0x10000`, and in-app updater consume. The web
  installer explicitly forbids whole-chip erase for these app-only manifests;
  CI verifies that contract before Pages deployment.
- **`firmware-x3.bin`** — the same app image contract for X3 builds.
- **`update.bin`** — byte-identical to `firmware.bin`, under the filename the
  stock OEM SD-card updater looks for. The OEM updater writes it to the app
  slot at `0x10000`, so it is an **app image, not a full-flash image**.
- **`FWUPDATE.BIN`** — byte-identical to `firmware.bin`, under the filename
  MarigoldOS itself looks for on the card root at boot.
- **`FWUPDX3.BIN`** — the X3 SD-card trigger filename.
- **`full-flash*.bin`** — merged 16 MB images (bootloader + partition table +
  app) for local bench recovery on unlocked units only.

Tagged GitHub releases publish only the public app/SD assets:
`firmware-x4.bin`, `firmware-x3.bin`, `update.bin`, `FWUPDATE.BIN`, and
`FWUPDX3.BIN`. `firmware-x4.bin` is the release-time name for the default X4
`target/release-images/firmware.bin`; `FWUPDATE.BIN` is the same X4 app image
under Marigold's in-app updater trigger name.

> [!CAUTION]
> Never put `full-flash*.bin` on an SD card and never write it to `0x10000`. The
> OEM SD updater writes whatever it finds to the app slot; a full-flash image
> there lands a bootloader in the middle of the app partition and bricks the
> device. Writing to `0x0` is the fastest brick on any unit. The SD card and the
> app slot only ever take `update.bin`/`FWUPDX3.BIN` or the app-only
> `firmware-*.bin` images.

## Xteink X3

The X3 is the X4's sibling: same ESP32-C3, same 16 MB flash, same partition
table and bootloader path, on a smaller 792×528 UC8253 panel with a BQ27220
battery gauge instead of the X4's ADC divider. Support lives behind the
`device-x3` feature; the default build is unchanged for the X4. X3 support has
now been validated on hardware, so the web flasher and release pipeline publish
first-class X3 images alongside X4 images.

Build the X3 images:

```sh
tools/build-release.sh x3
```

Produces, in `target/release-images/`: **`firmware-x3.bin`** (flash to
`0x10000`), **`FWUPDX3.BIN`** (SD-card trigger — a *device-specific* name, so a
card can't cross-flash an X4 image onto an X3 or vice versa), and
**`full-flash-x3.bin`** (local whole-flash image for unlocked bench units only).
The app-only flash paths are the same as the X4 below, with the `-x3` image
names.

> [!NOTE]
> The X3 charges and flashes through a **4-pin magnetic pogo connector**, not
> USB-C. The 2-pin variant of that cable is charge-only and will not enumerate
> as a serial port. Serial is behind the same native USB-Serial-JTAG as the X4.

When testing, **capture the serial log** (`cargo run` monitors it, or
`espflash monitor`): panel init, the `X3` BUSY-wait completions, per-refresh
timings, and `input: bq27220` battery reads are what pin down which bring-up
value (BUSY timing → orientation → waveforms → battery) needs adjustment.

## Flashing an unlocked unit

```sh
# Everyday dev flash + serial monitor:
tools/cargo.sh run -p fw --release

# App-only, with esptool:
esptool.py --chip esp32c3 write_flash 0x10000 target/release-images/firmware.bin
esptool.py --chip esp32c3 write_flash 0x10000 target/release-images/firmware-x3.bin

# Whole flash from scratch, local unlocked bench units only:
esptool.py --chip esp32c3 write_flash 0x0 target/release-images/full-flash.bin
```

## Flashing a locked unit

> [!WARNING]
> On a locked unit, USB flashing is the recovery path of last resort and it's
> disabled. If you install a firmware that has **no over-the-air / SD update
> path of its own**, and USB re-locks, there is no way back. This firmware does
> not yet ship that recovery path (see [Status](#status)), so **do not install
> it on a locked unit you can't afford to brick.**

Two mechanisms exist, both pioneered by CrossPoint:

1. **Stock SD-card updater.** The OEM bootloader/app updates from an image on
   the SD card: copy **`update.bin`** to the card root, power on holding
   **Power + Up** while on USB power, and it writes the image to the app slot at
   `0x10000` (no bootloader replacement). Some builds also auto-flash a file
   named `force_update.bin` on boot with no button combo — handy as a recovery
   file to keep on the card. This path does **not** re-enable USB flashing. It
   is the standard install route for locked/AliExpress units.

2. **External unlocker tools** (CrossPoint's USB Unlocker / OTA Unlocker) that
   re-enable USB flashing or intercept the official OTA channel. These are
   separate desktop tools, out of scope for this repo; they officially support
   only CrossPoint/CrossInk.

## In-app update (the recovery net)

Once *any* build of this firmware is running, it can update itself from the card
with no computer — this is what keeps a locked unit from being a one-way trip:

### Settings picker

Place one or more app-image `.BIN` files at the card root, then open
**Settings → Firmware update**. The picker uses the same Previous/Next list
controls as Library. Confirm selects an image; a second confirmation schedules
it and restarts the device. The selected image remains on the card so one card
can act as a reusable firmware shelf.

The picker stores only the selected file's FAT 8.3 alias in
`/XTEINK/FWPEND.BIN`. On the next boot the existing updater reopens that exact
source, validates the complete ESP application image, writes the inactive OTA
slot, and switches `otadata`. A failed or completed attempt clears only the
pending-selection record.

Only app images intended for the current X3/X4 should be selected. Generic ESP
image validation proves integrity and partition fit, but cannot prove that a
third-party image drives the correct panel or retains an SD recovery path.

### Automatic trigger

1. Copy a new app image to the card root as **`FWUPDATE.BIN`** (the file
   `tools/build-release.sh` produces for X4; the `FWUPDATE.BIN` name
   is the one-shot trigger, kept distinct from a permanent `update.bin` you may
   also keep on the card). On the **X3** the trigger is **`FWUPDX3.BIN`**
   instead — each build only picks up an image named for its own panel, so a
   card is safe to carry between an X4 and an X3 without either grabbing the
   other's image (they share a SoC and partition table, but not a display
   controller or battery gauge, so a cross-flash is a black screen).
2. Reboot. At boot, before the reader starts, the firmware validates the image
   (`proto::ota::validate_image`), writes it into the **inactive** OTA slot,
   deletes `FWUPDATE.BIN` so it can run only once, flips `otadata` to select
   the new slot (`proto::ota::plan_switch`), and resets into the new firmware.
   On the first boot after any OTA-slot install (including CrossInk's
   Settings -> SD firmware update flow), Marigold marks the selected `otadata`
   entry valid before the reader starts, so rollback-enabled bootloaders do not
   return to the previous firmware on the next deep-sleep reset.

Only the inactive slot and inactive `otadata` sector are written, so a bad or
half-copied image never harms the running firmware — the bootloader keeps
booting the current slot until a complete, valid image flips `otadata`. This
works on an unlocked unit too (espflash's bootloader is ESP-IDF and honours
`otadata`), which is how to test it without a locked device.

If an update lands you on a firmware that boots but misbehaves, hold **Back + Up**
at reset: the recovery hatch repoints `otadata` back at slot 0 (the firmware
first installed there) and reboots into it. It can't help a firmware that won't
boot far enough to run the check — that would need a custom bootloader, which no
app-level firmware provides — so treat it as a strong safety net, not a
guarantee against every brick.

## Status

Implemented and verified on host tooling:

- [x] Stock-compatible dual-OTA partition table (`partitions.csv`).
- [x] App descriptor with the open eFuse range at offset `0x20` (bootloader-gate
      workaround), verified present in the built image.
- [x] Reproducible app/SD images (`firmware.bin`, `firmware-x3.bin`,
      `update.bin`, `FWUPDATE.BIN`, `FWUPDX3.BIN`) plus local-only `full-flash*.bin` bench
      images (`tools/build-release.sh`). The SD images are app images written to
      `0x10000`, matching the OEM updater.
- [x] `cargo run` flashes the stock-compatible layout.
- [x] **Image validator** (`proto::ota::validate_image`) — the integrity gate
      (magic / segment walk / XOR checksum / SHA-256 trailer) that must pass
      before any candidate `.bin` is written to the inactive slot. Streaming,
      no heap; host-tested against synthetic valid and corrupt images.
- [x] **otadata layer** (`proto::ota`: `seq_crc`, `SelectEntry`, `plan_switch`,
      `active_app_slot`) — the OTA-slot select-entry format, the seq CRC
      (verified against the esp-bootloader-esp-idf algorithm *and* a real
      on-device value: `seq_crc(1) == 0x4743989A`), and the slot-switch math.
      Host-tested.
- [x] **Boot-time SD updater** (`fw::ota_update`) — on boot, `/FWUPDATE.BIN` is
      validated, written to the inactive OTA slot with `esp-storage`, selected
      via `otadata`, deleted, and the device resets into it. Only the inactive
      slot/sector are touched.
- [x] **Settings firmware picker** — scans root-level `.BIN` app images into a
      bounded native list, requires an explicit second confirmation, persists
      the selected FAT alias, and reuses the boot-time validator/flasher while
      leaving reusable source images on the card.
- [x] **OTA rollback acknowledgement** (`fw::ota_update::mark_running_slot_valid`)
      — early boot rewrites an active `NEW`/`PENDING_VERIFY` select entry as
      `VALID`. This covers installs launched from CrossInk/CrossPoint's
      Settings -> SD firmware update path, where a rollback-enabled bootloader
      may otherwise boot Marigold once and then return to CrossInk after sleep.
- [x] **Flash + otadata path validated on hardware** (2026-07-05, unlocked X4).
      A one-shot self-test (`fw::ota_update::run_selftest`, `ota-selftest`
      feature) copied the running image into the inactive slot with `esp-storage`
      and switched `otadata`; the device rebooted and the ESP-IDF bootloader
      loaded the app **from the far slot** (`Loaded app from partition at offset
      0x650000`) — proving the erase/write, the seq CRC (a wrong CRC would be
      ignored), and the switch. It settled on the new slot with **no rollback
      loop** on that bootloader; rollback-enabled installs are covered by the
      boot-time mark-valid step above.
      The SD read path is separately confirmed from normal boot logs, and
      `validate_image` is host-tested — so every constituent of the SD updater
      is now exercised even though a full `FWUPDATE.BIN` run awaits a card
      reader (the author's machine has none).

- [x] **Boot-time recovery combo** (`fw::ota_update::recover_to_slot0`) — holding
      **Back + Up** at reset repoints `otadata` at slot 0 and reboots into it,
      the FreeInk `RecoveryBoot` escape hatch for backing out of a far-slot
      firmware that boots but misbehaves. Sampled in `main()` before any task
      owns the ADC. Verified on device that it does **not** false-trigger on an
      idle boot; the band values are the same ones the input task uses daily, and
      the otadata switch is the mechanism the self-test already proved.

Not yet done:

- [ ] **End-to-end `FWUPDATE.BIN` run** — the whole SD trigger in one go (drop
      the file, reboot, watch it flash + delete + reboot). Needs a way to write
      the card root; blocked only by the missing card reader, not by code.
- [ ] **Live recovery-combo press** — confirm a physical Back+Up hold detects and
      switches on device (detection reuses the input task's proven bands, so this
      is a formality). Optional on-panel progress during an update.
- [ ] **Locked-unit confirmation** — that our app-descriptor eFuse range
      satisfies the stock gate and the OEM SD updater accepts our `update.bin`.
      Needs a locked device; the author's is unlocked.
- [x] **Xteink X3 bring-up** — the `device-x3` build now has hardware-verified
      UC8253 panel and BQ27220 gauge support, and release tooling publishes X3
      app/SD images beside the X4 images.
