// Extension-origin frame embedded in the YouTube tab being recorded: forwards the player's data to
// RDM in order. It runs with the extension's origin, the only one RDM's bridge accepts.
const BRIDGE = "http://127.0.0.1:9614";
const PAGES = new Set(["https://www.youtube.com", "https://m.youtube.com"]);
const token = location.hash.slice(1);
let queue = Promise.resolve();
let failed = false;

const tell = (type, extra = {}) => parent.postMessage({ source: "rdm-relay", token, type, ...extra }, "*");

async function call(path, body, json = false) {
  const res = await fetch(`${BRIDGE}/record/${token}/${path}`, {
    method: "POST",
    headers: json ? { "content-type": "application/json" } : undefined,
    body: json ? JSON.stringify(body) : body,
  });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
}

// Sequential: chunks must reach the file in the order the player appended them.
const enqueue = (task) => {
  queue = queue.then(async () => {
    if (failed) return;
    try {
      await task();
    } catch (e) {
      failed = true;
      tell("error", { reason: String(e.message ?? e) });
    }
  });
};

addEventListener("message", (e) => {
  const msg = e.data;
  if (!PAGES.has(e.origin) || msg?.source !== "rdm-rec" || msg.token !== token || !/^[0-9a-f]{64}$/.test(token)) return;
  switch (msg.type) {
    case "chunk":
      if (msg.data instanceof ArrayBuffer && Number.isInteger(msg.ms) && (msg.track === "video" || msg.track === "audio")) {
        enqueue(() => call(`append?ms=${msg.ms}&track=${msg.track}`, msg.data));
      }
      break;
    case "progress":
      if (Number.isFinite(msg.fraction)) enqueue(() => call("progress", { fraction: msg.fraction }, true));
      break;
    case "end":
      enqueue(async () => {
        await call("finish");
        tell("finished");
      });
      break;
    case "abort":
      enqueue(() => call("cancel"));
      break;
  }
});

tell("loaded");
