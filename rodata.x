/* Override of esp-hal's rodata.x. esp32c3.x does `INCLUDE "rodata.x"`, and
   lld resolves INCLUDE against the current directory before the -L search
   paths; rustc links with the workspace root as its working directory, so
   this file shadows the copy in esp-hal's build output.

   The only change is KEEP(*(.rodata_desc)) at the head of .rodata: that pins
   _ESP_APP_DESC (fw/src/main.rs) to image offset 0x20, right after the image
   and first segment headers, where the stock bootloader, the in-app OTA
   validator and tools/verify_release_images.py expect the app descriptor.
   Without this override the descriptor lands wherever the linker places the
   orphan section and every release image fails validation. */
SECTIONS {
  .rodata : ALIGN(4)
  {
    . = ALIGN (4);
    _rodata_start = ABSOLUTE(.);
    KEEP(*(.rodata_desc));
    *(.rodata .rodata.*)
    *(.srodata .srodata.*)
    . = ALIGN(4);
    _rodata_end = ABSOLUTE(.);
  } > RODATA

  .rodata.wifi : ALIGN(4)
  {
    . = ALIGN(4);
    *( .rodata_wlog_*.* )
    . = ALIGN(4);
  } > RODATA
}
