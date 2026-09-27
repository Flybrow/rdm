#!/bin/sh
# Static checks + unit tests for the extension: sh extension/test/check.sh (needs Node ≥ 20).
set -eu
cd "$(dirname "$0")/.."
for f in background.js shared.js; do node --check --input-type=module < "$f" && echo "syntax OK  $f"; done
for f in content.js youtube.js capture.js relay.js; do node --check "$f" && echo "syntax OK  $f"; done

# Chrome manifest as is, Firefox manifest as derived by packaging/firefox/build.sh, and the
# Chromium package (packaging/chromium/build.sh) identical to the source.
firefox=$(mktemp -d)
chromium=$(mktemp -d)
trap 'rm -rf "$firefox" "$chromium"' EXIT
sh ../packaging/firefox/build.sh "$firefox" >/dev/null
sh ../packaging/chromium/build.sh "$chromium/ext" >/dev/null
[ ! -e "$chromium/ext/test" ] && cmp -s manifest.json "$chromium/ext/manifest.json" || { echo "Chromium build differs from the source" >&2; exit 1; }
FIREFOX="$firefox" node -e '
const fs = require("fs");
const read = (p) => JSON.parse(fs.readFileSync(p, "utf8"));
const chrome = read("manifest.json");
const firefox = read(process.env.FIREFOX + "/manifest.json");
const fail = (m) => { throw new Error(m); };
// Chrome flags `background.scripts` in MV3 as an error: service worker only.
if (!chrome.background.service_worker || chrome.background.scripts) fail("Chrome manifest: service_worker only");
if (!firefox.background.scripts?.includes("background.js") || firefox.background.service_worker) fail("Firefox manifest: scripts only");
if (!firefox.browser_specific_settings?.gecko?.id) fail("Firefox manifest: gecko id missing");
if ("key" in firefox || "minimum_chrome_version" in firefox) fail("Firefox manifest: Chrome-only keys left");
// Apart from the background entry and the Chrome-only keys, both builds are identical.
const strip = ({ background, key, minimum_chrome_version, ...rest }) => JSON.stringify(rest);
if (strip(chrome) !== strip(firefox)) fail("Chrome and Firefox manifests diverge");
for (const f of fs.readdirSync(".").filter((f) => f !== "test")) {
  if (f !== "manifest.json" && !fs.existsSync(process.env.FIREFOX + "/" + f)) fail("Firefox build lacks " + f);
}
console.log("json   OK  manifest.json (Chrome) + Firefox build")'
node --test test/*.test.mjs
