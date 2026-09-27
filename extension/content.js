// Floating "download this video" button over <video>/<audio>, IDM-style, with a quality menu.
(() => {
  if (window.__rdm) return;
  window.__rdm = true;
  const ext = globalThis.browser ?? globalThis.chrome; // Firefox / Chrome

  const MIN_W = 160;
  const MIN_H = 90;
  const HIDE_DELAY = 1500;
  const YT_TIMEOUT = 20_000;
  const onYouTube = /(^|\.)youtube(-nocookie)?\.com$/.test(location.hostname);

  const host = document.createElement("rdm-overlay");
  const root = host.attachShadow({ mode: "closed" });
  root.innerHTML = `
    <style>
      :host { all: initial; position: fixed; z-index: 2147483647; display: none;
              font: 13px/1.4 Inter, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }
      .btn { display: flex; align-items: center; gap: 8px; padding: 8px 15px 8px 11px; border: 1px solid #ffffff2e;
             border-radius: 999px; background: linear-gradient(135deg, #3d8bff, #6a5cff 55%, #b24cff); color: #fff;
             cursor: pointer; font: inherit; font-weight: 650; letter-spacing: .1px;
             box-shadow: 0 8px 24px #6a5cff73, inset 0 1px 0 #ffffff40;
             transition: transform .15s ease, box-shadow .15s ease, filter .15s ease; }
      .btn:hover { transform: translateY(-1px); filter: brightness(1.08); box-shadow: 0 12px 30px #6a5cff8c, inset 0 1px 0 #ffffff4d; }
      .btn:active { transform: scale(.97); }
      .btn svg { width: 18px; height: 18px; flex: none; }
      .menu { position: absolute; right: 0; top: calc(100% + 10px); width: min(560px, 92vw); max-height: min(460px, 70vh);
              overflow: auto; margin: 0; padding: 8px; list-style: none; color: #eaeef8; background: #111626eb;
              -webkit-backdrop-filter: blur(18px) saturate(1.4); backdrop-filter: blur(18px) saturate(1.4);
              border: 1px solid #ffffff17; border-radius: 16px; box-shadow: 0 24px 60px #0000008c;
              animation: pop .16s ease-out; scrollbar-width: thin; scrollbar-color: #ffffff30 transparent; }
      @keyframes pop { from { opacity: 0; transform: translateY(-6px) scale(.98); } }
      .menu[hidden] { display: none; }
      li { padding: 9px 12px; border-radius: 10px; }
      li.action { cursor: pointer; transition: background .12s ease; }
      li.action:hover { background: linear-gradient(135deg, #3d8bff47, #b24cff38); }
      li.section { padding: 10px 12px 4px; color: #8d97b3; font-size: 10.5px; font-weight: 700; letter-spacing: .08em; text-transform: uppercase; }
      li.note { color: #8d97b3; }
      .title { overflow-wrap: anywhere; font-weight: 600; }
      .detail { color: #8d97b3; font-size: 12px; margin-top: 2px; }
      li.action:hover .detail { color: #d4dbff; }
    </style>
    <button class="btn"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"
      stroke-linejoin="round" aria-hidden="true"><path d="M12 4v11"/><path d="m7 11 5 5 5-5"/><path d="M5 20h14"/></svg>
      <span>Télécharger cette vidéo</span></button>
    <ul class="menu" hidden></ul>`;
  const btn = root.querySelector(".btn");
  const menu = root.querySelector(".menu");

  let target = null;
  let hideTimer = 0;
  let frame = 0;
  let generation = 0; // stale async menu builds must not overwrite a newer one
  let view = ""; // which menu is open: live refreshes only apply to the generic list

  // ── Atoms ─────────────────────────────────────────────────────────────
  const isHttp = (u) => /^https?:\/\//i.test(u ?? "");
  const human = (n) => {
    if (!n) return "";
    const units = ["o", "Ko", "Mo", "Go"];
    const i = Math.min(units.length - 1, Math.floor(Math.log(n) / Math.log(1024)));
    return `${(n / 1024 ** i).toFixed(i ? 1 : 0)} ${units[i]}`;
  };
  const safeName = (s) => s.replace(/[<>:"/\\|?*\x00-\x1f]/g, "_").replace(/\s+/g, " ").trim().slice(0, 150) || "video";
  const pageTitle = () => safeName(top === self ? document.title : "");
  const nameOf = (url) => {
    const { pathname, hostname } = new URL(url);
    const last = pathname.split("/").pop() || hostname;
    try {
      return decodeURIComponent(last);
    } catch {
      return last; // malformed %-escape: keep it raw rather than break the menu
    }
  };
  const send = (msg) => ext.runtime.sendMessage(msg).catch(() => undefined);

  const li = (className, title, detail, onClick) => {
    const el = document.createElement("li");
    el.className = className;
    el.append(Object.assign(document.createElement("div"), { className: "title", textContent: title }));
    if (detail) el.append(Object.assign(document.createElement("div"), { className: "detail", textContent: detail }));
    if (onClick) el.addEventListener("click", onClick);
    return el;
  };
  const section = (text) => li("section", text);
  const note = (text) => li("note", text);
  const show = (...items) => {
    menu.replaceChildren(...items);
    menu.hidden = false;
  };

  // `via`: how RDM must fetch the link — `{ user_agent, bare }` for YouTube, `{}` elsewhere.
  const download = (url, filename, audio_url, via = {}) => async () => {
    const ok = await send({ kind: "download", url, audio_url, filename, ...via });
    show(note(ok ? "✓ Envoyé à RDM" : "✗ RDM n'est pas lancé"));
    setTimeout(hide, 1400);
  };
  const action = (title, detail, url, filename, audio_url, via) =>
    li("action", title, detail, download(url, filename, audio_url, via));

  // ── HLS: quality picker (resolved by RDM) ────────────────────────────
  /** Items for an HLS stream already probed by RDM; only what RDM can deliver with sound. */
  function hlsItems(url, info, base, via) {
    const ext = info?.fmp4 ? "mp4" : "ts";
    const usable = (info?.variants ?? []).filter((v) => !v.audio || info.fmp4);
    if (!usable.length) return [action(`${base}.${ext}`, "Meilleure qualité disponible", url, `${base}.${ext}`, undefined, via)];
    return usable.map((v) => {
      const kbps = `${Math.round(v.bandwidth / 1000)} kb/s`;
      const quality = v.height ? `${v.height}p` : kbps;
      const filename = `${base} (${quality}).${ext}`;
      return action(filename, `${quality} · ${kbps}${v.audio ? " · audio séparé" : ""}`, v.url, filename, v.audio ?? undefined, via);
    });
  }

  async function hlsMenu(url, base, via = {}) {
    const gen = ++generation;
    view = "hls";
    show(note("Lecture des qualités…"));
    const info = await send({ kind: "probe", url, ...via });
    if (gen !== generation) return;
    if (!info) return show(note("✗ Flux illisible (RDM est-il lancé ?)"));
    show(section("Choisir la qualité"), ...hlsItems(url, info, base, via));
  }

  // ── YouTube: formats resolved in-page by youtube.js ──────────────────
  function youtubeFormats() {
    return new Promise((resolve) => {
      const nonce = crypto.randomUUID();
      const done = (result) => {
        removeEventListener("message", onMessage);
        clearTimeout(timer);
        resolve(result);
      };
      const onMessage = (e) => {
        if (e.source === window && e.data?.source === "rdm-yt" && e.data.nonce === nonce) done(e.data.result ?? {});
      };
      const timer = setTimeout(() => done({ error: "délai dépassé" }), YT_TIMEOUT);
      addEventListener("message", onMessage);
      window.postMessage({ source: "rdm", type: "yt-resolve", nonce }, location.origin);
    });
  }

  // Only YouTube's stream servers: a page script could otherwise answer with arbitrary URLs.
  const isYouTubeMedia = (u) => {
    try {
      const { protocol, hostname } = new URL(u);
      return protocol === "https:" && hostname.endsWith(".googlevideo.com");
    } catch {
      return false;
    }
  };

  /** Menu items for one client's direct links (all verified to download with `via`). */
  function formatItems(formats, base, via) {
    const items = [];
    const bestAudio = formats.filter((f) => f.audio && !f.video && f.container === "mp4").sort((a, b) => b.bitrate - a.bitrate)[0];
    const videos = new Map(); // height → best video-only MP4
    for (const f of formats.filter((f) => f.video && !f.audio && f.container === "mp4")) {
      const cur = videos.get(f.height);
      if (!cur || f.bitrate > cur.bitrate) videos.set(f.height, f);
    }
    if (bestAudio && videos.size) {
      items.push(section("Vidéo + audio (fusion automatique)"));
      for (const v of [...videos.values()].sort((a, b) => b.height - a.height)) {
        const label = v.label || `${v.height}p`;
        const filename = `${base} (${label}).mp4`;
        const detail = [label, human(v.size + bestAudio.size), "MP4"].filter(Boolean).join(" · ");
        items.push(action(filename, detail, v.url, filename, bestAudio.url, via));
      }
    }
    const muxed = formats.filter((f) => f.video && f.audio).sort((a, b) => b.height - a.height);
    if (muxed.length) {
      items.push(section("Vidéo avec son"));
      for (const m of muxed) {
        const filename = `${base} (${m.label || `${m.height}p`}).${m.container}`;
        const detail = [m.label, human(m.size), m.container.toUpperCase()].filter(Boolean).join(" · ");
        items.push(action(filename, detail, m.url, filename, undefined, via));
      }
    }
    const audios = formats.filter((f) => f.audio && !f.video).sort((a, b) => b.bitrate - a.bitrate);
    if (audios.length) {
      items.push(section("Audio seul"));
      for (const a of audios.slice(0, 2)) {
        const ext = a.container === "mp4" ? "m4a" : "weba";
        const filename = `${base}.${ext}`;
        const detail = [`${Math.round(a.bitrate / 1000)} kb/s`, human(a.size), ext.toUpperCase()].filter(Boolean).join(" · ");
        items.push(action(filename, detail, a.url, filename, undefined, via));
      }
    }
    return items;
  }

  const describeRefusal = (res) => (res?.status ? `HTTP ${res.status}` : "injoignable");

  async function youtubeMenu() {
    const gen = ++generation;
    view = "youtube";
    show(note("Recherche des qualités YouTube…"));
    const yt = await youtubeFormats();
    if (gen !== generation) return;
    const base = safeName(yt.title || pageTitle());
    const notes = [...(yt.errors ?? [])];
    const items = [];

    // Direct links: the first client whose links really download from RDM wins.
    show(note("Vérification des liens auprès de YouTube…"));
    for (const c of yt.clients ?? []) {
      const formats = c.formats.filter((f) => isYouTubeMedia(f.url));
      const sample = formats.find((f) => f.audio && !f.video) ?? formats[0];
      if (!sample) continue;
      const via = { user_agent: c.ua, bare: true };
      const res = await send({ kind: "check", url: sample.url, ...via });
      if (gen !== generation) return;
      if (res?.ok) {
        items.push(...formatItems(formats, base, via));
        break;
      }
      notes.push(`${c.client} : liens refusés (${describeRefusal(res)})`);
    }

    // HLS as an alternative (or the only way): listed once RDM could read it.
    for (const c of yt.clients ?? []) {
      if (!c.hls || !isYouTubeMedia(c.hls)) continue;
      const via = { user_agent: c.ua, bare: true };
      const info = await send({ kind: "probe", url: c.hls, ...via });
      if (gen !== generation) return;
      if (info?.variants) {
        items.push(section("Flux HLS"), ...hlsItems(c.hls, info, base, via));
        break;
      }
      notes.push(`${c.client} : flux HLS illisible`);
    }

    if (yt.drm) {
      items.push(note("Vidéo protégée par DRM : non téléchargeable."));
    } else {
      if (!items.length) items.push(note("Téléchargement direct refusé par YouTube (jeton anti-robot requis) : utilisez l'enregistrement."));
      // Always available: records what YouTube's own player receives (see capture.js).
      const minutes = target?.duration ? Math.ceil(target.duration / 2 / 60) : null;
      const detail = `Lecture muette en 2×, qualité maximale${minutes ? ` · environ ${minutes} min` : ""} · la page va se recharger`;
      items.push(section("Enregistrement"), li("action", `${base}.mp4`, detail, () => startRecording(`${base}.mp4`)));
    }
    show(...items);
  }

  // ── YouTube recording: page reload in recording mode, relay to RDM, progress banner ──
  async function startRecording(filename) {
    const videoId = new URL(location.href).searchParams.get("v");
    if (!videoId) return show(note("✗ Ouvrez la vidéo elle-même (page « watch ») pour l'enregistrer."));
    show(note("Préparation de l'enregistrement…"));
    const token = await send({ kind: "record-start", filename });
    if (typeof token !== "string") return show(note("✗ RDM n'est pas lancé"));
    sessionStorage.setItem("rdm-record", JSON.stringify({ token, videoId, at: Date.now() }));
    location.reload(); // the player must start afresh with the recorder in place (capture.js)
  }

  function recorder(token) {
    const banner = document.createElement("rdm-recorder");
    const shadow = banner.attachShadow({ mode: "closed" });
    shadow.innerHTML = `
      <style>
        :host { all: initial; position: fixed; top: 14px; left: 50%; transform: translateX(-50%); z-index: 2147483647;
                font: 13px/1.4 Inter, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }
        .bar { display: flex; align-items: center; gap: 12px; padding: 10px 12px 10px 16px; border-radius: 999px;
               color: #eaeef8; background: #111626eb; -webkit-backdrop-filter: blur(18px) saturate(1.4);
               backdrop-filter: blur(18px) saturate(1.4); border: 1px solid #ffffff1f;
               box-shadow: 0 16px 40px #00000080, 0 0 0 1px #6a5cff33; animation: drop .25s ease-out; }
        @keyframes drop { from { opacity: 0; transform: translateY(-8px); } }
        .dot { width: 10px; height: 10px; border-radius: 50%; background: #fb7185; box-shadow: 0 0 0 4px #fb718533;
               animation: pulse 1.2s ease-in-out infinite; }
        .done .dot { background: #34d399; box-shadow: 0 0 0 4px #34d39933; animation: none; }
        @keyframes pulse { 50% { box-shadow: 0 0 0 7px #fb718500; opacity: .6; } }
        button { border: 1px solid #ffffff26; background: #ffffff14; color: #eaeef8; border-radius: 999px; padding: 4px 12px;
                 cursor: pointer; font: inherit; font-weight: 600; transition: background .12s ease; }
        button:hover { background: #ffffff26; }
      </style>
      <div class="bar"><span class="dot"></span><span class="text">RDM prépare l'enregistrement…</span><button>Annuler</button></div>`;
    const bar = shadow.querySelector(".bar");
    const text = shadow.querySelector(".text");
    const cancel = shadow.querySelector("button");
    document.documentElement.append(banner);

    const frame = document.createElement("iframe");
    frame.src = `${ext.runtime.getURL("relay.html")}#${token}`;
    frame.style.display = "none";
    const relayOrigin = new URL(frame.src).origin;
    const control = (type) => window.postMessage({ source: "rdm-rec-control", token, type }, location.origin);
    const toRelay = (msg, transfer = []) => frame.contentWindow?.postMessage(msg, relayOrigin, transfer);
    const started = Date.now();
    let over = false;

    const finish = (message, ok) => {
      over = true;
      text.textContent = message;
      cancel.remove();
      bar.classList.toggle("done", ok);
      setTimeout(() => banner.remove(), ok ? 8000 : 15000);
    };

    addEventListener("message", (e) => {
      const msg = e.data;
      if (msg?.token !== token) return;
      if (e.source === window && msg.source === "rdm-rec") {
        // Player → RDM (zero-copy transfer of the media chunk).
        toRelay(msg, msg.data instanceof ArrayBuffer ? [msg.data] : []);
        if (msg.type === "progress" && !over && msg.ad) {
          text.textContent = "Publicité en cours (non enregistrée) — l'enregistrement reprend juste après";
        } else if (msg.type === "progress" && !over) {
          const f = Math.min(Math.max(msg.fraction, 0), 1);
          const left = f > 0.02 ? Math.round(((Date.now() - started) / 1000) * (1 - f) / f) : null;
          const eta = left == null ? "" : left >= 60 ? ` · reste ${Math.ceil(left / 60)} min` : ` · reste ${left} s`;
          text.textContent = msg.paused && f < 0.01
            ? "Cliquez sur la vidéo pour lancer l'enregistrement"
            : `RDM enregistre cette vidéo · ${Math.floor(f * 100)} %${eta}`;
        }
        if (msg.type === "end") text.textContent = "Fusion en cours dans RDM…";
        if (msg.type === "abort") finish(`✗ Enregistrement annulé : ${msg.reason}`, false);
      } else if (e.source === frame.contentWindow && msg.source === "rdm-relay") {
        if (msg.type === "loaded") control("ready");
        if (msg.type === "finished") finish("✓ Enregistrement terminé : le fichier est dans RDM", true);
        if (msg.type === "error") {
          control("cancel");
          finish(`✗ Enregistrement interrompu (${msg.reason}) — RDM est-il lancé ?`, false);
        }
      }
    });
    cancel.addEventListener("click", () => {
      control("cancel");
      toRelay({ source: "rdm-rec", token, type: "abort" });
      finish("Enregistrement annulé", false);
    });
    document.documentElement.append(frame);
  }

  const recordingToken = document.documentElement.getAttribute("data-rdm-recording");
  if (onYouTube && top === self && recordingToken && /^[0-9a-f]{64}$/.test(recordingToken)) recorder(recordingToken);

  // ── Generic pages: <video> sources + media seen on the network ───────
  const elementSources = (el) =>
    [el.currentSrc, el.src, ...[...el.querySelectorAll("source")].map((s) => s.src)]
      .filter(isHttp)
      .map((url) => ({ url, size: 0, type: el.tagName.toLowerCase() }));

  async function genericMenu() {
    const gen = ++generation;
    view = "generic";
    const network = (await send({ kind: "list" })) ?? [];
    if (gen !== generation) return;
    const seen = new Set();
    const items = [...(target ? elementSources(target) : []), ...network.sort((a, b) => b.size - a.size)].filter(
      ({ url }) => isHttp(url) && !seen.has(url) && seen.add(url),
    );
    if (!items.length) return show(note("Aucun flux détecté — lancez la lecture, puis réessayez."));
    const base = pageTitle();
    show(
      section("Vidéos détectées"),
      ...items.map((m) =>
        m.hls
          ? li("action", `${base} (flux HLS)`, "Choisir la qualité…", () => hlsMenu(m.url, base))
          : action(nameOf(m.url), [human(m.size), m.type].filter(Boolean).join(" · "), m.url, nameOf(m.url)),
      ),
    );
  }

  const openMenu = () => (onYouTube ? youtubeMenu() : genericMenu());

  // ── Positioning ───────────────────────────────────────────────────────
  function place() {
    if (!target?.isConnected) return hide();
    const r = target.getBoundingClientRect();
    host.style.top = `${Math.max(4, r.top + 8)}px`;
    host.style.left = `${Math.max(4, r.right - btn.offsetWidth - 8)}px`;
  }

  function showButton(el) {
    clearTimeout(hideTimer);
    const container = document.fullscreenElement?.tagName === "VIDEO" ? null : document.fullscreenElement;
    const parent = container ?? document.documentElement;
    if (host.parentNode !== parent) parent.append(host);
    if (target !== el) menu.hidden = true;
    target = el;
    host.style.display = "block";
    place();
  }

  function hide() {
    host.style.display = "none";
    menu.hidden = true;
    target = null;
    generation++;
  }

  const scheduleHide = () => {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => menu.hidden && hide(), HIDE_DELAY);
  };

  // Live collections: no DOM query per mouse move, and a free "this page has no media" test.
  const videos = document.getElementsByTagName("video");
  const audios = document.getElementsByTagName("audio");

  // Players cover <video> with overlays (sometimes `pointer-events: none`), so hit-test by geometry.
  const mediaAt = (x, y) => {
    for (const list of [videos, audios]) {
      for (const el of list) {
        const r = el.getBoundingClientRect();
        if (r.width >= MIN_W && r.height >= MIN_H && x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return el;
      }
    }
    return null;
  };

  document.addEventListener(
    "pointermove",
    ({ clientX, clientY }) => {
      // Most pages have no media at all: nothing to do, not even a layout read.
      if (frame || (!target && !videos.length && !audios.length)) return;
      frame = requestAnimationFrame(() => {
        frame = 0;
        const el = mediaAt(clientX, clientY);
        if (el) showButton(el);
        else if (target) scheduleHide();
      });
    },
    { passive: true, capture: true },
  );

  addEventListener("scroll", () => target && place(), { passive: true, capture: true });
  addEventListener("resize", () => target && place(), { passive: true });
  host.addEventListener("pointerenter", () => clearTimeout(hideTimer));
  host.addEventListener("pointerleave", scheduleHide);
  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    if (menu.hidden) openMenu();
    else menu.hidden = true;
  });
  document.addEventListener("click", (e) => e.composedPath().includes(host) || (menu.hidden = true), true);

  ext.runtime.onMessage.addListener((msg) => {
    if (msg?.kind === "media-changed" && !menu.hidden && view === "generic") genericMenu();
  });
})();
