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
// Firefox and its derivatives update the installed package from the latest release: updates.json.
if (!/^https:\/\/.+\/updates\.json$/.test(firefox.browser_specific_settings.gecko.update_url ?? "")) fail("Firefox manifest: https update_url missing");
if ("key" in firefox || "minimum_chrome_version" in firefox) fail("Firefox manifest: Chrome-only keys left");
// Chrome MV3 refuses `webRequestBlocking`; Firefox needs it to take downloads over before they start.
if (!firefox.permissions.includes("webRequestBlocking") || chrome.permissions.includes("webRequestBlocking")) fail("webRequestBlocking: Firefox only");
// Apart from the background entry, the Chrome-only keys and that permission, both builds are identical.
const strip = ({ background, key, minimum_chrome_version, permissions, ...rest }) =>
  JSON.stringify({ ...rest, permissions: permissions.filter((p) => p !== "webRequestBlocking") });
if (strip(chrome) !== strip(firefox)) fail("Chrome and Firefox manifests diverge");
for (const f of fs.readdirSync(".").filter((f) => f !== "test")) {
  if (f !== "manifest.json" && !fs.existsSync(process.env.FIREFOX + "/" + f)) fail("Firefox build lacks " + f);
}
console.log("json   OK  manifest.json (Chrome) + Firefox build")'
# Translations: the same messages in every language, and every message the scripts ask for exists.
node -e '
const fs = require("fs");
const fail = (m) => { throw new Error(m); };
const locales = fs.readdirSync("_locales");
const keys = (l) => Object.keys(JSON.parse(fs.readFileSync(`_locales/${l}/messages.json`, "utf8")));
const en = keys("en");
for (const l of locales) {
  const k = keys(l);
  const missing = en.filter((x) => !k.includes(x)).concat(k.filter((x) => !en.includes(x)));
  if (missing.length) fail(`_locales/${l}: ${missing.join(", ")} differ from en`);
}
const manifest = JSON.parse(fs.readFileSync("manifest.json", "utf8"));
for (const [, key] of JSON.stringify(manifest).matchAll(/__MSG_(\w+)__/g)) if (!en.includes(key)) fail(`manifest: ${key} undefined`);
for (const f of ["background.js", "content.js"]) {
  for (const [, key] of fs.readFileSync(f, "utf8").matchAll(/\bt\("(\w+)"/g)) if (!en.includes(key)) fail(`${f}: message ${key} undefined`);
}
console.log(`i18n   OK  ${locales.join(" + ")} (${en.length} messages)`)'
node --test test/*.test.mjs
