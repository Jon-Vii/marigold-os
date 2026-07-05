# xteink-x4-os

![The X4 home screen in the browser emulator, showing Alice's Adventures in Wonderland](docs/home.png)

If you would like to explore the OS without flashing a device,
you may do so [in your browser](https://jon-vii.github.io/xteink-x4-os/),
the firmware's app and rendering code compiled to WebAssembly, with simulated e-ink and a selection of public-domain books.

## Features

- Every surface renders landscape; the X4 is held sideways for its page
  buttons.
- EPUBs parse once, streaming ZIP and XHTML into a whole-book pagination
  cache on the card. A cached book reopens in tens of milliseconds.
- Literata in four styles, pre-rendered to bitmap glyphs on the host.
  Font size and line spacing are adjustable, and a spacing change
  repaginates without reparsing the book.
- The library streams from a catalog snapshot on the card, so its size
  is not bounded by RAM.
- The device joins your Wi-Fi and serves a shelf page on your LAN: list,
  upload, and delete books from any browser. The radio needs ~100 KB of
  heap the firmware does not have, so a session loans it out of the
  reader's scratch buffers and ends with a reset that hands them back.
- With no stored credentials, the device raises an open `XTEINK-X4`
  hotspot with a captive portal and an on-screen QR code.
- Idle ends in a sleep screen, then the panel and the SoC enter deep
  sleep. The power button takes the same path.

## Performance

| | |
|---|---|
| Page turn | 473 ms end-to-end; 421 ms of that is the panel's rated fast waveform |
| Wake from sleep | one flicker, ~1.5 s |
| Cold-boot full refresh | 3.5 s |
| Reopen a cached book | tens of milliseconds |
| RAM | 400 KB SRAM, no PSRAM |
| Usable stack | ~43 KB |
| Framebuffer | one, 48 KB, 1 bit per pixel |

Internals: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)

## Development

```sh
cargo run -p fw --release                                       # build, flash, serial monitor
cargo test -p app-core -p proto --target aarch64-apple-darwin   # host tests
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin \
  --no-default-features -- --scenario fixtures/scenarios --check fixtures/golden
```

Only flashing needs the device on USB; the app logic, parsers, renderer,
and emulator all build and test on a plain host. The nightly toolchain is
pinned in `rust-toolchain.toml`.

## Flashing

`cargo run` flashes over USB for development. To install without a toolchain —
from a built image, or onto a unit that shipped with USB flashing disabled —
`tools/build-release.sh` produces the images and [docs/FLASHING.md](docs/FLASHING.md)
covers the paths: web flasher, SD card, and the in-app update from the card.

## Credits

- [Literata](https://github.com/googlefonts/literata) and [Merriweather](https://github.com/SorkinType/Merriweather) (both OFL) for the reading typefaces
- [esp-hal](https://github.com/esp-rs/esp-hal) and [Embassy](https://embassy.dev) underneath everything
- The OpenX4 community SDK for the panel's addressing behavior

## License

MIT
