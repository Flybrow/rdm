#!/bin/sh
# Chromium flavour of the extension (Chrome, Brave, Opera, Edge, Vivaldi, Chromium):
#   sh packaging/chromium/build.sh [output dir, default target/chromium]
#
# The folder loads as is ("Charger l'extension non empaquetée", developer mode on), and
# rdm-chromium.zip next to it is that folder as one file to hand around. A .crx cannot serve this
# purpose: Chrome, Brave and Edge only install .crx packages coming from their store. RDM itself
# installs the extension in one click (the extension window) and keeps it up to date.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
src="$root/extension"
out=${1:-"$root/target/chromium"}

rm -rf "$out"
mkdir -p "$out"
cp -R "$src/." "$out/"
rm -rf "$out/test"

grep -q '"service_worker": "background.js"' "$out/manifest.json" || {
    echo "build.sh: background service worker not found in extension/manifest.json" >&2
    exit 1
}
zip_path="$(dirname "$out")/rdm-chromium.zip"
rm -f "$zip_path"
if command -v zip >/dev/null 2>&1; then
    (cd "$out" && zip -qr "$zip_path" .)
    echo "Chromium extension: $out (+ $zip_path)"
else
    echo "Chromium extension: $out (zip not installed: no rdm-chromium.zip)"
fi
