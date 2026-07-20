#!/usr/bin/env bash
# Build the distributable firmware images for the Xteink X4 or X3.
#
# Usage: tools/build-release.sh [x4|x3]   (default x4)
#
# Produces local development images in target/release-images/ (X3 images carry an
# -x3 suffix):
#   firmware.bin    app image for OTA slot app0/ota_0. Flash to 0x10000. This is
#                   what the web flasher, `esptool write_flash 0x10000`, and the
#                   in-app SD/OTA updater consume. Leaves the bootloader intact.
#   update.bin      X4 stock/OEM SD updater filename.
#   update-x3.bin   X3 stock/OEM SD updater image; rename to update.bin on card.
#   FWUPDATE.BIN    X4 in-app SD updater filename.
#   FWUPDX3.BIN     X3 in-app SD updater filename. The name is
#                   device-specific so a card can't cross-flash the wrong panel.
#   full-flash.bin  merged 16 MB image (bootloader + partition table + app) for
#                   programming a whole *unlocked* unit from scratch with
#                   `esptool write_flash 0x0`. NEVER put this on an SD card and
#                   NEVER write it to 0x10000 — it would land a bootloader in the
#                   app slot and brick the device.
#
# GitHub releases publish only app/SD images: firmware-x4.bin, firmware-x3.bin,
# update.bin, update-x3.bin, FWUPDATE.BIN, and FWUPDX3.BIN. full-flash*.bin
# remains local-only.
#
# The app images carry our app descriptor (magic 0xABCD5432 at image offset
# 0x20) with the wide-open eFuse-revision range, which is what lets the stock
# bootloader on a locked unit accept a non-stock image.
set -euo pipefail

cd "$(dirname "$0")/.."

# STOCK_SD_ARTIFACT is the unique release filename; STOCK_SD_CARD_NAME is the
# name expected at the card root. X3 gets a unique artifact so it cannot
# overwrite the X4 update.bin when both builds share the release directory.
# IN_APP_SD_IMAGE is the trigger filename Calendula itself consumes on boot.
DEVICE="${1:-x4}"
case "$DEVICE" in
  x4) FEATURES=(); SUFFIX=""; STOCK_SD_ARTIFACT=update.bin; STOCK_SD_CARD_NAME=update.bin; IN_APP_SD_IMAGE=FWUPDATE.BIN ;;
  x3) FEATURES=(--features device-x3); SUFFIX="-x3"; STOCK_SD_ARTIFACT=update-x3.bin; STOCK_SD_CARD_NAME=update.bin; IN_APP_SD_IMAGE=FWUPDX3.BIN ;;
  *)  echo "usage: $0 [x4|x3]" >&2; exit 2 ;;
esac

CHIP=esp32c3
FLASH_SIZE=16mb
PARTS=partitions.csv
APP_LABEL=app0                # the ota_0 partition's label in partitions.csv
ELF=target/riscv32imc-unknown-none-elf/release/fw
OUT=target/release-images
FW="$OUT/firmware$SUFFIX.bin"
FULL="$OUT/full-flash$SUFFIX.bin"

echo "==> building fw ($DEVICE, release)"
if ((${#FEATURES[@]})); then
  tools/cargo.sh build -p fw --release "${FEATURES[@]}"
else
  tools/cargo.sh build -p fw --release
fi

mkdir -p "$OUT"

# espflash validates the app descriptor against its own schema and rejects our
# hand-rolled one; --ignore-app-descriptor skips that check. The descriptor is
# still present and correctly placed at image offset 0x20 for the bootloader.
common=(--chip "$CHIP" --flash-size "$FLASH_SIZE"
        --partition-table "$PARTS" --target-app-partition "$APP_LABEL"
        --ignore-app-descriptor)

echo "==> firmware$SUFFIX.bin (app image, app0/ota_0 @ 0x10000)"
espflash save-image "${common[@]}" "$ELF" "$FW"

echo "==> $STOCK_SD_ARTIFACT (same app image, stock/OEM SD updater artifact)"
cp "$FW" "$OUT/$STOCK_SD_ARTIFACT"
if [[ "$IN_APP_SD_IMAGE" != "$STOCK_SD_ARTIFACT" ]]; then
  echo "==> $IN_APP_SD_IMAGE (same app image, Calendula in-app SD updater name)"
  cp "$FW" "$OUT/$IN_APP_SD_IMAGE"
fi

echo "==> full-flash$SUFFIX.bin (merged 16 MB, unlocked-only, write to 0x0)"
espflash save-image "${common[@]}" --merge "$ELF" "$FULL"

echo
echo "Artifacts in $OUT:"
artifacts=("$FW" "$OUT/$STOCK_SD_ARTIFACT" "$FULL")
if [[ "$IN_APP_SD_IMAGE" != "$STOCK_SD_ARTIFACT" ]]; then
  artifacts+=("$OUT/$IN_APP_SD_IMAGE")
fi
ls -la "${artifacts[@]}"
echo
echo "Flash paths (see docs/FLASHING.md):"
echo "  Calendula SD update: copy $IN_APP_SD_IMAGE to the SD card root, then reboot."
echo "  Stock/OEM updater : copy $STOCK_SD_ARTIFACT as $STOCK_SD_CARD_NAME to the SD card root, then power"
echo "                      on holding Power + Up on USB power."
echo "  Unlocked, app only: esptool.py --chip $CHIP write_flash 0x10000 $FW"
echo "  Unlocked, whole   : esptool.py --chip $CHIP write_flash 0x0 $FULL"
