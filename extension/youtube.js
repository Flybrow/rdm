// Runs in YouTube's own page world (manifest "world": "MAIN") so it can use the page session —
// cookies, visitor data, signed-in account — to ask YouTube's player API for real stream URLs.
// It answers only `window.postMessage` requests and talks to nobody but youtube.com itself.
(() => {
  if (window.__rdmYouTube) return;
  window.__rdmYouTube = true;

  const CACHE_MS = 5 * 60_000; // stream URLs stay valid for hours; keep lookups cheap
  const REQUEST_TIMEOUT_MS = 8_000;
  const SAFARI_UA =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15";
  const VR_UA =
    "com.google.android.apps.youtube.vr.oculus/1.62.27 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip";
  const IOS_UA = "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)";
  const VR = { deviceMake: "Oculus", deviceModel: "Quest 3", androidSdkVersion: 32, osName: "Android", osVersion: "12L", userAgent: VR_UA };

  // Every client is asked; RDM then verifies which answers really download (the stream servers
  // bind links to the client's User-Agent, and some clients' links also need a token we lack).
  // `ua` is the User-Agent the download must present.
  const CLIENTS = [
    // Direct links, no proof-of-origin token. Anonymous first, then with the visitor cookies,
    // which can be enough to pass the anti-bot check on a flagged network (VPN).
    { name: "ANDROID_VR", id: 28, version: "1.62.27", credentials: "omit", auth: false, client: VR, ua: VR_UA },
    { name: "ANDROID_VR", id: 28, version: "1.62.27", credentials: "include", auth: false, client: VR, ua: VR_UA },
    // Web client as Safari: HLS manifest, with the signed-in session.
    { name: "WEB", id: 1, version: null, credentials: "include", auth: true, client: { userAgent: `${SAFARI_UA},gzip(gfe)` }, ua: SAFARI_UA },
    // iOS: HLS manifest and direct links (the latter often need a token: verification drops them).
    {
      name: "IOS",
      id: 5,
      version: "20.10.4",
      credentials: "omit",
      auth: false,
      client: { deviceMake: "Apple", deviceModel: "iPhone16,2", osName: "iPhone", osVersion: "18.3.2.22D82", userAgent: IOS_UA },
      ua: IOS_UA,
    },
  ];

  const cfg = (key) => window.ytcfg?.get?.(key) ?? window.ytcfg?.data_?.[key];
  const hex = (buf) => [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");

  /** SAPISIDHASH: how YouTube's own web app authenticates API calls for a signed-in user. */
  async function authorization() {
    const sid = document.cookie.match(/(?:^|;\s*)(?:SAPISID|__Secure-3PAPISID)=([^;]+)/)?.[1];
    if (!sid) return null;
    const ts = Math.floor(Date.now() / 1000);
    const digest = hex(await crypto.subtle.digest("SHA-1", new TextEncoder().encode(`${ts} ${sid} ${location.origin}`)));
    return ["SAPISIDHASH", "SAPISID1PHASH", "SAPISID3PHASH"].map((k) => `${k} ${ts}_${digest}`).join(" ");
  }

  async function player(videoId, c) {
    const version = c.version ?? cfg("INNERTUBE_CLIENT_VERSION");
    const visitor = cfg("VISITOR_DATA");
    const headers = {
      "content-type": "application/json",
      "x-youtube-client-name": String(c.id),
      "x-youtube-client-version": String(version),
    };
    if (visitor) headers["x-goog-visitor-id"] = visitor;
    if (c.auth) {
      const auth = await authorization();
      if (auth) {
        headers.authorization = auth;
        headers["x-goog-authuser"] = String(cfg("SESSION_INDEX") ?? 0);
        headers["x-origin"] = location.origin;
      }
    }
    const body = {
      context: {
        client: { clientName: c.name, clientVersion: version, hl: cfg("HL") ?? "fr", gl: cfg("GL") ?? "FR", visitorData: visitor, ...c.client },
      },
      videoId,
      contentCheckOk: true,
      racyCheckOk: true,
      playbackContext: { contentPlaybackContext: { signatureTimestamp: cfg("STS") } },
    };
    const res = await fetch("/youtubei/v1/player?prettyPrint=false", {
      method: "POST",
      credentials: c.credentials,
      headers,
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS), // one stuck client must not hold the others
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return res.json();
  }

  const describe = (f) => {
    const [kind, rest = ""] = (f.mimeType ?? "").split("/");
    const codecs = /codecs="([^"]+)"/.exec(f.mimeType ?? "")?.[1] ?? "";
    return {
      itag: f.itag,
      url: f.url,
      container: rest.split(";")[0],
      video: kind === "video",
      audio: kind === "audio" || codecs.includes(","),
      height: f.height ?? 0,
      fps: f.fps ?? 0,
      label: f.qualityLabel ?? "",
      size: Number(f.contentLength ?? 0),
      bitrate: f.bitrate ?? 0,
    };
  };

  /** One entry per client that answered, in preference order: `{ client, ua, formats, hls }`. */
  async function resolve(videoId) {
    const result = { title: "", clients: [], drm: false, errors: [] };
    const answers = await Promise.all(
      CLIENTS.map((c) => player(videoId, c).then((r) => ({ c, r }), (e) => ({ c, e }))),
    );
    for (const { c, r, e } of answers) {
      if (e) {
        result.errors.push(`${c.name}: ${e.message ?? e}`);
        continue;
      }
      const status = r.playabilityStatus?.status;
      if (status !== "OK") {
        result.errors.push(`${c.name}: ${r.playabilityStatus?.reason ?? status ?? "refusé"}`);
        continue;
      }
      result.title ||= r.videoDetails?.title ?? "";
      const sd = r.streamingData ?? {};
      const all = [...(sd.formats ?? []), ...(sd.adaptiveFormats ?? [])];
      result.drm ||= all.some((f) => f.drmFamilies?.length);
      // Ciphered links need YouTube's obfuscated player code: skipped rather than guessed.
      const formats = all.filter((f) => f.url && !f.signatureCipher && !f.cipher && !f.drmFamilies?.length).map(describe);
      const hls = sd.hlsManifestUrl ?? null;
      if (formats.length || hls) result.clients.push({ client: c.name, ua: c.ua, formats, hls });
    }
    result.title ||= document.title.replace(/ - YouTube$/, "");
    return result;
  }

  function currentVideoId() {
    const url = new URL(location.href);
    return (
      url.searchParams.get("v") ??
      url.pathname.match(/^\/(?:shorts|embed|live)\/([\w-]{11})/)?.[1] ??
      document.getElementById("movie_player")?.getVideoData?.()?.video_id ??
      null
    );
  }

  const cache = new Map(); // videoId → { at, promise }
  const reply = (nonce, result) => window.postMessage({ source: "rdm-yt", nonce, result }, location.origin);

  addEventListener("message", (e) => {
    if (e.source !== window || e.data?.source !== "rdm" || e.data.type !== "yt-resolve") return;
    const { nonce } = e.data;
    const videoId = currentVideoId();
    if (!videoId) return reply(nonce, { error: "no-video" });
    let hit = cache.get(videoId);
    if (!hit || Date.now() - hit.at > CACHE_MS) {
      hit = { at: Date.now(), promise: resolve(videoId) };
      cache.set(videoId, hit);
    }
    hit.promise.then(
      (result) => {
        if (!result.clients.length) cache.delete(videoId); // retry next time
        reply(nonce, result);
      },
      (err) => {
        cache.delete(videoId);
        reply(nonce, { error: String(err) });
      },
    );
  });
})();
