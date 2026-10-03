# MarigoldOS Pages Site — PRD

Status: **shipped** (July 2026). The landing page with the live emulator and
the esp-web-tools flasher is `web/index.html`, deployed by
`.github/workflows/pages.yml`; tagged releases build and publish the images
through `.github/workflows/release.yml`. The feature-card screenshot gallery
was cut from the page and its build step removed (`e0e932f`). The text below
is the original plan, scoped in a grilling session (2026-07-07).

## Summary

Grow the existing GitHub Pages emulator page into a proper **MarigoldOS
landing site**: a single, warm, brand-forward page at `/` that leads with the
live browser emulator, tells the firmware's story through a gallery of real
device screens, and gives visitors a first-class way to **flash their own X3 or
X4** — a Web-Serial flasher for unlocked units and an honest SD-card download
path for locked ones. Backing it: a **tag-triggered release pipeline** that
publishes the firmware images the flasher consumes.

This is a rename-and-grow, not a green field. The pieces that already exist and
are being extended (not replaced):

- **Live emulator** — `web/index.html`, a hand-built page with a CSS device
  shell wrapping the wasm emulator, currently owning `/`.
- **Pages workflow** — `.github/workflows/pages.yml`, builds the wasm and
  deploys the site on push to `main`.
- **`tools/build-release.sh`** — produces the firmware images (X4 + X3).
- **`docs/FLASHING.md`** — the canonical, dev-voiced install reference.
- **`fixtures/scenarios` + `fixtures/golden`** — the emulator's scenario →
  golden-PNG regression pipeline, reused here as the screenshot factory.

Out of scope for v1: writing the end-user guide and rendering multi-page docs
(deferred to a later project); any SD-card provisioning tool.

## Brand: MarigoldOS

MarigoldOS is the product name everywhere — the remote repo is already renamed;
only the local checkout still reads `xteink-x4-os` (a cleanup rider below).

- **Positioning:** open-source firmware for the Xteink X3 / X4 e-reader.
- **Icon / favicon:** an orange marigold, 🌼.
- **Feeling:** warmth. The *site* carries warm marigold/paper tones — the gray
  "desk" background of the current emulator page gets rethought. The **emulator's
  Mist Gray device shell stays as-is** (it must read as a physical object), and
  the panel itself stays clean ink-on-paper. The amber LED accent already in the
  page (`--led: #c98a2b`) is the seed of the palette.
- **Design language:** echo the on-device discipline — warmth in the palette,
  restraint in the layout. Clean, not a busy marketing page.

The tension is the point: the OS is marigold-warm in every context where it has
color (site, wordmark), and pure ink-on-paper the moment it's on the physical
e-ink panel.

## The page — single landing page at `/`, hook-first

One scrolling page. The emulator is the hook and earns trust; the flasher is the
conversion and comes after.

1. **Hero** — MarigoldOS wordmark + one-line pitch, and the **live emulator in
   the full device shell** (keeps the current book picker and public-domain
   books — the interactivity *is* the hook). A secondary "Get it on your device
   ↓" jump for people already sold.
2. **One-breath pitch** — what MarigoldOS is (open firmware for the Xteink
   X3/X4), so a stranger isn't reverse-engineering it from the emulator.
3. **Feature-card gallery** — ~6 framed device screens + copy: reading & type,
   the library, the Wi-Fi shelf, QR onboarding, sleep/battery. This is the
   "screens at a glance." Cards reuse a **bezel-only** version of the device
   frame; the hero keeps the full shell.
4. **Get it on your device** — X3/X4 selector → web flasher → locked-device SD
   download. Shows the current version and a link to that release's notes on
   GitHub.
5. **Footer** — GitHub, Docs (→ repo `docs/` for now; upgrades in place when the
   real guide exists), license, credits, an Internals → `ARCHITECTURE.md` link.

## Feature-card screenshots

Same-pixels-as-firmware, never stale, presented the pretty way.

- **Source:** real **golden frames** from `fixtures/scenarios` / `fixtures/golden`,
  extended with new scenarios to cover the marketing moments not already in the
  golden set (Wi-Fi shelf, QR onboarding, type-settings mid-adjust, etc.).
- **Presentation:** the raw goldens are 1-bit panel dumps (harsh, built for
  pixel-diffing). Run each through the **emulator's `present()` mapping** —
  1-bit → ink-on-paper (ideally ink-on-*transparent* so it drops onto the CSS
  bezel's paper), scaled smooth — so a card looks like the **web emulator**, not
  the test dump. Reuse the emulator's color mapping so the two can't diverge.
- **Wiring:** the Pages CI job regenerates the goldens, applies the transform,
  and copies the chosen frames into the site's image dir on every build.

Rejected: capturing from a headless live web emulator. The "better look" is a
deterministic recolor+scale, not different content — not worth the extra rig for
v1.

## The device frame

The CSS device shell in `web/index.html` is factored out into a **shared
visual**, used two ways:

- **Hero:** the full shell — bezel, page buttons, keys, LED — wrapping the
  *live* emulator.
- **Cards:** the identical frame with a modifier class — **bezel + screen only,
  no keys** — wrapping a *static* screenshot, so the gallery reads as a calm row
  of screens rather than six keyboards.

Same CSS, one modifier apart. Reusable on later pages too.

## Web flasher

- **Library:** **esp-web-tools**, **vendored and version-pinned** into `web/`
  (no CDN for the critical flash path). App-image-only, written to **`0x10000`**
  — never `full-flash`, never `0x0`.
- **Device disambiguation:** X3 and X4 are *both* ESP32-C3, so esp-web-tools
  can't tell them apart. The **X3/X4 selector chooses which manifest** the
  button loads (`manifest-x4.json` vs `manifest-x3.json`), each pointing at that
  device's `firmware-x*.bin` @ `0x10000`. The UI does the disambiguation the
  library can't. Both devices are frictionless — X3 is now hardware-verified.
- **Capability gating:** feature-detect `navigator.serial`. Capable browser
  (Chrome / Edge / Opera desktop) → the flash button. Anything else (Firefox,
  Safari, iOS, Android) → **no dead button**: a marigold-styled message ("Web
  flashing needs Chrome, Edge, or Opera on desktop; on any other browser or a
  phone, use the SD-card install below") that routes them to the SD download,
  which works on every OS. Uses esp-web-tools' `unsupported` slot.
- **Version display:** the section shows the current firmware version and links
  to that GitHub release's notes.
- **Graceful degrade:** if `releases/latest` 404s (before the first release
  exists), show "no release published yet," not a broken button.
- **Dialog:** accept esp-web-tools' stock install dialog for v1; theme the
  button and surrounding page to marigold so the entry point feels native.

## Locked-device path (below the flasher)

Locked units have USB disabled in eFuse — the web flasher can never see them.
Their path is SD-card install.

- **Download:** `update.bin` (X4) / `FWUPDX3.BIN` (X3) — the app image under the
  exact filename the on-device SD updater scans for. (The filename is a
  contract, not cosmetic.)
- **Honest per-case copy:** still on stock firmware → this file + the OEM
  updater combo (Power + Up); already running MarigoldOS → the in-app updater
  (reboot with the trigger file). The X3 locked-first-install story is thinner in
  the docs than X4's — the copy must not over-promise there.
- **No `full-flash` on the site, ever** — it's the instant-brick footgun and
  serves no real user (every shipped device already has the stock bootloader, so
  app-only @ `0x10000` always suffices). It stays a local dev artifact only.

## Release CI/CD (new)

Two pipelines, deliberately decoupled:

- **Pages (exists, push-to-`main`):** deploys the site + emulator wasm. The
  emulator on the site tracks **`main`** — newest features to play with.
- **Release (new, tag-triggered):** a `v*` tag runs `build-release.sh` for X4
  and X3, creates a **GitHub Release**, and uploads the assets. The thing people
  **flash** tracks the **latest tagged release** — deliberate and stable, not
  every random `main` commit.

Coupling is via GitHub's stable **`releases/latest/download/<asset>`** URL: the
flasher manifests (on Pages) hardcode it and always pull the newest tagged
firmware, with zero coordination between the two pipelines.

**Assets per release (four):**

| Asset | Job |
|---|---|
| `firmware-x4.bin` | Web flasher + `esptool @ 0x10000` (X4) |
| `firmware-x3.bin` | Web flasher + `esptool @ 0x10000` (X3) |
| `update.bin` | X4 locked-device SD install |
| `FWUPDX3.BIN` | X3 locked-device SD install |

- Rename `firmware.bin → firmware-x4.bin` as a CI step (release symmetry). Keep
  `update.bin` / `FWUPDX3.BIN` names as-is — they're contracts. **Drop
  `full-flash` from the release.**
- **CI feasibility confirmed:** `build-release.sh` needs no hardware — it's
  `cargo build -p fw --release` (nightly + `riscv32imc-unknown-none-elf`) plus
  `espflash save-image` (offline). Installable on an Ubuntu runner.
- **Versioning:** semver tags. First release **`v0.2.0`** (the firmware is well
  past a first rough cut). The tag is the release label only; wiring it into
  `esp_app_desc_t.version` so a device self-reports is a later nice-to-have, not
  v1.

## Build sequence

1. **Release CI + cut `v0.2.0`** — so the flasher has something to pull, and the
   `releases/latest/download/` URL is live.
2. **Web flasher** — vendor esp-web-tools, per-device manifests, selector,
   capability gating, locked-device download, version + release-notes link.
3. **Landing redesign** — factor the device frame into a shared visual, marigold
   palette, hero + pitch + gallery + flasher + footer.
4. **Screenshot pipeline** — extend scenarios, `present()` transform, CI copies
   frames into the site.

## Cleanup riders (not blockers)

- **Stale "X3 unverified" claims** — `FLASHING.md`, `README.md`, and the X3
  memory all still say the X3 is unverified on real silicon. It's now
  hardware-verified; correct them.
- **Local rename** — the local checkout still reads `xteink-x4-os`; update the
  local `README.md` / framing to MarigoldOS to match the remote.

## Open questions / deferred

- **User guide + multi-page docs** — deferred to a later project. The site
  launches without a guide, leaning on the feature cards and the touchable
  emulator to teach the device. The footer Docs link points at repo `docs/` for
  now and upgrades in place later.
- **Post-flash SD onboarding** — judged a non-issue for v1 (a fresh flash comes
  up usable; the device handles the card). No card-prep tool, no hand-holding
  note.
- **App-descriptor version self-reporting** — later nice-to-have.
