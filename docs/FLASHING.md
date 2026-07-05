# Flashing & release images

This firmware ships as a standard ESP32-C3 application image that boots under
the Xteink X4's **stock second-stage bootloader**. That's what makes it
installable the same way the other community firmwares (CrossPoint, CrossInk)
are — including, in principle, on *locked* units.

## Unlocked vs. locked units

Some X4s — typically the ones bought from third-party sellers (AliExpress) —
ship with **USB flashing disabled in eFuse at the factory**. Units bought
directly from xteink.com are not locked.

To tell which you have: connect over USB-C and try to flash (`cargo run` or the
web flasher). If the device never appears as a serial port even after trying
another cable/port/browser, assume it's locked.

The author's own unit is unlocked, and **the locked-device path below has not
yet been validated on real locked silicon** — see [Status](#status).

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

```sh
tools/build-release.sh
```

Produces, in `target/release-images/`:

- **`firmware.bin`** — app image for `ota_0`. Flash to `0x10000`. Updates the
  app in place and leaves the bootloader untouched. This is what the web
  flasher, `esptool write_flash 0x10000`, and (once implemented) the in-app
  updater consume.
- **`update.bin`** — byte-identical to `firmware.bin`, under the filename the
  stock OEM SD-card updater looks for. The OEM updater writes it to the app
  slot at `0x10000`, so it is an **app image, not a full-flash image**.
- **`full-flash.bin`** — merged 16 MB image (bootloader + partition table +
  app) for programming a whole *unlocked* unit from scratch with
  `esptool write_flash 0x0`.

> [!CAUTION]
> Never put `full-flash.bin` on an SD card and never write it to `0x10000`. The
> OEM SD updater writes whatever it finds to the app slot; a full-flash image
> there lands a bootloader in the middle of the app partition and bricks the
> device. Writing to `0x0` is the fastest brick on any unit. The SD card and the
> app slot only ever take `update.bin`/`firmware.bin`.

## Flashing an unlocked unit

```sh
# Everyday dev flash + serial monitor:
cargo run -p fw --release

# App-only, with esptool:
esptool.py --chip esp32c3 write_flash 0x10000 target/release-images/firmware.bin

# Whole flash from scratch:
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

## Status

Implemented and verified on host tooling:

- [x] Stock-compatible dual-OTA partition table (`partitions.csv`).
- [x] App descriptor with the open eFuse range at offset `0x20` (bootloader-gate
      workaround), verified present in the built image.
- [x] Reproducible `firmware.bin` / `update.bin` / `full-flash.bin` release
      images (`tools/build-release.sh`). The SD `update.bin` is an app image
      written to `0x10000`, matching the OEM updater.
- [x] `cargo run` flashes the stock-compatible layout.
- [x] **Image validator** (`proto::ota::validate_image`) — the integrity gate
      (magic / segment walk / XOR checksum / SHA-256 trailer) that must pass
      before any candidate `.bin` is written to the inactive slot. Streaming,
      no heap; host-tested against synthetic valid and corrupt images.
- [x] **otadata layer** (`proto::ota`: `seq_crc`, `SelectEntry`, `plan_switch`)
      — the OTA-slot select-entry format, the seq CRC (verified against the
      esp-bootloader-esp-idf algorithm *and* a real on-device value:
      `seq_crc(1) == 0x4743989A`), and the slot-switch math. Host-tested.

Not yet done (needed before locked-device install is safe):

- [ ] **Flash wiring** — connect the validator + `plan_switch` to real flash
      access (`esp-storage`): stream a validated image into the inactive OTA
      slot, then write the planned `otadata` entry and reset. The logic is done
      and tested; this is the hardware I/O around it. (Consider adopting
      `esp-bootloader-esp-idf`'s `Ota` directly instead, since our `plan_switch`
      already matches it.)
- [ ] **SD update activity** — pick a `.bin` from the card, validate, flash,
      switch, reset. This is the anti-brick net; it is the single most
      important remaining piece.
- [ ] **Boot-time recovery combo** (hold a combo at reset → repoint otadata at
      `ota_0`), mirroring the SDK's `RecoveryBoot`.
- [ ] **On-device validation** — confirm on a real *locked* unit that our
      app-descriptor eFuse range satisfies the stock gate and that the OEM
      updater accepts our `update.bin`. Untested so far: the author's unit is
      unlocked.
