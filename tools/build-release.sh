#!/usr/bin/env bash
# Build the distributable firmware images for the Xteink X4.
#
# Produces, in target/release-images/:
#   firmware.bin   app-only image for OTA slot app0/ota_0 (flash to 0x10000).
#                  This is what the web flasher, `esptool write_flash 0x10000`,
#                  and the in-app SD/OTA updater consume. It updates the app in
#                  place and leaves the (stock) bootloader untouched.
#   update.bin     merged full-flash image (bootloader + partition table + app)
#                  for programming the whole 16 MB from scratch on an unlocked
#                  unit. NOTE: this replaces the bootloader too, so it is the
#                  riskier artifact on a locked unit — prefer firmware.bin there.
#
# Both carry our app descriptor (magic 0xABCD5432 at image offset 0x20) with the
# wide-open eFuse-revision range (min 0, max 65535), which is what lets the
# stock bootloader on a *locked* unit accept a non-stock image.
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

echo "==> firmware.bin (app-only, app0/ota_0 @ 0x10000)"
espflash save-image "${common[@]}" "$ELF" "$OUT/firmware.bin"

echo "==> update.bin (merged full-flash, 16 MB)"
espflash save-image "${common[@]}" --merge "$ELF" "$OUT/update.bin"

echo
echo "Artifacts in $OUT:"
ls -la "$OUT/firmware.bin" "$OUT/update.bin"
echo
echo "Flash paths:"
echo "  Unlocked, app only    : esptool.py --chip $CHIP write_flash 0x10000 $OUT/firmware.bin"
echo "  Unlocked, whole flash : esptool.py --chip $CHIP write_flash 0x0 $OUT/update.bin"
echo "  Locked (stock updater): copy an SD-updater image to the card root, then"
echo "                          power on holding Power + Up on USB power."
echo "                          See docs/FLASHING.md for the locked-device caveats."
