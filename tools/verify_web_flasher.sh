#!/usr/bin/env bash
set -euo pipefail

site_dir=${1:?usage: tools/verify_web_flasher.sh SITE_DIR}

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

echo "web flasher assets are same-origin and present"
