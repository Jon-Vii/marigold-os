#!/usr/bin/env bash
set -euo pipefail

site_dir=${1:?usage: tools/verify_web_flasher.sh SITE_DIR [--safety-only]}

python3 - "$site_dir" <<'PY'
import json
import pathlib
import sys

site = pathlib.Path(sys.argv[1])
for manifest_path in sorted(site.glob("manifest-x*.json")):
    manifest = json.loads(manifest_path.read_text())
    if manifest.get("new_install_prompt_erase") is not False:
        raise SystemExit(f"{manifest_path}: app-only installs must not prompt for erase")
    if manifest.get("full_erase_allowed") is not False:
        raise SystemExit(f"{manifest_path}: app-only installs must forbid whole-chip erase")
    builds = manifest.get("builds") or []
    if not builds:
        raise SystemExit(f"{manifest_path}: no firmware builds")
    for build in builds:
        parts = build.get("parts") or []
        if not parts or any(part.get("offset") != 0x10000 for part in parts):
            raise SystemExit(
                f"{manifest_path}: app-only parts must all target 0x10000"
            )

dialog_path = site / "vendor/esp-web-tools/web/install-dialog-C5LjR_e6.js"
dialog = dialog_path.read_text()
try:
    no_improv = dialog.split("}_renderDashboardNoImprov(){", 1)[1].split(
        "_renderProvision()", 1
    )[0]
except IndexError as error:
    raise SystemExit(f"{dialog_path}: cannot locate no-Improv install path") from error
if "_startInstall(!1)" not in no_improv or "_startInstall(!0)" in no_improv:
    raise SystemExit(f"{dialog_path}: no-Improv app install is not unconditionally non-erasing")
if "full_erase_allowed" not in dialog or "[0,32768,57344,65536]" not in dialog:
    raise SystemExit(f"{dialog_path}: full erase is not gated by a complete recovery bundle")
PY

if [[ ${2:-} == --safety-only ]]; then
  echo "web flasher is app-only and non-erasing"
  exit 0
fi

for manifest in "$site_dir"/manifest-x*.json; do
  while IFS= read -r path; do
    case "$path" in
      http://*|https://*|/*)
        echo "flasher asset must be a same-origin relative path: $path" >&2
        exit 1
        ;;
    esac
    if [[ ! -f "$site_dir/$path" ]]; then
      echo "flasher asset is missing from the published site: $path" >&2
      exit 1
    fi
  done < <(awk -F '"' '/"path"[[:space:]]*:/{print $4}' "$manifest")
done

echo "web flasher is app-only, non-erasing, same-origin, and complete"
