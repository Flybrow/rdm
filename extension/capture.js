// Recording mode (page world, document_start). Does nothing unless this tab was reloaded by RDM to
// record the current video. Then it saves exactly what YouTube's own player receives (Media Source
// Extensions appends), while the video plays muted at YouTube's official 2× speed: no token
// forging, no protection bypass. Encrypted (DRM) media is never touched.
(() => {
  const KEY = "rdm-record";
  const MAX_AGE_MS = 10 * 60_000; // a flag left behind by a crash must not trigger later
  let job;
  try {
    job = JSON.parse(sessionStorage.getItem(KEY) ?? "null");
  } catch {
    return;
  }
  const videoId = () => new URL(location.href).searchParams.get("v");
  if (!job?.token || job.videoId !== videoId() || Date.now() - job.at > MAX_AGE_MS) return;
  sessionStorage.removeItem(KEY); // one shot: a later reload must not record again
  document.documentElement.setAttribute("data-rdm-recording", job.token); // for the content script

  const RATE = 2; // highest speed YouTube itself offers: no anomaly for the player
  const ORIGIN = location.origin;
  const RELAY_TIMEOUT_MS = 20_000; // media must not pile up in memory if nobody listens
  const MAX_PENDING = 64 << 20; // an unfinished box larger than this is not a real segment
  let timer = 0;
  let active = true;
  let ready = false; // the relay (content script + extension frame) is listening
  const backlog = [];
  setTimeout(() => {
    if (!ready) {
      stop();
      backlog.length = 0;
    }
  }, RELAY_TIMEOUT_MS);

  const post = (msg, transfer = []) => {
    if (ready) window.postMessage({ source: "rdm-rec", token: job.token, ...msg }, ORIGIN, transfer);
    else backlog.push([msg, transfer]); // never lose the first init segment
  };
  addEventListener("message", (e) => {
    if (e.source !== window || e.data?.source !== "rdm-rec-control" || e.data.token !== job.token) return;
    if (e.data.type === "ready" && !ready) {
      ready = true;
      for (const [msg, transfer] of backlog.splice(0)) post(msg, transfer);
    } else if (e.data.type === "cancel") {
      stop();
    }
  });

  // 1. MP4 only (H.264/AV1 + AAC): the formats RDM can merge without re-encoding.
  const blocked = (t) => /webm|vp0?9|opus|vorbis|hev1|hvc1/i.test(String(t ?? ""));
  const isTypeSupported = MediaSource.isTypeSupported.bind(MediaSource);
  MediaSource.isTypeSupported = (t) => !blocked(t) && isTypeSupported(t);
  const canPlayType = HTMLMediaElement.prototype.canPlayType;
  HTMLMediaElement.prototype.canPlayType = function (t) {
    return blocked(t) ? "" : canPlayType.call(this, t);
  };
  if (navigator.mediaCapabilities?.decodingInfo) {
    const decodingInfo = navigator.mediaCapabilities.decodingInfo.bind(navigator.mediaCapabilities);
    navigator.mediaCapabilities.decodingInfo = (c) =>
      blocked(c?.video?.contentType) || blocked(c?.audio?.contentType)
        ? Promise.resolve({ supported: false, smooth: false, powerEfficient: false })
        : decodingInfo(c);
  }

  // 2. Record appends as complete MP4 boxes (a segment the player aborts mid-way is dropped).
  const player = () => document.getElementById("movie_player");
  const isAd = () => player()?.classList.contains("ad-showing") ?? false;
  let sources = 0;
  const sourceIds = new WeakMap();
  const buffers = new WeakMap(); // SourceBuffer → { ms, track, pending: Uint8Array }

  const addSourceBuffer = MediaSource.prototype.addSourceBuffer;
  MediaSource.prototype.addSourceBuffer = function (mime) {
    const sb = addSourceBuffer.call(this, mime);
    if (!sourceIds.has(this)) sourceIds.set(this, ++sources);
    const track = /^audio\//i.test(mime) ? "audio" : /^video\//i.test(mime) ? "video" : null;
    if (track) buffers.set(sb, { ms: sourceIds.get(this), track, pending: new Uint8Array(0) });
    return sb;
  };

  /** Splits off the complete top-level boxes; keeps an unfinished box for the next append. */
  const completeBoxes = (bytes) => {
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    let pos = 0;
    while (pos + 8 <= bytes.length) {
      let size = view.getUint32(pos);
      if (size === 1) {
        if (pos + 16 > bytes.length) break;
        size = Number(view.getBigUint64(pos + 8));
      }
      if (size < 8 || pos + size > bytes.length) break;
      pos += size;
    }
    return pos;
  };

  const appendBuffer = SourceBuffer.prototype.appendBuffer;
  SourceBuffer.prototype.appendBuffer = function (data) {
    const info = buffers.get(this);
    if (active && info && !isAd()) {
      const incoming = data instanceof ArrayBuffer ? new Uint8Array(data) : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      const joined = new Uint8Array(info.pending.length + incoming.length);
      joined.set(info.pending);
      joined.set(incoming, info.pending.length);
      const cut = completeBoxes(joined);
      info.pending = joined.length - cut > MAX_PENDING ? new Uint8Array(0) : joined.slice(cut);
      if (cut > 0) {
        const chunk = joined.slice(0, cut).buffer; // an owned copy: the player keeps its own data
        post({ type: "chunk", ms: info.ms, track: info.track, data: chunk }, [chunk]);
      }
    }
    return appendBuffer.call(this, data);
  };
  const abort = SourceBuffer.prototype.abort;
  SourceBuffer.prototype.abort = function () {
    const info = buffers.get(this);
    if (info) info.pending = new Uint8Array(0);
    return abort.call(this);
  };

  // 3. Drive playback: best quality, muted, from the start, at 2×; report progress; detect the end.
  function stop() {
    active = false;
    clearInterval(timer);
  }

  let configured = false;
  timer = setInterval(() => {
    const p = player();
    const video = p?.querySelector("video");
    if (!p?.getAvailableQualityLevels || !video) return;
    if (videoId() !== job.videoId) {
      post({ type: "abort", reason: "video-changed" }); // translated by content.js
      return stop();
    }
    if (isAd()) return post({ type: "progress", ad: true }); // not recorded; the banner says so
    if (!configured) {
      const best = p.getAvailableQualityLevels().find((q) => q !== "auto");
      if (!best) return; // levels not known yet
      configured = true;
      p.setPlaybackQualityRange?.(best, best);
      p.mute?.();
      p.seekTo?.(0, true);
      p.playVideo?.();
    }
    if (p.getPlaybackRate?.() !== RATE) p.setPlaybackRate?.(RATE);
    const duration = video.duration;
    if (Number.isFinite(duration) && duration > 0) post({ type: "progress", fraction: video.currentTime / duration, paused: video.paused });
    if (p.getPlayerState?.() === 0 || (Number.isFinite(duration) && video.currentTime >= duration - 0.25 && video.ended)) {
      post({ type: "end" });
      stop();
      settle(p);
    }
  }, 1000);

  /** Back to normal: 1× speed, and the "up next" video YouTube may autoplay stays paused. */
  function settle(p) {
    p.setPlaybackRate?.(1);
    p.pauseVideo?.();
    const guard = setInterval(() => {
      if (videoId() !== job.videoId) {
        p.pauseVideo?.();
        p.setPlaybackRate?.(1);
        clearInterval(guard);
      }
    }, 250);
    setTimeout(() => clearInterval(guard), 30_000);
  }
})();
