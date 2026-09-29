// Pure helpers shared by the service worker. The capture list itself comes from the app (/config),
// so the Rust domain stays the single source of truth; this mirrors `domain::is_capturable`.

// Only URLs lose their ?query and #fragment: in a file name "#" is ordinary ("Clip #42.mp4").
const extOf = (name = "") => {
  const path = name.includes("://") ? name.split(/[?#]/)[0] : name;
  const file = path.slice(Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\")) + 1);
  const dot = file.lastIndexOf(".");
  return dot >= 0 ? file.slice(dot + 1).toLowerCase() : "";
};

// .r00 .r01 … and .001 .002 …: parts of split archives.
const isSplitArchive = (ext) => /^r\d{1,3}$/.test(ext) || /^\d{3}$/.test(ext);

export const isCapturable = (list, name) => {
  const ext = extOf(name);
  if (!ext) return false;
  if (isSplitArchive(ext)) return true;
  return list
    .split(/[\s,;]+/)
    .some((e) => e.replace(/^\./, "").toLowerCase() === ext);
};

const MEDIA_EXTENSIONS = new Set(
  "3gp aac asf avi flac flv m4a m4v mkv mov mp3 mp4 mpa mpe mpeg mpg ogg ogv opus qt ra rm rmvb wav webm wma wmv".split(" "),
);

const pathOf = (url) => {
  try {
    return new URL(url).pathname;
  } catch {
    return "";
  }
};

export const isHls = (url, type = "") => extOf(pathOf(url)) === "m3u8" || /mpegurl/i.test(type);

export const isMediaUrl = (url) => MEDIA_EXTENSIONS.has(extOf(pathOf(url)));

// MPEG-TS fragments and DASH manifests are pieces of a stream, not standalone media.
export const isMediaType = (type = "") => /^(video|audio)\//i.test(type) && !/mp2t|mpegurl|dash/i.test(type);

/** Registrable-domain approximation (last two labels), enough to tell same-site from cross-site. */
export const siteOf = (url) => {
  try {
    return new URL(url).hostname.split(".").slice(-2).join(".");
  } catch {
    return "";
  }
};

/** The file name a `Content-Disposition` header gives (`filename*=` first), or "". */
export const dispositionName = (disposition = "") => {
  const star = /filename\*\s*=\s*([^']*)'[^']*'([^;]+)/i.exec(disposition);
  if (star) {
    try {
      return decodeURIComponent(star[2].trim());
    } catch {
      // Malformed: the plain `filename=` below.
    }
  }
  const plain = /filename\s*=\s*("((?:[^"\\]|\\.)*)"|[^;]+)/i.exec(disposition);
  return plain ? (plain[2] ?? plain[1]).replace(/\\(.)/g, "$1").trim() : "";
};

// Types a browser shows (or runs) instead of saving: kept by the browser unless sent as attachment.
const SHOWN = /^(text\/|image\/|video\/|audio\/|multipart\/)|^application\/(pdf|json|xml|xhtml\+xml|(x-)?javascript|ecmascript|wasm|rss\+xml|atom\+xml)\b/i;

/**
 * The name of the file a top-level response is about to become in the browser — a download:
 * sent as attachment, or of a type the browser cannot show — else "".
 */
export const downloadName = (url, disposition = "", type = "") => {
  const attachment = /^\s*attachment/i.test(disposition);
  if (!attachment && (!type || SHOWN.test(type))) return "";
  let name = dispositionName(disposition);
  if (!name) {
    name = pathOf(url).split("/").pop() || "";
    try {
      name = decodeURIComponent(name);
    } catch {
      // Kept as sent.
    }
  }
  return name.split(/[\\/]/).pop();
};
