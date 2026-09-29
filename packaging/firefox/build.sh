#!/bin/sh
# Firefox flavour of the extension: sh packaging/firefox/build.sh [output dir, default target/firefox]
#
# Chrome/Brave load extension/ as is: an MV3 background *service worker*, and they flag
# `background.scripts` as an error. Firefox has no extension service workers and needs
# `background.scripts`. One manifest cannot satisfy both, so Firefox gets this derived copy
# (Chrome-only keys dropped; `webRequestBlocking` added: Chrome MV3 refuses it, Firefox needs it to
# take a download over before it starts). Load it via about:debugging, or zip it into an .xpi to sign.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
src="$root/extension"
out=${1:-"$root/target/firefox"}

rm -rf "$out"
mkdir -p "$out"
cp -R "$src/." "$out/"
rm -rf "$out/test"

sed -e 's|"service_worker": "background.js"|"scripts": ["background.js"]|' \
    -e '/^  "key":/d' \
    -e '/^  "minimum_chrome_version":/d' \
    -e 's|"webRequest",|"webRequest", "webRequestBlocking",|' \
    "$src/manifest.json" >"$out/manifest.json"

grep -q '"scripts": \["background.js"\]' "$out/manifest.json" && grep -q '"webRequestBlocking"' "$out/manifest.json" || {
    echo "build.sh: background entry or webRequest permission not found in extension/manifest.json" >&2
    exit 1
}
echo "Firefox extension: $out"
