import { isCapturable, isHls, isMediaType, isMediaUrl, siteOf } from "./shared.js";

// Chrome/Brave (`chrome`) and Firefox (`browser`): same promise-based API.
const ext = globalThis.browser ?? globalThis.chrome;

const BRIDGE = "http://127.0.0.1:9614";
const MIN_MEDIA_BYTES = 256 * 1024; // ignore stream fragments and previews
const MAX_ITEMS_PER_TAB = 60; // bounds session storage on pages that stream many unique URLs

// Service workers sleep: detected media lives in session storage (memory only), keyed per tab.
const tabKey = (tabId) => `tab:${tabId}`;
const media = {
  get: async (tabId) => (await ext.storage.session.get(tabKey(tabId)))[tabKey(tabId)] ?? {},
  set: (tabId, items) => ext.storage.session.set({ [tabKey(tabId)]: items }),
  clear: (tabId) => ext.storage.session.remove(tabKey(tabId)),
};

// Read-modify-write on storage must not interleave between concurrent responses (nor with a clear).
let queue = Promise.resolve();
const serially = (task) => (queue = queue.then(task).catch(() => {}));

// ── Bridge ────────────────────────────────────────────────────────────────
const isWeb = (url) => /^https?:\/\//i.test(url ?? "");

async function call(path, { method = "GET", body, timeout = 1500 } = {}) {
  try {
    return await fetch(`${BRIDGE}${path}`, {
      method,
      body: body && JSON.stringify(body),
      headers: body ? { "content-type": "application/json" } : undefined,
      signal: AbortSignal.timeout(timeout),
    });
  } catch {
    return null;
  }
}

/** The app's settings (capture list) — `null` when RDM is not running. */
async function appConfig() {
  const res = await call("/config", { timeout: 600 });
  return res?.ok ? res.json().catch(() => null) : null;
}

/** Cookie store of a tab: private windows (and Firefox containers) have their own. */
async function storeOf(tab) {
  if (tab?.cookieStoreId) return tab.cookieStoreId; // Firefox
  if (!tab?.incognito) return undefined; // Chrome: the default store
  const stores = await ext.cookies.getAllCookieStores().catch(() => []);
  return stores.find((s) => s.tabIds.includes(tab.id))?.id;
}

/**
 * Cookies for `url`, as the browser would send them from `frameUrl` inside `topUrl`: `SameSite=Strict`
 * ones only when the whole chain is same-site. Taken from the page's own store, never another profile's.
 */
async function cookieHeader(url, { frameUrl, topUrl = frameUrl, storeId } = {}) {
  const site = siteOf(url);
  const crossSite = !frameUrl || siteOf(frameUrl) !== site || siteOf(topUrl) !== site;
  const cookies = await ext.cookies.getAll(storeId ? { url, storeId } : { url }).catch(() => []);
  return cookies
    .filter((c) => !(crossSite && c.sameSite === "strict"))
    .map((c) => `${c.name}=${c.value}`)
    .join("; ");
}

// A client User-Agent handed by the page (YouTube links are bound to the app that asked for them).
const validAgent = (ua) => typeof ua === "string" && ua.length > 0 && ua.length <= 300 && /^[\x20-\x7e]+$/.test(ua);

/**
 * Request context for RDM. `bare`: links that need neither cookies nor referer (YouTube's
 * stream servers) get none — nothing about the browsing session leaves the browser.
 */
async function context({ url, referrer, topUrl, storeId, userAgent, bare = false }) {
  return {
    url,
    referrer: bare ? undefined : referrer,
    cookies: bare ? undefined : await cookieHeader(url, { frameUrl: referrer, topUrl, storeId }),
    user_agent: validAgent(userAgent) ? userAgent : navigator.userAgent,
  };
}

async function sendToApp(request) {
  const { url, audio_url, filename } = request;
  if (!isWeb(url) || (audio_url && !isWeb(audio_url))) return false;
  const body = { ...(await context(request)), audio_url, filename };
  const res = await call("/add", { method: "POST", body, timeout: 5000 });
  return res?.status === 202;
}

/** HLS qualities, resolved by RDM (no CORS limits there). */
async function probe(request) {
  if (!isWeb(request.url)) return null;
  const res = await call("/probe", { method: "POST", body: await context(request), timeout: 25_000 });
  return res?.ok ? res.json().catch(() => null) : null;
}

/** Whether RDM can really fetch this link with these headers (`{ ok, status, size }`). */
async function check(request) {
  if (!isWeb(request.url)) return null;
  const res = await call("/check", { method: "POST", body: await context(request), timeout: 15_000 });
  return res?.ok ? res.json().catch(() => null) : null;
}

/** Opens a recording session in RDM for this YouTube page; returns its token (or null). */
async function recordStart(page, filename) {
  const youtube = /^https:\/\/(www|m)\.youtube\.com\/watch\?/.test(page ?? "");
  if (!youtube || typeof filename !== "string" || !filename.trim()) return null;
  const res = await call("/record/start", { method: "POST", body: { page, filename }, timeout: 5000 });
  const data = res?.ok ? await res.json().catch(() => null) : null;
  return typeof data?.token === "string" ? data.token : null;
}

// ── Download interception ────────────────────────────────────────────────
async function intercept(item) {
  const url = item.finalUrl || item.url;
  // Private windows stay private (RDM keeps a history); other extensions' downloads are theirs.
  if (!isWeb(url) || item.incognito || item.byExtensionId) return;
  const filename = (item.filename ?? "").split(/[\\/]/).pop() || undefined;
  const config = await appConfig();
  if (!config || !(isCapturable(config.captured, filename ?? "") || isCapturable(config.captured, url))) return;
  // Pause first, cancel only once RDM accepted: a refused hand-off never loses the download. A
  // download that can no longer be paused (a small file already finished) stays the browser's:
  // handing it over too would fetch the same file twice.
  if (!(await ext.downloads.pause(item.id).then(() => true, () => false))) return;
  const accepted = await sendToApp({ url, filename, referrer: item.referrer, storeId: item.cookieStoreId });
  if (accepted) {
    await ext.downloads.cancel(item.id).catch(() => {});
    await ext.downloads.erase({ id: item.id }).catch(() => {});
  } else {
    await ext.downloads.resume(item.id).catch(() => {});
  }
}

if (ext.downloads.onDeterminingFilename) {
  ext.downloads.onDeterminingFilename.addListener((item, suggest) => {
    suggest();
    intercept(item);
  });
} else {
  // Firefox has no onDeterminingFilename: the download is created with its final name already.
  ext.downloads.onCreated.addListener(intercept);
}

// ── Network media sniffing ───────────────────────────────────────────────
const remember = (tabId, key, item) =>
  serially(async () => {
    const items = await media.get(tabId);
    if (items[key]?.url === item.url) return;
    delete items[key];
    items[key] = item; // newest URL wins (stream URLs expire) and moves to the end
    const keys = Object.keys(items);
    keys.slice(0, Math.max(0, keys.length - MAX_ITEMS_PER_TAB)).forEach((k) => delete items[k]);
    await media.set(tabId, items);
    ext.tabs.sendMessage(tabId, { kind: "media-changed" }).catch(() => {});
  });

const forget = (tabId) => serially(() => media.clear(tabId));

const header = (headers, name) => headers?.find((h) => h.name.toLowerCase() === name)?.value;
// YouTube streams are chunks of a proprietary protocol: resolved in-page instead (youtube.js).
const isYouTubeStream = (url) => /^https:\/\/[^/?#]+\.googlevideo\.com[:/]/i.test(url);

ext.webRequest.onHeadersReceived.addListener(
  ({ tabId, url, statusCode, responseHeaders }) => {
    if (tabId < 0 || statusCode >= 400) return;
    // Cheapest tests first: this runs for every XHR of every tab.
    const type = header(responseHeaders, "content-type") ?? "";
    const hls = isHls(url, type);
    if (!hls && !isMediaType(type) && !isMediaUrl(url)) return;
    if (isYouTubeStream(url)) return;
    const range = header(responseHeaders, "content-range");
    const size = hls ? 0 : Number(range?.split("/")[1] ?? header(responseHeaders, "content-length") ?? 0);
    if (size && size < MIN_MEDIA_BYTES) return;

    const key = url.replace(/([?&])(range|rn|rbuf|bytestart|byteend)=[^&]*/g, "$1");
    remember(tabId, key, { url, size, type: hls ? "HLS" : type.split(";")[0], hls });
  },
  { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other"] },
  ["responseHeaders"],
);

ext.tabs.onRemoved.addListener(forget);
ext.tabs.onUpdated.addListener((tabId, { url }) => url && forget(tabId));

// ── Content-script API ───────────────────────────────────────────────────
ext.runtime.onMessage.addListener((msg, sender, reply) => {
  if (sender.id !== ext.runtime.id) return;
  const tab = sender.tab;
  // The frame is what the browser would send as Referer (embedded players live in iframes).
  const base = { referrer: sender.url ?? tab?.url, topUrl: tab?.url, userAgent: msg?.user_agent, bare: msg?.bare === true };
  const withStore = async (request) => ({ ...request, storeId: await storeOf(tab) });
  switch (msg?.kind) {
    case "list":
      media.get(tab?.id).then((items) => reply(Object.values(items)), () => reply([]));
      return true;
    case "download":
      withStore({ ...base, url: msg.url, audio_url: msg.audio_url, filename: msg.filename })
        .then(sendToApp)
        .then(reply, () => reply(false));
      return true;
    case "probe":
      withStore({ ...base, url: msg.url }).then(probe).then(reply, () => reply(null));
      return true;
    case "check":
      withStore({ ...base, url: msg.url }).then(check).then(reply, () => reply(null));
      return true;
    case "record-start":
      recordStart(tab?.url, msg.filename).then(reply, () => reply(null));
      return true;
  }
});

// ── Toolbar button & context menu ────────────────────────────────────────
ext.action.onClicked.addListener(() => call("/show", { method: "POST" }));

ext.runtime.onInstalled.addListener(() =>
  ext.contextMenus
    .removeAll()
    .then(() => ext.contextMenus.create({ id: "rdm", title: "Télécharger avec RDM", contexts: ["link", "video", "audio"] })),
);

ext.contextMenus.onClicked.addListener(async ({ linkUrl, srcUrl, pageUrl, frameUrl }, tab) => {
  const url = linkUrl || srcUrl;
  if (!isWeb(url)) return;
  sendToApp({ url, referrer: frameUrl || pageUrl, topUrl: pageUrl, storeId: await storeOf(tab) });
});
