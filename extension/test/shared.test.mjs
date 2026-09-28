// node --test extension/test — mirrors the Rust tests of `domain::file_kind` (same inputs, same answers).
import assert from "node:assert/strict";
import { test } from "node:test";

import { isCapturable, isHls, isMediaType, isMediaUrl, siteOf } from "../shared.js";

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
});
