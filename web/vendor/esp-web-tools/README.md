# esp-web-tools vendor bundle

This directory vendors `esp-web-tools` version `10.2.1` from npm:

`https://registry.npmjs.org/esp-web-tools/-/esp-web-tools-10.2.1.tgz`

Only `dist/web/` is checked in, under `web/`, so the GitHub Pages flasher does
not depend on a CDN for the critical install path. The package is Apache-2.0
licensed by the ESPHome maintainers.

## Local patch: non-erasing installs

Every file matches the npm tarball except `web/install-dialog-C5LjR_e6.js`,
which is patched so the web flasher can never erase a reader. MarigoldOS ships
an app-only image for `0x10000`; erasing the chip would wipe the stock
bootloader and partition table that image depends on.

| File | sha256 |
| --- | --- |
| upstream `dist/web/install-dialog-C5LjR_e6.js` | `a96f7631fdd8d6e0ba25b6ca5504a46ebbb48ce3cfd901159b955d721236a283` |
| patched `web/install-dialog-C5LjR_e6.js` | `5751da36ccefb538d349baa3b1e6b6206cf8349958f7870f99a115e8c55c8860` |

The patch changes four click handlers in the minified dialog:

- **Install/Update (Improv dashboard):** upstream installs without erasing only
  for the same firmware, otherwise it shows the erase prompt (when
  `new_install_prompt_erase` is set) or erases. Patched: always
  `_startInstall(!1)`, never erasing.
- **Install (no-Improv dashboard):** the path a MarigoldOS reader takes, since
  it does not speak Improv. Upstream prompts or erases the same way. Patched:
  always `_startInstall(!1)`.
- **Erase prompt "Next" button:** upstream passes the "Erase device" checkbox to
  `_startInstall`. Patched: always `_startInstall(!1)`, ignoring the checkbox.
- **"Erase User Data" (same version installed):** upstream offers it whenever
  the device already runs this version. Patched: only when the manifest sets
  `full_erase_allowed` and every build has parts at all four offsets of a
  complete recovery bundle (`0x0`, `0x8000`, `0xE000`, `0x10000`), so a
  whole-chip erase is always followed by a full reflash. The MarigoldOS
  manifests set `full_erase_allowed: false`.

`tools/verify_web_flasher.sh` guards the patch, and CI runs it with
`--safety-only` on `web/`. It fails if any `manifest-x*.json` does not set
`new_install_prompt_erase: false` and `full_erase_allowed: false` or has a
part outside `0x10000`; if `index.html` loses the updater download names; if
the no-Improv install path stops calling `_startInstall(!1)` or contains
`_startInstall(!0)`; or if the dialog no longer gates erasing on
`full_erase_allowed` and the `[0,32768,57344,65536]` offset list. The Pages
deploy runs the full check on the assembled site, which also requires every
manifest path to be a same-origin file that exists.

To update esp-web-tools, unpack the new tarball's `dist/web/` here, reapply the
four edits above to the new `install-dialog-*.js` (the hash in its name
changes), point `tools/verify_web_flasher.sh` at the new filename, run it, and
update the hashes in this file.
