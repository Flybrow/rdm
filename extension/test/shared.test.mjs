// node --test extension/test — mirrors the Rust tests of `domain::file_kind` (same inputs, same answers).
import assert from "node:assert/strict";
import { test } from "node:test";

import { browserKey, dispositionName, downloadName, isCapturable, isHls, isMediaType, isMediaUrl, siteOf } from "../shared.js";

const LIST = "zip mp4 webm iso 7z";

test("captures listed and split-archive extensions", () => {
  assert.ok(isCapturable(LIST, "https://x.io/a/Video.MP4?token=1"));
  assert.ok(isCapturable(LIST, "archive.r01"));
  assert.ok(isCapturable(LIST, "archive.r1"));
  assert.ok(isCapturable(LIST, "backup.7z.001"));
  assert.ok(isCapturable(LIST, "film.webm"));
  assert.ok(!isCapturable(LIST, "page.html"));
  assert.ok(!isCapturable(LIST, "noext"));
  assert.ok(!isCapturable(LIST, "https://x.io/dir.zip/page"));
  assert.ok(!isCapturable(LIST, "archive.rar5x"));
});

test("'#' and '?' are ordinary characters in file names (regression)", () => {
  assert.ok(isCapturable(LIST, "Chine–USA _ terres rares #octogone93 (1080p).mp4"));
  assert.ok(isCapturable(LIST, "Clip #42.zip"));
  assert.ok(isCapturable(LIST, "https://x.io/a.zip#section"));
  assert.ok(!isCapturable(LIST, "https://x.io/page#a.zip"));
});

test("user list syntax is forgiving", () => {
  assert.ok(isCapturable(".iso, .ZIP", "a.zip"));
  assert.ok(!isCapturable("iso", "a.zip"));
  assert.ok(!isCapturable("", "a.zip"));
  assert.ok(isCapturable("iso\nzip\tmkv", "a.zip"), "one per line, as typed in RDM's settings");
  assert.ok(isCapturable(LIST, ".zip"), "a name that is only an extension, as in Rust");
});

test("media and HLS detection", () => {
  assert.ok(isHls("https://cdn.io/v/master.m3u8?x=1"));
  assert.ok(isHls("https://cdn.io/v/playlist", "application/vnd.apple.mpegurl"));
  assert.ok(isMediaUrl("https://cdn.io/a.mp4"));
  assert.ok(!isMediaUrl("not a url"));
  assert.ok(isMediaType("video/mp4"));
  assert.ok(!isMediaType("video/mp2t"));
});

test("site comparison for SameSite cookies", () => {
  assert.equal(siteOf("https://www.youtube.com/watch"), "youtube.com");
  assert.equal(siteOf("https://rr3---sn.googlevideo.com/x"), "googlevideo.com");
  assert.equal(siteOf("garbage"), "");
  // Same cases as `sites_as_cookies_see_them` in RDM's `engine/src/net.rs`.
  assert.equal(siteOf("https://www.bbc.co.uk/"), siteOf("https://media.bbc.co.uk/x"));
  assert.notEqual(siteOf("https://www.bbc.co.uk/"), siteOf("https://evil.co.uk/"), "a country's second level is not a site");
  assert.equal(siteOf("https://example.com/"), siteOf("https://EXAMPLE.com.:8443/x"));
  assert.notEqual(siteOf("https://example.com/"), siteOf("https://example.com.evil.io/"));
});

test("file name of a Content-Disposition header", () => {
  assert.equal(dispositionName('attachment; filename="a b.zip"'), "a b.zip");
  assert.equal(dispositionName("attachment; filename=setup.exe"), "setup.exe");
  assert.equal(dispositionName("attachment; filename=\"x.zip\"; filename*=UTF-8''%C3%A9t%C3%A9.zip"), "été.zip");
  assert.equal(dispositionName("attachment"), "");
});

test("a top-level response is a download when attached or not showable", () => {
  const url = "https://x.io/files/7z2409-x64.exe?sig=1";
  assert.equal(downloadName(url, "", "application/octet-stream"), "7z2409-x64.exe");
  assert.equal(downloadName(url, 'attachment; filename="other.exe"', "application/octet-stream"), "other.exe");
  assert.equal(downloadName("https://x.io/doc.pdf", "", "application/pdf"), "", "shown by the PDF viewer");
  assert.equal(downloadName("https://x.io/doc.pdf", "attachment", "application/pdf"), "doc.pdf");
  assert.equal(downloadName("https://x.io/film.mp4", "", "video/mp4"), "", "played in the tab");
  assert.equal(downloadName("https://x.io/", "", "text/html; charset=utf-8"), "");
  assert.equal(downloadName("https://x.io/a%20b.zip", "", "application/zip"), "a b.zip");
});

test("browser keys, as RDM computes them (extension::key_of)", () => {
  const cases = [
    ["Google Chrome", "chrome"],
    ["Microsoft Edge", "edge"],
    ["Mozilla Firefox", "firefox"],
    ["Firefox", "firefox"],
    ["Waterfox", "waterfox"],
    ["Mullvad Browser", "mullvad"],
    ["Zen Browser", "zen"],
    ["Opera Stable", "opera"],
    ["Brave", "brave"],
    ["Thorium", "thorium"],
    ["Supercalifragilisticexpialidocious", "supercalifragili"],
    ["", ""],
  ];
  for (const [name, key] of cases) assert.equal(browserKey(name), key, name);
});
