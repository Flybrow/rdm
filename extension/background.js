import { isCapturable, isHls, isMediaType, isMediaUrl, siteOf } from "./shared.js";

// Chrome, Brave, Opera, Edge (`chrome`) and Firefox (`browser`): same promise-based API.
const ext = globalThis.browser ?? globalThis.chrome;
/** The browser's language (English by default, see `_locales`). */
const t = (key, ...subs) => ext.i18n.getMessage(key, subs.map(String)) || key;

const BRIDGE = "http://127.0.0.1:9614";
/** RDM's connector, started by the browser itself (see RDM's `native.rs`). */
const NATIVE_HOST = "rdm.bridge";
/** An idle connection to the connector is closed: the browser (and the connector) can rest. */
const NATIVE_IDLE_MS = 60_000;
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
const BROWSER = (async () => {
  const info = globalThis.browser?.runtime?.getBrowserInfo;
  if (typeof info === "function") {
    // Firefox and its derivatives (Waterfox reports its own name here, not in its User-Agent).
    const { name = "" } = await info().catch(() => ({}));
    return /waterfox/i.test(name) ? "waterfox" : "firefox";
  }
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

/**
 * The connector: one connection, opened on demand, closed when idle. Requests carry an id; the
 * connector answers `{ id, status, body }` (status 0: RDM not running, 1: no answer in time,
 * 2: refused). `null` when the connector is not available (not registered, sandboxed browser):
 * the bridge is then called directly.
 */
const native = {
  port: null,
  nextId: 1,
  pending: new Map(),
  idleTimer: 0,
  /** Unavailable until this time (ms): not retried on every request. */
  downUntil: 0,

  connect() {
    if (this.port) return this.port;
    if (Date.now() < this.downUntil || typeof ext.runtime.connectNative !== "function") return null;
    let port;
    try {
      port = ext.runtime.connectNative(NATIVE_HOST);
    } catch {
      this.downUntil = Date.now() + 5 * 60_000;
      return null;
    }
    let answered = false;
    port.onMessage.addListener((msg) => {
      answered = true;
      const resolve = this.pending.get(msg?.id);
      if (resolve) {
        this.pending.delete(msg.id);
        resolve(msg);
      }
      this.touch();
    });
    port.onDisconnect.addListener(() => {
      void ext.runtime.lastError; // read: an absent connector is expected, not an error to log
      if (this.port === port) this.port = null;
      // Never answered: the connector is missing or cannot start here; the bridge is used instead.
      if (!answered) this.downUntil = Date.now() + 5 * 60_000;
      for (const resolve of this.pending.values()) resolve(null);
      this.pending.clear();
    });
    this.port = port;
    return port;
  },

  touch() {
    clearTimeout(this.idleTimer);
    this.idleTimer = setTimeout(() => {
      if (this.pending.size) return this.touch();
      this.port?.disconnect();
      this.port = null;
    }, NATIVE_IDLE_MS);
  },

  /** The connector's answer, or `null` when it is unavailable. */
  request(message, timeout) {
    const port = this.connect();
    if (!port) return Promise.resolve(null);
    const id = this.nextId++;
    this.touch();
    // The connector bounds each request itself; starting RDM (explicit actions) takes longer.
    const slack = STARTS_RDM.has(message.path) ? 25_000 : 3_000;
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        resolve({ id, status: 1, body: "" });
      }, timeout + slack);
      this.pending.set(id, (msg) => {
        clearTimeout(timer);
        resolve(msg);
      });
      try {
        port.postMessage({ ...message, id, timeout });
      } catch {
        clearTimeout(timer);
        this.pending.delete(id);
        resolve(null);
      }
    });
  },
};

/** Requests on which the connector starts RDM if it is not running (the user asked for them). */
const STARTS_RDM = new Set(["/add", "/show", "/record/start"]);
const NULL_BODY = new Set([204, 205, 304]);

/** A response, `TIMEOUT`, or `null` when RDM is not running. */
async function call(path, { method = "GET", body, timeout = 3000 } = {}) {
  const reply = await native.request({ method, path, body, browser: await BROWSER }, timeout);
  if (reply) {
    if (reply.status === 0) return null;
    if (reply.status === 1) return TIMEOUT;
    if (reply.status >= 200 && reply.status <= 599) {
      setPaired(true); // the connector needs no approval
      return new Response(NULL_BODY.has(reply.status) ? null : reply.body, { status: reply.status });
    }
    // Refused by the connector (an older RDM): the bridge itself.
  }
  let res;
  try {
    const headers = { "x-rdm-browser": await BROWSER };
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
const UNPAIRED_TEXT = t("unpaired");
function setPaired(ok) {
  // No in-memory "already set" shortcut: the badge outlives a sleeping service worker's state.
  ext.action.setBadgeText({ text: ok ? "" : "!" }).catch(() => {});
  if (!ok) ext.action.setBadgeBackgroundColor({ color: "#d97706" }).catch(() => {});
  ext.action.setTitle({ title: ok ? "RDM" : `RDM — ${UNPAIRED_TEXT}` }).catch(() => {});
}

/**
 * Firefox: the connector vouches for this extension, so RDM accepts its random origin (the
 * recording relay calls the bridge directly) without asking the user.
 */
async function pair() {
  if (!globalThis.browser?.runtime?.getBrowserInfo) return;
  const origin = new URL(ext.runtime.getURL("")).origin;
  await native.request({ method: "POST", path: "/pair", body: { origin }, browser: await BROWSER }, 3000);
}

/** Tells RDM this browser is here (and asks for approval right away on Firefox without connector). */
async function checkIn() {
  await pair();
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
    toast(t("sentName", filename ?? nameOf(url)));
  } else {
    await ext.downloads.resume(item.id).catch(() => {});
    if (sent === "unpaired") return toast(`✗ ${UNPAIRED_TEXT}`);
    // RDM closed since its last answer: say it once, then leave downloads alone until it is back.
    await forgetConfig();
    toast(t("noAnswer"));
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
  else if (!answered(res)) toast(t("notRunning"), tab?.id);
});

ext.contextMenus.onClicked.addListener(async ({ linkUrl, srcUrl, pageUrl, frameUrl }, tab) => {
  const url = linkUrl || srcUrl;
  if (!isWeb(url)) return toast(t("notWeb"), tab?.id);
  const sent = await sendToApp({ url, referrer: frameUrl || pageUrl, topUrl: pageUrl, storeId: await storeOf(tab) });
  const text = sent === true ? t("sentName", nameOf(url)) : sent === "unpaired" ? `✗ ${UNPAIRED_TEXT}` : t("notRunning");
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
    .then(() => ext.contextMenus.create({ id: "rdm", title: t("menuDownload"), contexts: ["link", "video", "audio"] }));
});
ext.runtime.onStartup.addListener(start);
ext.alarms.onAlarm.addListener(({ name }) => name === "rdm-check-in" && checkIn());
