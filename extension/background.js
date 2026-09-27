import { isCapturable, isHls, isMediaType, isMediaUrl, siteOf } from "./shared.js";

// Chrome, Brave, Opera, Edge (`chrome`) and Firefox (`browser`): same promise-based API.
const ext = globalThis.browser ?? globalThis.chrome;

const BRIDGE = "http://127.0.0.1:9614";
const MIN_MEDIA_BYTES = 256 * 1024; // ignore stream fragments and previews
const MAX_ITEMS_PER_TAB = 60; // bounds session storage on pages that stream many unique URLs
/** Checks in with RDM this often while the browser runs (RDM shows the extension as connected). */
const HEARTBEAT_MINUTES = 5;

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

/** Which browser this is, for RDM's extension window (`x-rdm-browser`). */
const BROWSER = (() => {
  if (typeof globalThis.browser?.runtime?.getBrowserInfo === "function") return "firefox";
  const brands = (navigator.userAgentData?.brands ?? []).map((b) => b.brand);
  const ua = navigator.userAgent;
  if (navigator.brave || brands.some((b) => /brave/i.test(b))) return "brave";
  if (brands.some((b) => /opera/i.test(b)) || /\bOPR\//.test(ua)) return "opera";
  if (brands.some((b) => /edge/i.test(b)) || /\bEdg\//.test(ua)) return "edge";
  if (brands.includes("Google Chrome")) return "chrome";
  if (brands.includes("Chromium")) return "chromium";
  return "chrome";
})();

// ── Bridge ────────────────────────────────────────────────────────────────
const isWeb = (url) => /^https?:\/\//i.test(url ?? "");

/** RDM is running but did not answer in time (busy disk, starting up): not the same as absent. */
const TIMEOUT = Symbol("timeout");

/** A response, `TIMEOUT`, or `null` when RDM is not running. */
async function call(path, { method = "GET", body, timeout = 3000 } = {}) {
  let res;
  try {
    const headers = { "x-rdm-browser": BROWSER };
    if (body) headers["content-type"] = "application/json";
    res = await fetch(`${BRIDGE}${path}`, {
      method,
      body: body && JSON.stringify(body),
      headers,
      signal: AbortSignal.timeout(timeout),
    });
  } catch (e) {
    return e?.name === "TimeoutError" ? TIMEOUT : null;
  }
  // GETs carry no `Origin` in Firefox: only a POST tells whether RDM approved this extension.
  if (method === "POST") setPaired(res.status !== UNPAIRED);
  return res;
}

const answered = (res) => res && res !== TIMEOUT;

/**
 * Firefox gives each install a random origin, which RDM only accepts once the user approved it in
 * RDM's window (401 until then). The toolbar badge says so instead of silently failing.
 */
const UNPAIRED = 401;
const UNPAIRED_TEXT = "Autorisez l'extension dans la fenêtre de RDM, puis réessayez";
function setPaired(ok) {
  // No in-memory "already set" shortcut: the badge outlives a sleeping service worker's state.
  ext.action.setBadgeText({ text: ok ? "" : "!" }).catch(() => {});
  if (!ok) ext.action.setBadgeBackgroundColor({ color: "#d97706" }).catch(() => {});
  ext.action.setTitle({ title: ok ? "RDM" : `RDM — ${UNPAIRED_TEXT.toLowerCase()}` }).catch(() => {});
}

/** Tells RDM this browser is here (and asks for approval right away on Firefox). */
async function checkIn() {
  await call("/ping", { method: "POST" });
  await appConfig(); // refreshes the cached capture list
}

/** Outcome of a hand-off: true (accepted), "unpaired" (awaiting approval in RDM) or false. */
const outcome = (res, accepted) => (res?.status === accepted ? true : res?.status === UNPAIRED ? "unpaired" : false);

const CONFIG = "config";
const cachedConfig = async () => (await ext.storage.session.get(CONFIG).catch(() => ({})))[CONFIG] ?? null;
/** RDM is gone: until it answers again, downloads are left to the browser without a pause. */
const forgetConfig = () => ext.storage.session.remove(CONFIG).catch(() => {});

/**
 * The app's settings (capture list) — `null` when RDM is not running. When RDM is running but slow
 * to answer, its last known list still applies: a busy moment must not let downloads slip past.
 */
async function appConfig() {
  const res = await call("/config");
  if (answered(res) && res.ok) {
    const config = await res.json().catch(() => null);
    if (typeof config?.captured === "string") {
      await ext.storage.session.set({ [CONFIG]: config }).catch(() => {});
      return config;
    }
  }
  if (res === TIMEOUT) return cachedConfig();
  if (!res) await forgetConfig();
  return null;
}

/**
 * The capture list, without delay when RDM answered recently: a fast download (small file, fibre)
 * finishes within one round trip to RDM, and a finished download can no longer be handed over.
 * The cached list is refreshed in the background (the check-ins keep it current anyway).
 */
async function captureList() {
  const cached = await cachedConfig();
  if (typeof cached?.captured === "string") {
    appConfig();
    return cached.captured;
  }
  return (await appConfig())?.captured ?? null;
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

/** true (accepted), "unpaired" (awaiting approval in RDM) or false (RDM absent or refusing). */
async function sendToApp(request) {
  const { url, audio_url, filename } = request;
  if (!isWeb(url) || (audio_url && !isWeb(audio_url))) return false;
  const body = { ...(await context(request)), audio_url, filename };
  // RDM answers /add at once (the file is inspected afterwards): a long wait means trouble, but a
  // busy machine deserves a few seconds.
  const res = await call("/add", { method: "POST", body, timeout: 8000 });
  return outcome(res, 202);
}

/** HLS qualities, resolved by RDM (no CORS limits there). */
async function probe(request) {
  if (!isWeb(request.url)) return null;
  const res = await call("/probe", { method: "POST", body: await context(request), timeout: 25_000 });
  return answered(res) && res.ok ? res.json().catch(() => null) : null;
}

/** Whether RDM can really fetch this link with these headers (`{ ok, status, size }`). */
async function check(request) {
  if (!isWeb(request.url)) return null;
  const res = await call("/check", { method: "POST", body: await context(request), timeout: 15_000 });
  return answered(res) && res.ok ? res.json().catch(() => null) : null;
}

/** Opens a recording session in RDM for this YouTube page; returns its token (or null). */
async function recordStart(page, filename) {
  const youtube = /^https:\/\/(www|m)\.youtube\.com\/watch\?/.test(page ?? "");
  if (!youtube || typeof filename !== "string" || !filename.trim()) return null;
  const res = await call("/record/start", { method: "POST", body: { page, filename }, timeout: 8000 });
  if (res?.status === UNPAIRED) return "unpaired";
  const data = answered(res) && res.ok ? await res.json().catch(() => null) : null;
  return typeof data?.token === "string" ? data.token : null;
}

// ── Feedback in the page ─────────────────────────────────────────────────
/** A short message in the page (content.js draws it); `tabId` defaults to the tab in front. */
async function toast(text, tabId) {
  const id = tabId ?? (await ext.tabs.query({ active: true, lastFocusedWindow: true }).catch(() => []))[0]?.id;
  if (id == null || id < 0) return;
  // Pages without content scripts (browser pages, the PDF viewer) just stay silent.
  ext.tabs.sendMessage(id, { kind: "toast", text }, { frameId: 0 }).catch(() => {});
}

const nameOf = (url) => {
  try {
    return decodeURIComponent(new URL(url).pathname.split("/").pop() || "") || new URL(url).hostname;
  } catch {
    return url;
  }
};

// ── Download interception ────────────────────────────────────────────────
async function intercept(item) {
  const url = item.finalUrl || item.url;
  // Private windows stay private (RDM keeps a history); other extensions' downloads are theirs.
  if (!isWeb(url) || item.incognito || item.byExtensionId) return;
  const filename = (item.filename ?? "").split(/[\\/]/).pop() || undefined;
  const captured = await captureList();
  if (captured == null || !(isCapturable(captured, filename ?? "") || isCapturable(captured, url))) return;
  // Pause first, cancel only once RDM accepted: a refused hand-off never loses the download. A
  // download that can no longer be paused (a small file already finished) stays the browser's:
  // handing it over too would fetch the same file twice.
  if (!(await ext.downloads.pause(item.id).then(() => true, () => false))) return;
  const sent = await sendToApp({ url, filename, referrer: item.referrer, storeId: item.cookieStoreId });
  if (sent === true) {
    await ext.downloads.cancel(item.id).catch(() => {});
    await ext.downloads.erase({ id: item.id }).catch(() => {});
    toast(`✓ Envoyé à RDM : ${filename ?? nameOf(url)}`);
  } else {
    await ext.downloads.resume(item.id).catch(() => {});
    if (sent === "unpaired") return toast(`✗ ${UNPAIRED_TEXT}`);
    // RDM closed since its last answer: say it once, then leave downloads alone until it is back.
    await forgetConfig();
    toast("✗ RDM n'a pas répondu : le navigateur garde ce téléchargement");
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

// ── Toolbar button, context menu, check-ins ──────────────────────────────
ext.action.onClicked.addListener(async (tab) => {
  const res = await call("/show", { method: "POST" });
  if (res?.status === UNPAIRED) toast(`✗ ${UNPAIRED_TEXT}`, tab?.id);
  else if (!answered(res)) toast("✗ RDM n'est pas lancé : ouvrez RDM, puis réessayez", tab?.id);
});

ext.contextMenus.onClicked.addListener(async ({ linkUrl, srcUrl, pageUrl, frameUrl }, tab) => {
  const url = linkUrl || srcUrl;
  if (!isWeb(url)) return toast("✗ Ce lien ne se télécharge pas hors de la page (adresse non web)", tab?.id);
  const sent = await sendToApp({ url, referrer: frameUrl || pageUrl, topUrl: pageUrl, storeId: await storeOf(tab) });
  const text = sent === true ? `✓ Envoyé à RDM : ${nameOf(url)}` : sent === "unpaired" ? `✗ ${UNPAIRED_TEXT}` : "✗ RDM n'est pas lancé : ouvrez RDM, puis réessayez";
  toast(text, tab?.id);
});

function start() {
  ext.alarms.create("rdm-check-in", { periodInMinutes: HEARTBEAT_MINUTES });
  checkIn();
}

ext.runtime.onInstalled.addListener(() => {
  start();
  ext.contextMenus
    .removeAll()
    .then(() => ext.contextMenus.create({ id: "rdm", title: "Télécharger avec RDM", contexts: ["link", "video", "audio"] }));
});
ext.runtime.onStartup.addListener(start);
ext.alarms.onAlarm.addListener(({ name }) => name === "rdm-check-in" && checkIn());
