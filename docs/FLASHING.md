# Flashing & release images

This firmware ships as a standard ESP32-C3 application image that boots under
the Xteink X4/X3 **stock second-stage bootloader**. That is why it installs the
same way as the other community firmwares (CrossPoint, CrossInk), and in
principle that includes *locked* units.

## Unlocked vs. locked units

Some X4s and X3s leave the factory with **USB flashing disabled in eFuse**.
These are usually units bought from third-party sellers such as AliExpress.
Units bought directly from xteink.com are not locked.

To find out which kind you have, connect over USB (USB-C on the X4, the 4-pin
pogo cable on the X3) and try to flash it with `cargo run` or the web flasher.
If the device never shows up as a serial port, even after you try another
cable, port or browser, assume it's locked.

The author's own X4 is unlocked, so the locked-device path still has to be
confirmed on a real locked unit (see [Status](#status)).

## Marigold/CrossPoint layout

`partitions.csv` defines the layout used by Marigold full-flash images and the
CrossPoint firmware family:

| Partition | Type | Offset | Size |
|---|---|---|---|
| nvs | data/nvs | `0x9000` | 20 KiB (`0x5000`) |
| otadata | data/ota | `0xe000` | 8 KiB (`0x2000`) |
| app0 | app/ota_0 | `0x10000` | 6.25 MiB (`0x640000`) |
| app1 | app/ota_1 | `0x650000` | 6.25 MiB (`0x640000`) |
| spiffs | data/spiffs | `0xc90000` | 3.375 MiB (`0x360000`) |
| coredump | data/coredump | `0xff0000` | 64 KiB (`0x10000`) |

The app image is about 4.2 MB, so it fits in `ota_0` with room to spare.
`cargo run` flashes against this table (see `.cargo/config.toml`).

### Flash clock

The second-stage bootloader sets how fast the chip reads its flash. The
firmware runs from flash through a 16 KB cache, so the flash clock shows up in
every cache miss. On the X4, running at 80 MHz instead of 40 MHz takes a cold
book build from 65 s to 52 s, and most of what remains is layout and parsing.
`cargo run` and the `full-flash*.bin` images write a bootloader set to dual I/O
at 80 MHz, as CrossPoint's is. espflash on its own would write 40 MHz. Quad I/O
isn't an option because it needs GPIO12, which is the SD card's chip select.
App-only installs (`0x10000`, the web flasher, SD updates) keep whatever
bootloader the device already has.

### Why the stock bootloader accepts our image

The X4 bootloader checks each image against an eFuse block-revision range that
it reads from the app descriptor (`esp_app_desc_t`). We emit that descriptor in
`fw/src/main.rs` (`_ESP_APP_DESC`, magic `0xABCD5432`) at image offset `0x20`,
which is exactly where the bootloader reads it. It sets
`min_efuse_blk_rev_full = 0` and `max_efuse_blk_rev_full = 65535`, meaning
"accept any revision". The other firmwares get past the same gate with a
build-time patch; we satisfy it directly in the descriptor. To check the
placement:

```sh
xxd -s 0x20 -l 4 target/release-images/firmware.bin   # -> 3254 cdab (0xABCD5432 LE)
```

## Building the release images

Release builds need Rust from `rustup`, the firmware target, and `espflash`:

```sh
rustup target add riscv32imc-unknown-none-elf
cargo install espflash
```

```sh
tools/build-release.sh        # X4 (default)
tools/build-release.sh x3     # X3
```

The script writes local images to `target/release-images/`:

- `firmware.bin` is the app image for `ota_0`. Flash it to `0x10000`. It
  updates the app in place and leaves the bootloader alone. The web flasher,
  `esptool write_flash 0x10000` and the in-app updater all use this image. The
  web installer explicitly forbids a whole-chip erase for these app-only
  manifests, and CI checks that before deploying to Pages.
- `firmware-x3.bin` is the same kind of app image, built for the X3.
- `update.bin` is byte-identical to `firmware.bin`, under the filename the X4
  stock OEM SD-card updater looks for. The OEM updater writes it to the app
  slot at `0x10000`, so it is an **app image, not a full-flash image**.
- `update-x3.bin` is byte-identical to `firmware-x3.bin`. Rename it to
  `update.bin` on the card for X3 OEM bootloaders that use that filename.
- `FWUPDATE.BIN` is byte-identical to `firmware.bin`, under the filename
  MarigoldOS itself looks for on the card root at boot.
- `FWUPDX3.BIN` is the X3 SD-card trigger filename.
- `full-flash*.bin` are merged 16 MB images (bootloader + partition table +
  app) for local bench recovery on unlocked units only.

Tagged GitHub releases publish one canonical app image per hardware target,
`firmware-x4.bin` and `firmware-x3.bin`, plus a `SHA256SUMS` file. The release
doesn't repeat those bytes under updater-specific names. Rename the image for
your device to match how you're installing it:

| Installation path | X4 card filename | X3 card filename |
| --- | --- | --- |
| Stock/OEM SD updater | `update.bin` | `update.bin` |
| Marigold automatic boot-time update | `FWUPDATE.BIN` | `FWUPDX3.BIN` |
| USB, web flasher, or Settings firmware picker | no rename required | no rename required |

The project website serves the same canonical image under the right download
filename for each SD workflow, so if you download from the website you don't
need to rename anything. Downloads straight from a GitHub release keep their
canonical filename. The extra aliases that `tools/build-release.sh` produces
are there for local development; they aren't separate release payloads.

> [!CAUTION]
> Never put `full-flash*.bin` on an SD card and never write it to `0x10000`.
> The OEM SD updater writes whatever it finds to the app slot, and a full-flash
> image there puts a bootloader in the middle of the app partition and bricks
> the device. An app image written to `0x0` replaces the bootloader and bricks any unit. The SD card
> and the app slot only ever take `update.bin`/`FWUPDX3.BIN` or the app-only
> `firmware-*.bin` images.

## Xteink X3

The X3 is the X4's sibling. It has the same ESP32-C3 and 16 MB of flash, but
stock X3 units may still have a different dual-OTA partition table (`ota_1` at
`0x780000`, with 7.44 MiB (`0x770000`) slots) instead of the Marigold/CrossPoint
layout (`ota_1` at `0x650000`, with 6.25 MiB (`0x640000`) slots). It uses a
smaller 792×528 UC8253 panel, with a BQ27220 battery gauge in place of the X4's
ADC divider. Support lives behind the `device-x3` feature, and the default
build for the X4 is unchanged. X3 support has been validated on hardware, so
the web flasher and release pipeline publish X3 images alongside the X4 ones.

Build the X3 images:

```sh
tools/build-release.sh x3
```

This puts four files in `target/release-images/`: `firmware-x3.bin` (flash to
`0x10000`), `update-x3.bin` (rename to `update.bin` for the stock X3 OEM
updater), `FWUPDX3.BIN` (Marigold's X3 one-shot trigger) and
`full-flash-x3.bin` (a local whole-flash image for unlocked bench units only).
Some stock X3 variants also recognize `FWUPDX3.BIN`. The website offers the
canonical X3 image under either card filename and never mixes in the X4 image.
Otherwise the app-only flash paths are the same as for the X4, described below.

> [!NOTE]
> The X3 charges and flashes through a **4-pin magnetic pogo connector**, not
> USB-C. The 2-pin version of that cable only charges and won't show up as a
> serial port. Serial goes through the same native USB-Serial-JTAG as on the X4.

When testing, capture the serial log (`cargo run` monitors it, or use
`espflash monitor`). Panel init, the `display: x3 busy` lines, per-refresh
timings, and the `input: battery seeded` / `battery: gauge` lines are what
show which bring-up value needs adjusting (BUSY timing → orientation →
waveforms → battery).

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
> On a locked unit, USB flashing is disabled, and it's the recovery path of
> last resort. If you install a firmware that has **no over-the-air / SD update
> path of its own** and USB re-locks, there is no way back. This firmware does
> ship an SD update path (see [In-app update](#in-app-update-the-recovery-net)),
> but it has not yet been confirmed on a locked unit (see [Status](#status)).
> **Do not install it on a locked unit you can't afford to brick.**

There are two ways to install on a locked unit, both pioneered by CrossPoint:

1. **Stock SD-card updater.** The OEM bootloader/app can update from an image
   on the SD card. Copy **`update.bin`** to the card root, then power on while
   holding **Power + Up** with the device on USB power. It writes the image to
   the app slot at `0x10000` and doesn't replace the bootloader. Some builds
   also flash a file named `force_update.bin` automatically on boot, with no
   button combo, which makes it a handy recovery file to keep on the card. This
   path does **not** re-enable USB flashing. It is the standard install route
   for locked/AliExpress units.

2. **External unlocker tools** (CrossPoint's USB Unlocker / OTA Unlocker),
   which re-enable USB flashing or intercept the official OTA channel. They are
   separate desktop tools and out of scope for this repo, and they officially
   support only CrossPoint/CrossInk.

## In-app update (the recovery net)

Once *any* build of this firmware is running, it can update itself from the
card without a computer. That is what keeps a locked unit from being a one-way
trip.

### Settings picker

Put one or more app-image `.BIN` files at the card root, then open
**Settings → Firmware update**. The picker uses the same Previous/Next list
controls as Library. Confirm selects an image, and a second confirmation
schedules it and restarts the device. The image stays on the card afterwards,
so one card can serve as a reusable firmware shelf.

The picker stores only the selected file's FAT 8.3 alias, in
`/XTEINK/FWPEND.BIN`. On the next boot the existing updater reopens that exact
file, reads the installed partition table, validates the complete ESP
application image against the inactive slot it found there, writes that slot,
and switches `otadata`. Reading the table is required on app-only X3
installations, where the stock and Marigold/CrossPoint `ota_1` offsets differ.
Whether the attempt fails or completes, only the pending-selection record is
cleared.

Only select app images meant for the current X3/X4. Generic ESP image
validation proves integrity and that the image fits the partition. It can't
prove that a third-party image drives the correct panel or keeps an SD
recovery path.

### Automatic trigger

1. Copy a new app image to the card root as **`FWUPDATE.BIN`**. This is the
   file `tools/build-release.sh` produces for the X4. The `FWUPDATE.BIN` name
   is a one-shot trigger, kept separate from a permanent `update.bin` you may
   also keep on the card. On the **X3** the trigger is **`FWUPDX3.BIN`**
   instead. Each build only picks up an image named for its own panel, so you
   can carry a card between an X4 and an X3 without either one grabbing the
   other's image. The two share a SoC but not a display controller or battery
   gauge, so flashing one's image onto the other gives a black screen.
2. Reboot. At boot, before the reader starts, the firmware validates the image
   (`proto::ota::validate_image`), finds the **inactive** OTA slot from the
   installed partition table and writes the image there. It then deletes
   `FWUPDATE.BIN` so the update runs only once, flips `otadata` to select the
   new slot (`proto::ota::plan_switch`), and resets into the new firmware.
   On the first boot after any OTA-slot install (including CrossInk's
   Settings -> SD firmware update flow), Marigold marks the selected `otadata`
   entry valid before the reader starts. Without that, a rollback-enabled
   bootloader would return to the previous firmware on the next deep-sleep
   reset.

Only the inactive slot and the inactive `otadata` sector are written, so a bad
or half-copied image never harms the running firmware. The bootloader keeps
booting the current slot until a complete, valid image flips `otadata`. This
also works on an unlocked unit, because espflash's bootloader is ESP-IDF and
honours `otadata`. That's how to test it without a locked device.

If an update leaves you on a firmware that boots but misbehaves, hold
**Back + Up** at reset. The recovery hatch points `otadata` back at slot 0 (the
firmware first installed there) and reboots into it. It can't help with a
firmware that doesn't boot far enough to run the check. That would need a
custom bootloader, and no app-level firmware provides one. Treat it as a strong
safety net, but not a guarantee against every brick.

## Status

Implemented and verified on host tooling:

- [x] Marigold/CrossPoint dual-OTA partition table (`partitions.csv`) plus
      runtime discovery of the different stock X3 OTA offsets for app-only
      installations.
- [x] App descriptor with the open eFuse range at offset `0x20` (the
      bootloader-gate workaround), verified present in the built image.
- [x] Reproducible canonical app images (`firmware.bin`, `firmware-x3.bin`),
      local updater-name aliases (`update.bin`, `update-x3.bin`,
      `FWUPDATE.BIN`, `FWUPDX3.BIN`), and local-only `full-flash*.bin` bench
      images (`tools/build-release.sh`). Tagged releases publish only the two
      canonical device images plus `SHA256SUMS`. Every SD alias is still an
      app image written to `0x10000`, matching the OEM updater.
- [x] `cargo run` flashes the stock-compatible layout.
- [x] Image validator (`proto::ota::validate_image`). This is the integrity
      gate (magic / segment walk / XOR checksum / SHA-256 trailer) that must
      pass before any candidate `.bin` is written to the inactive slot. It
      streams, uses no heap, and is host-tested against synthetic valid and
      corrupt images.
- [x] otadata layer (`proto::ota`: `seq_crc`, `SelectEntry`, `plan_switch`,
      `active_app_slot`): the OTA-slot select-entry format, the seq CRC and the
      slot-switch math. The seq CRC is verified against the
      esp-bootloader-esp-idf algorithm *and* a real on-device value
      (`seq_crc(1) == 0x4743989A`). Host-tested.
- [x] Boot-time SD updater (`fw::ota_update`). On boot, `/FWUPDATE.BIN` is
      validated, written with `esp-storage` to the inactive slot found in the
      installed partition table, selected via `otadata` and deleted, and the
      device resets into it. Only the inactive slot and sector are touched.
- [x] Settings firmware picker. It scans root-level `.BIN` app images into a
      bounded native list, asks for an explicit second confirmation, saves
      the selected FAT alias, and reuses the boot-time validator and flasher
      while leaving reusable source images on the card.
- [x] OTA rollback acknowledgement
      (`fw::ota_update::mark_running_slot_valid`). Early in boot, an active
      `NEW`/`PENDING_VERIFY` select entry is rewritten as `VALID`. This covers
      installs started from CrossInk/CrossPoint's Settings -> SD firmware
      update path, where a rollback-enabled bootloader might otherwise boot
      Marigold once and then go back to CrossInk after sleep.
- [x] Flash and otadata path validated on hardware (2026-07-05, unlocked X4).
      A one-shot self-test (`fw::ota_update::run_selftest`, `ota-selftest`
      feature) copied the running image into the inactive slot with
      `esp-storage` and switched `otadata`. The device rebooted and the ESP-IDF
      bootloader loaded the app **from the far slot** (`Loaded app from
      partition at offset 0x650000`). That proves the erase/write, the seq CRC
      (a wrong CRC would be ignored) and the switch. It settled on the new slot
      with **no rollback loop** on that bootloader; the boot-time mark-valid
      step above covers rollback-enabled installs.
      The SD read path is confirmed separately from normal boot logs, and
      `validate_image` is host-tested. So every part of the SD updater has
      been exercised, even though a full `FWUPDATE.BIN` run is still waiting on
      a card reader (the author's machine has none).

- [x] Boot-time recovery combo (`fw::ota_update::recover_to_slot0`). Holding
      **Back + Up** at reset points `otadata` at slot 0 and reboots into it.
      This is the FreeInk `RecoveryBoot` escape hatch, for backing out of a
      far-slot firmware that boots but misbehaves. The buttons are sampled in
      `main()` before any task owns the ADC. Verified on device that it does
      **not** trigger by mistake on an idle boot. The band values are the same
      ones the input task uses every day, and the otadata switch is the
      mechanism the self-test already proved.
- [x] Xteink X3 bring-up. The `device-x3` build has hardware-verified UC8253
      panel and BQ27220 gauge support, and the release tooling publishes X3
      app/SD images beside the X4 images.

Not yet done:

- [ ] End-to-end `FWUPDATE.BIN` run: the whole SD trigger in one go (drop the
      file, reboot, watch it flash, delete and reboot). It needs a way to
      write the card root, and only the missing card reader is blocking it,
      not code.
- [ ] Live recovery-combo press. Confirm on device that a physical Back+Up
      hold is detected and switches slots. Detection reuses the input task's
      proven bands, so this is a formality. Optional on-panel progress during
      an update.
- [ ] Locked-unit confirmation, meaning that our app-descriptor eFuse range
      satisfies the stock gate and the OEM SD updater accepts our
      `update.bin`. Needs a locked device; the author's is unlocked.
