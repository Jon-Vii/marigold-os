# MarigoldOS

MarigoldOS is a lightweight, open-source firmware for the Xteink X4 and X3 e-readers.

[![Flashing](https://img.shields.io/badge/Flashing-2b2b2b?style=flat)](docs/FLASHING.md)
[![Architecture](https://img.shields.io/badge/Architecture-2b2b2b?style=flat)](docs/ARCHITECTURE.md)

![The MarigoldOS site showing the browser emulator home menu with Alice's Adventures in Wonderland selected](docs/home.png)

If you'd like to explore the OS without flashing a device,
[try the emulator](https://jon-vii.github.io/marigold-os/) in your browser — the
firmware's app and rendering code compiled to WebAssembly, with a simulated
e-ink display and a selection of public-domain books.

## Features

### Reading
- **EPUB 2 & 3** — native table of contents for each (EPUB 3 nav, NCX fallback)
- **Two typefaces** — Literata and Merriweather, with adjustable size, weight, and line spacing
- **Whole-book pagination cache** — a book parses once and reopens in tens of milliseconds
- **Fast page turns** — 473 ms end-to-end, within ~50 ms of the panel's rated floor

### Library & sync
- **Streamed catalog** — library size isn't bounded by RAM
- **Local Wi-Fi shelf** — upload, list, and delete books from your browser
- **Zero-config onboarding** — with no stored credentials, the reader raises an open hotspot with a captive portal and an on-screen QR code

### Try it
- **Browser emulator** — the real render code in WebAssembly, no device needed

## Development

```sh
cargo run -p fw --release                                       # build, flash, serial monitor
cargo test -p app-core -p proto --target aarch64-apple-darwin   # host tests
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin \
  --no-default-features -- --scenario fixtures/scenarios --check fixtures/golden
```

Only flashing needs the device on USB; the app logic, parsers, renderer, and
emulator all build and test on a plain host. The stable toolchain is configured in
`rust-toolchain.toml`.

## Flashing

`cargo run` flashes over USB for development. To install without a toolchain —
from a built image, or onto a unit that shipped with USB flashing disabled —
tagged releases publish the app/SD images and [docs/FLASHING.md](docs/FLASHING.md)
covers the paths: web flasher, SD card, and the in-app update from the card.

## Credits

- [Literata](https://github.com/googlefonts/literata) and [Merriweather](https://github.com/SorkinType/Merriweather) (both OFL) for the reading typefaces
- [The OpenX4 community SDK](https://github.com/open-x4-epaper/community-sdk) for panel addressing behavior
- [Crosspoint Reader](https://github.com/crosspoint-reader/crosspoint-reader) for the community reverse-engineering behind X3 device support

## License

MIT
