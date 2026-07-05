#!/usr/bin/env bash
# Build the distributable firmware images for the Xteink X4.
#
# Produces, in target/release-images/:
#   firmware.bin    app image for OTA slot app0/ota_0. Flash to 0x10000. This is
#                   what the web flasher, `esptool write_flash 0x10000`, and the
#                   in-app SD/OTA updater consume. Leaves the bootloader intact.
#   update.bin      byte-identical to firmware.bin, under the filename the stock
#                   OEM SD-card updater looks for on a locked unit's card. The
#                   OEM updater writes it to the app slot (0x10000) — it is an
#                   app image, NOT a full-flash image.
#   full-flash.bin  merged 16 MB image (bootloader + partition table + app) for
#                   programming a whole *unlocked* unit from scratch with
#                   `esptool write_flash 0x0`. NEVER put this on an SD card and
#                   NEVER write it to 0x10000 — it would land a bootloader in the
#                   app slot and brick the device.
#
# firmware.bin/update.bin carry our app descriptor (magic 0xABCD5432 at image
# offset 0x20) with the wide-open eFuse-revision range, which is what lets the
# stock bootloader on a locked unit accept a non-stock image.
#
# Usage: tools/build-release.sh
set -euo pipefail

cd "$(dirname "$0")/.."

CHIP=esp32c3
FLASH_SIZE=16mb
PARTS=partitions.csv
APP_LABEL=app0                # the ota_0 partition's label in partitions.csv
ELF=target/riscv32imc-unknown-none-elf/release/fw
OUT=target/release-images

echo "==> building fw (release)"
cargo build -p fw --release

mkdir -p "$OUT"

# espflash validates the app descriptor against its own schema and rejects our
# hand-rolled one; --ignore-app-descriptor skips that check. The descriptor is
# still present and correctly placed at image offset 0x20 for the bootloader.
common=(--chip "$CHIP" --flash-size "$FLASH_SIZE"
        --partition-table "$PARTS" --target-app-partition "$APP_LABEL"
        --ignore-app-descriptor)

echo "==> firmware.bin (app image, app0/ota_0 @ 0x10000)"
espflash save-image "${common[@]}" "$ELF" "$OUT/firmware.bin"

echo "==> update.bin (same app image, name the OEM SD updater reads)"
cp "$OUT/firmware.bin" "$OUT/update.bin"

echo "==> full-flash.bin (merged 16 MB, unlocked-only, write to 0x0)"
espflash save-image "${common[@]}" --merge "$ELF" "$OUT/full-flash.bin"

echo
echo "Artifacts in $OUT:"
ls -la "$OUT/firmware.bin" "$OUT/update.bin" "$OUT/full-flash.bin"
echo
echo "Flash paths (see docs/FLASHING.md):"
echo "  Locked (stock updater): copy update.bin to the SD card root, then power"
echo "                          on holding Power + Up on USB power."
echo "  Unlocked, app only    : esptool.py --chip $CHIP write_flash 0x10000 $OUT/firmware.bin"
echo "  Unlocked, whole flash : esptool.py --chip $CHIP write_flash 0x0 $OUT/full-flash.bin"
