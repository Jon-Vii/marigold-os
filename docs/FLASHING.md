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

- **`firmware.bin`** — app-only image for `ota_0`. Flash to `0x10000`. Updates
  the app in place and leaves the bootloader untouched. This is what the web
  flasher, `esptool write_flash 0x10000`, and (once implemented) the in-app
  updater consume.
- **`update.bin`** — merged full-flash 16 MB image (bootloader + partition table
  + app). For programming a whole unlocked unit from scratch. It replaces the
  bootloader too, so it is the **riskier** artifact on a locked unit.

## Flashing an unlocked unit

```sh
# Everyday dev flash + serial monitor:
cargo run -p fw --release

# App-only, with esptool:
esptool.py --chip esp32c3 write_flash 0x10000 target/release-images/firmware.bin

# Whole flash from scratch:
esptool.py --chip esp32c3 write_flash 0x0 target/release-images/update.bin
```

## Flashing a locked unit

> [!WARNING]
> On a locked unit, USB flashing is the recovery path of last resort and it's
> disabled. If you install a firmware that has **no over-the-air / SD update
> path of its own**, and USB re-locks, there is no way back. This firmware does
> not yet ship that recovery path (see [Status](#status)), so **do not install
> it on a locked unit you can't afford to brick.**

Two mechanisms exist, both pioneered by CrossPoint:

1. **Stock SD-card updater.** The stock Xteink app can update itself from an
   image on the SD card: copy the image to the card root, power on holding
   **Power + Up** while on USB power, and the stock firmware writes it. This is
   the least invasive path (no bootloader replacement). What container the stock
   updater expects for a *replacement* image is defined by closed stock firmware
   and is **not yet confirmed** for our build — see [Status](#status).

2. **External unlocker tools** (CrossPoint's USB Unlocker / OTA Unlocker) that
   re-enable USB flashing or intercept the official OTA channel. These are
   separate desktop tools, out of scope for this repo; they officially support
   only CrossPoint/CrossInk.

## Status

Implemented and verified on host tooling:

- [x] Stock-compatible dual-OTA partition table (`partitions.csv`).
- [x] App descriptor with the open eFuse range at offset `0x20` (bootloader-gate
      workaround), verified present in the built image.
- [x] Reproducible `firmware.bin` + `update.bin` release images
      (`tools/build-release.sh`).
- [x] `cargo run` flashes the stock-compatible layout.

Not yet done (needed before locked-device install is safe):

- [ ] **In-app recovery/update path** — a Rust port of the FreeInk SDK's
      otadata switch + SD/OTA firmware flasher, so an update can be applied
      (and a bad one recovered from) without USB. This is the anti-brick net;
      it is the single most important remaining piece.
- [ ] **Boot-time recovery combo** (hold a combo at reset → repoint otadata at
      `ota_0`), mirroring the SDK's `RecoveryBoot`.
- [ ] **Confirm the stock SD-updater container** and the eFuse-gate assumption
      on a real locked unit. Both are currently unverified because the author's
      device is unlocked and the stock updater format is closed.
