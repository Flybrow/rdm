//! HLS (`.m3u8`): variant choice, alternate audio tracks, AES-128, parallel fetch with in-order append.

use std::{
    collections::HashMap,
    io::SeekFrom,
    path::Path,
    sync::{Arc, Mutex, PoisonError, atomic::Ordering::Relaxed},
    time::Duration,
};

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use futures_util::{StreamExt, stream};
use reqwest::{Client, header::HeaderMap};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{EngineError, Job, Outcome, Progress, RateLimit, merged::Split, net, state_path};

const RETRIES: u32 = 8;
const MAX_PLAYLIST_BYTES: u64 = 16 << 20;
/// A single segment larger than this is not a segment: refuse instead of exhausting memory.
const MAX_PART_BYTES: u64 = 256 << 20;
/// Segments held in memory while waiting to be written in order. 8 keep a fast line busy: on a
/// 600 MB 1080p stream, as fast as 16, with a third less memory.
const MAX_IN_FLIGHT: usize = 8;
/// Memory set aside for a segment before it arrives, at most (see `get_once`).
const RESERVE_MAX: u64 = 16 << 20;
/// Resume state written this often while downloading (data synced first).
const CHECKPOINT: Duration = Duration::from_secs(20);

pub(crate) fn is_playlist(url: &Url, content_type: Option<&str>) -> bool {
    url.path().to_ascii_lowercase().ends_with(".m3u8")
        || content_type.is_some_and(|t| t.to_ascii_lowercase().contains("mpegurl"))
}

/// One quality of a master playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Variant {
    pub url: Url,
    /// Separate audio rendition to fetch and mux alongside (`EXT-X-MEDIA:TYPE=AUDIO`).
    pub audio: Option<Url>,
    pub bandwidth: u64,
    pub height: Option<u32>,
}

/// What a playlist offers, without downloading it.
#[derive(Debug, Clone, Serialize)]
pub struct HlsInfo {
    /// Best first; empty for a plain media playlist.
    pub variants: Vec<Variant>,
    /// Segments are fragmented MP4 (→ `.mp4`), otherwise MPEG-TS (→ `.ts`).
    pub fmp4: bool,
}

#[derive(Clone)]
struct Key {
    url: Url,
    iv: Option<[u8; 16]>,
}

struct Part {
    url: Url,
    key: Option<Key>,
    seq: u64,
    /// `EXT-X-BYTERANGE`: (offset, length) inside `url`.
    range: Option<(u64, u64)>,
}

enum Playlist {
    Master(Vec<Variant>),
    Media { parts: Vec<Part>, fmp4: bool },
}

/// Parts written so far and the file length they produced.
#[derive(Default, Serialize, Deserialize)]
struct Resume {
    done: usize,
    bytes: u64,
}

struct Http<'a> {
    client: &'a Client,
    headers: &'a HeaderMap,
    /// Where the playlist came from: every other URL must not escape it into the LAN, and gets
    /// its credentials only on the same site.
    origin: &'a Url,
    keys: Mutex<HashMap<Url, Arc<[u8]>>>,
    limit: &'a RateLimit,
    own_limit: &'a RateLimit,
}

pub(crate) async fn info(client: &Client, url: &Url, headers: &HeaderMap) -> Result<HlsInfo, EngineError> {
    let limit = RateLimit::default();
    let http = Http { client, headers, origin: url, keys: Mutex::default(), limit: &limit, own_limit: &limit };
    match load(&http, url).await? {
        Playlist::Media { fmp4, .. } => Ok(HlsInfo { variants: Vec::new(), fmp4 }),
        Playlist::Master(variants) => {
            let fmp4 = match variants.first() {
                Some(best) => matches!(load(&http, &best.url).await?, Playlist::Media { fmp4: true, .. }),
                None => false,
            };
            Ok(HlsInfo { variants, fmp4 })
        }
    }
}

pub(crate) async fn run(
    client: &Client,
    job: &Job,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<Outcome, EngineError> {
    let http = Http { client, headers: &job.headers, origin: &job.url, keys: Mutex::default(), limit: &job.limit, own_limit: &job.own_limit };
    let n = job.connections;
    // Playlists load with retries: a pause must not wait for them.
    let plan = async {
        match load(&http, &job.url).await? {
            Playlist::Media { parts, .. } => Ok((parts, None)),
            Playlist::Master(variants) => choose(&http, &variants).await,
        }
    };
    let (video, audio) = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(Outcome::Paused),
        plan = plan => plan?,
    };
    let Some(audio) = audio else {
        return download(&http, video, &job.target, n, &progress, &cancel).await;
    };
    let split = Split::new(&job.target, &cancel);
    let v = Box::pin(download(&http, video, &split.video, n, &split.progress[0], &split.stop));
    let a = Box::pin(async {
        let playlist = tokio::select! {
            biased;
            () = split.stop.cancelled() => return Ok(Outcome::Paused),
            playlist = load(&http, &audio) => playlist?,
        };
        let Playlist::Media { parts, .. } = playlist else {
            return Err(EngineError::Playlist("nested master playlist"));
        };
        download(&http, parts, &split.audio, (n / 4).max(1), &split.progress[1], &split.stop).await
    });
    split.finish(&progress, &job.target, v, a).await
}

/// Best variant that we can deliver with sound: separate audio only when both tracks are fMP4
/// (muxable); otherwise the best variant with muxed audio.
async fn choose(http: &Http<'_>, variants: &[Variant]) -> Result<(Vec<Part>, Option<Url>), EngineError> {
    let media = |v: &Variant| {
        let url = v.url.clone();
        async move {
            match load(http, &url).await? {
                Playlist::Media { parts, fmp4 } => Ok((parts, fmp4)),
                Playlist::Master(_) => Err(EngineError::Playlist("nested master playlist")),
            }
        }
    };
    let best = variants.first().ok_or(EngineError::Playlist("empty master playlist"))?;
    let (parts, fmp4) = media(best).await?;
    match &best.audio {
        None => Ok((parts, None)),
        Some(audio) if fmp4 => Ok((parts, Some(audio.clone()))),
        Some(_) => match variants.iter().find(|v| v.audio.is_none()) {
            Some(muxed) => Ok((media(muxed).await?.0, None)),
            None => Err(EngineError::Playlist("separate MPEG-TS audio track is not supported")),
        },
    }
}

async fn download(
    http: &Http<'_>,
    parts: Vec<Part>,
    target: &Path,
    connections: u8,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<Outcome, EngineError> {
    let state = state_path(target);
    let mut resume = load_state(&state).await.unwrap_or_default();
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir).await?;
    }
    let mut file = OpenOptions::new().create(true).write(true).truncate(false).open(target).await?;
    if file.metadata().await?.len() < resume.bytes || resume.done > parts.len() {
        resume = Resume::default();
    }
    file.set_len(resume.bytes).await?;
    file.seek(SeekFrom::End(0)).await?;

    let total_parts = parts.len() as u64;
    let report = |r: &Resume| {
        progress.downloaded.store(r.bytes, Relaxed);
        if r.done > 0 {
            progress.total.store(r.bytes * total_parts / r.done as u64, Relaxed);
        }
    };
    report(&resume);
    progress.active.store(connections.into(), Relaxed);

    let mut fetched = stream::iter(parts.into_iter().skip(resume.done))
        .map(|part| fetch_part(http, part))
        .buffered(usize::from(connections).clamp(1, MAX_IN_FLIGHT));

    // A crash or a kill mid-download loses at most this much (the segmented engine does the same).
    let mut checkpoint = tokio::time::interval_at(tokio::time::Instant::now() + CHECKPOINT, CHECKPOINT);
    let result = loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => break Ok(Outcome::Paused),
            _ = checkpoint.tick() => {
                if file.flush().await.is_ok() && file.sync_data().await.is_ok() {
                    let _ = save_state(&state, &resume).await;
                }
                continue;
            }
            next = fetched.next() => next,
        };
        match next {
            None => break Ok(Outcome::Completed),
            Some(Err(e)) => break Err(e),
            Some(Ok(data)) => {
                if let Err(e) = file.write_all(&data).await {
                    break Err(e.into());
                }
                resume.done += 1;
                resume.bytes += data.len() as u64;
                report(&resume);
            }
        }
    };
    progress.active.store(0, Relaxed);
    file.flush().await?;

    match result {
        Ok(Outcome::Completed) if resume.bytes == 0 => Err(EngineError::Empty),
        Ok(Outcome::Completed) => {
            let _ = fs::remove_file(&state).await;
            progress.total.store(resume.bytes, Relaxed);
            Ok(Outcome::Completed)
        }
        other => {
            // Data on disk first, then an atomic state (temp + rename), like the segmented engine.
            file.sync_data().await?;
            save_state(&state, &resume).await?;
            other
        }
    }
}

async fn save_state(state: &Path, resume: &Resume) -> std::io::Result<()> {
    crate::write_atomic(state, &serde_json::to_vec(resume)?).await
}

async fn load(http: &Http<'_>, url: &Url) -> Result<Playlist, EngineError> {
    let body = get(http, url, None, MAX_PLAYLIST_BYTES).await?;
    parse(url, &String::from_utf8_lossy(&body))
}

fn parse(base: &Url, text: &str) -> Result<Playlist, EngineError> {
    if !text.trim_start_matches('\u{feff}').trim_start().starts_with("#EXTM3U") {
        return Err(EngineError::Playlist("not an m3u8 playlist"));
    }
    if text.contains("#EXT-X-STREAM-INF") {
        return Ok(Playlist::Master(parse_master(base, text)));
    }
    let parts = parse_media(base, text)?;
    Ok(Playlist::Media { parts, fmp4: text.contains("#EXT-X-MAP") })
}

fn parse_master(base: &Url, text: &str) -> Vec<Variant> {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    // GROUP-ID → the audio rendition a player takes: the DEFAULT one, else the first. One without
    // a URI is the sound inside the variant's own segments: no separate track then.
    let mut audio: HashMap<&str, (bool, Option<Url>)> = HashMap::new();
    for attrs in lines.iter().filter_map(|l| l.strip_prefix("#EXT-X-MEDIA:")) {
        if attr(attrs, "TYPE") != Some("AUDIO") {
            continue;
        }
        let Some(group) = attr(attrs, "GROUP-ID") else { continue };
        let url = match attr(attrs, "URI").map(|u| base.join(u)) {
            Some(Ok(url)) => Some(url),
            Some(Err(_)) => continue,
            None => None,
        };
        let default = attr(attrs, "DEFAULT") == Some("YES");
        if audio.get(group).is_none_or(|(was_default, _)| default && !was_default) {
            audio.insert(group, (default, url));
        }
    }

    let mut variants = Vec::new();
    let mut pending: Option<&str> = None;
    for line in &lines {
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attrs);
        } else if !line.is_empty() && !line.starts_with('#')
            && let (Some(attrs), Ok(url)) = (pending.take(), base.join(line))
        {
            variants.push(Variant {
                url,
                audio: attr(attrs, "AUDIO").and_then(|g| audio.get(g)).and_then(|(_, u)| u.clone()),
                bandwidth: attr(attrs, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                height: attr(attrs, "RESOLUTION").and_then(|r| r.split_once(['x', 'X'])?.1.parse().ok()),
            });
        }
    }
    variants.sort_by_key(|v| std::cmp::Reverse((v.height, v.bandwidth)));
    variants.dedup_by(|a, b| a.url == b.url);
    variants
}

fn parse_media(base: &Url, text: &str) -> Result<Vec<Part>, EngineError> {
    let join = |u: &str| base.join(u).map_err(|_| EngineError::Playlist("bad segment url"));
    let (mut parts, mut key, mut seq) = (Vec::new(), None, 0u64);
    // `EXT-X-BYTERANGE` without `@offset` continues right after the previous range of the same resource.
    let mut pending_range: Option<(u64, Option<u64>)> = None;
    let mut last_end: Option<(Url, u64)> = None;
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = Some(parse_byterange(v).ok_or(EngineError::Playlist("bad byte range"))?);
        } else if let Some(a) = line.strip_prefix("#EXT-X-KEY:") {
            key = match attr(a, "METHOD") {
                Some("NONE") => None,
                Some("AES-128") => Some(Key {
                    url: join(attr(a, "URI").ok_or(EngineError::Playlist("key without uri"))?)?,
                    iv: attr(a, "IV").and_then(parse_iv),
                }),
                _ => return Err(EngineError::Playlist("DRM-protected stream")),
            };
        } else if let Some(a) = line.strip_prefix("#EXT-X-MAP:") {
            let uri = attr(a, "URI").ok_or(EngineError::Playlist("map without uri"))?;
            let range = match attr(a, "BYTERANGE") {
                Some(r) => {
                    let (len, offset) = parse_byterange(r).ok_or(EngineError::Playlist("bad byte range"))?;
                    let offset = offset.unwrap_or(0);
                    offset.checked_add(len).ok_or(EngineError::Playlist("bad byte range"))?;
                    Some((offset, len))
                }
                None => None,
            };
            parts.push(Part { url: join(uri)?, key: None, seq, range });
        } else if !line.is_empty() && !line.starts_with('#') {
            let url = join(line)?;
            let range = match pending_range.take() {
                None => None,
                Some((len, offset)) => {
                    let offset = match (offset, &last_end) {
                        (Some(o), _) => o,
                        (None, Some((prev, end))) if *prev == url => *end,
                        (None, _) => return Err(EngineError::Playlist("byte range without offset")),
                    };
                    let end = offset.checked_add(len).ok_or(EngineError::Playlist("bad byte range"))?;
                    last_end = Some((url.clone(), end));
                    Some((offset, len))
                }
            };
            parts.push(Part { url, key: key.clone(), seq, range });
            seq = seq.wrapping_add(1);
        }
    }
    if parts.is_empty() {
        return Err(EngineError::Playlist("empty playlist"));
    }
    Ok(parts)
}

/// `length[@offset]`.
fn parse_byterange(v: &str) -> Option<(u64, Option<u64>)> {
    let (len, offset) = match v.trim().split_once('@') {
        Some((l, o)) => (l, Some(o.trim().parse().ok()?)),
        None => (v.trim(), None),
    };
    let len: u64 = len.trim().parse().ok()?;
    (len > 0 && len <= MAX_PART_BYTES).then_some((len, offset))
}

async fn fetch_part(http: &Http<'_>, part: Part) -> Result<Vec<u8>, EngineError> {
    let data = get(http, &part.url, part.range, MAX_PART_BYTES).await?;
    // Dropped with the stream on cancel.
    http.limit.take(data.len()).await;
    http.own_limit.take(data.len()).await;
    let Some(key) = part.key else { return Ok(data) };
    let cached = http.keys.lock().unwrap_or_else(PoisonError::into_inner).get(&key.url).cloned();
    let secret = match cached {
        Some(k) => k,
        None => {
            let k: Arc<[u8]> = get(http, &key.url, None, 1024).await?.into();
            http.keys.lock().unwrap_or_else(PoisonError::into_inner).insert(key.url, k.clone());
            k
        }
    };
    let iv = key.iv.unwrap_or_else(|| u128::from(part.seq).to_be_bytes());
    cbc::Decryptor::<aes::Aes128>::new_from_slices(&secret, &iv)
        .map_err(|_| EngineError::Playlist("invalid AES key"))?
        .decrypt_padded_vec_mut::<Pkcs7>(&data)
        .map_err(|_| EngineError::Playlist("decryption failed"))
}

/// GET with retries, an optional (offset, length) byte range and a hard size cap.
async fn get(http: &Http<'_>, url: &Url, range: Option<(u64, u64)>, max: u64) -> Result<Vec<u8>, EngineError> {
    if !net::allowed_hop(http.origin, url) {
        return Err(EngineError::LocalNetwork);
    }
    let mut attempt = 0;
    loop {
        match get_once(http, url, range, max).await {
            Ok(data) => return Ok(data),
            Err(e) if attempt >= RETRIES || e.is_permanent() => return Err(e),
            Err(e) => {
                e.forget_address();
                attempt += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(250 << attempt.min(5))).await;
    }
}

async fn get_once(http: &Http<'_>, url: &Url, range: Option<(u64, u64)>, max: u64) -> Result<Vec<u8>, EngineError> {
    // The page's cookies and the site's login stay on the playlist's site.
    let headers = net::headers_for(http.headers, http.origin, url).into_owned();
    let mut req = http.client.get(url.clone()).headers(headers);
    if let Some((offset, len)) = range {
        let last = offset.checked_add(len - 1).ok_or(EngineError::Playlist("bad byte range"))?;
        req = req.header(reqwest::header::RANGE, format!("bytes={offset}-{last}"));
    }
    let res = req.send().await?.error_for_status()?;

    // How many bytes to read, and where the wanted slice starts within them.
    let (skip, want) = match range {
        // 206: exactly the slice.
        Some((_, len)) if res.status() == reqwest::StatusCode::PARTIAL_CONTENT => (0, len),
        // 200: the server ignored the range; read the prefix up to the slice, if that stays reasonable.
        Some((offset, len)) => (offset, offset.saturating_add(len)),
        None => (0, max),
    };
    if want > max {
        return Err(if range.is_some() { EngineError::RangeIgnored } else { EngineError::Playlist("too large") });
    }
    if range.is_none() && res.content_length().is_some_and(|n| n > max) {
        return Err(EngineError::Playlist("too large"));
    }

    // Room for the announced size at once: grown by doubling, a segment would take up to twice
    // its size, and several are held at once (see `MAX_IN_FLIGHT`). Only up to a usual segment's
    // size: the announced length is the server's word, not data received.
    let announced = res.content_length().unwrap_or(0).min(want).min(RESERVE_MAX);
    let mut body = Vec::with_capacity(usize::try_from(announced).unwrap_or(0));
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
        let read = body.len() as u64;
        if range.is_some() && read >= want {
            break; // got the slice: the rest (if any) is not ours
        }
        if read > want {
            return Err(EngineError::Playlist("too large"));
        }
    }
    let Some((_, len)) = range else { return Ok(body) };
    let (start, end) = (usize::try_from(skip), usize::try_from(skip + len));
    match (start, end) {
        (Ok(s), Ok(e)) => body.get(s..e).map(<[u8]>::to_vec).ok_or(EngineError::Truncated),
        _ => Err(EngineError::Truncated),
    }
}

/// `KEY=value,KEY="quoted, value"` attribute lists.
fn attr<'a>(list: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = list;
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let (value, next) = match after.strip_prefix('"') {
            Some(quoted) => {
                let (v, n) = quoted.split_once('"')?;
                (v, n.strip_prefix(',').unwrap_or(n))
            }
            None => after.split_once(',').unwrap_or((after, "")),
        };
        if key.trim() == name {
            return Some(value);
        }
        rest = next;
    }
    None
}

fn parse_iv(hex: &str) -> Option<[u8; 16]> {
    let hex = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X"))?;
    u128::from_str_radix(hex, 16).ok().map(u128::to_be_bytes)
}

async fn load_state(path: &Path) -> Option<Resume> {
    serde_json::from_slice(&fs::read(path).await.ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        "https://cdn.io/v/master.m3u8".parse().unwrap()
    }

    fn master(text: &str) -> Vec<Variant> {
        match parse(&base(), text).unwrap() {
            Playlist::Master(v) => v,
            Playlist::Media { .. } => panic!("expected master"),
        }
    }

    #[test]
    fn orders_variants_best_first() {
        let v = master(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=5000000,CODECS=\"avc1,mp4a\",RESOLUTION=1920x1080\nhd/high.m3u8\n",
        );
        assert_eq!(v[0].url.as_str(), "https://cdn.io/v/hd/high.m3u8");
        assert_eq!((v[0].height, v[1].height), (Some(1080), Some(360)));
    }

    #[test]
    fn links_audio_renditions_preferring_default() {
        let v = master(
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",URI=\"en.m3u8\"\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"fr\",DEFAULT=YES,URI=\"fr.m3u8\"\n\
             #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"sub\",URI=\"subs.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=900,AUDIO=\"aud\",RESOLUTION=1280x720\nv720.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=500\nmuxed.m3u8\n",
        );
        assert_eq!(v[0].audio.as_ref().unwrap().as_str(), "https://cdn.io/v/fr.m3u8");
        assert!(v[1].audio.is_none());
    }

    /// Regression (Apple's `bipbop_16x9`): the DEFAULT audio rendition has no URI — its sound is in
    /// the variant — and an alternate one has; RDM took the alternate and refused the stream.
    #[test]
    fn a_default_rendition_without_uri_is_the_variants_own_sound() {
        let v = master(
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Main\",AUTOSELECT=YES,DEFAULT=YES\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Alt\",DEFAULT=NO,URI=\"alt/prog.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=900,CODECS=\"mp4a.40.2,avc1.4d400d\",AUDIO=\"a\"\ngear1/prog.m3u8\n",
        );
        assert!(v[0].audio.is_none(), "{:?}", v[0].audio);
    }

    #[test]
    fn parses_segments_keys_and_map() {
        let text = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-MAP:URI=\"init.mp4\"\n\
                    #EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\",IV=0x0000000000000000000000000000000A\n\
                    #EXTINF:4,\na.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\nb.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media { parts, fmp4 } = parse(&base(), text).unwrap() else { panic!("expected media") };
        assert!(fmp4);
        assert_eq!(parts.len(), 3);
        assert!(parts[0].key.is_none());
        assert_eq!(parts[1].key.as_ref().unwrap().iv.unwrap()[15], 10);
        assert_eq!((parts[1].seq, parts[2].seq), (7, 8));
        assert!(parts[2].key.is_none());
    }

    #[test]
    fn byte_ranges_follow_each_other() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"main.mp4\",BYTERANGE=\"616@0\"\n\
                    #EXTINF:6,\n#EXT-X-BYTERANGE:1000@616\nmain.mp4\n\
                    #EXTINF:6,\n#EXT-X-BYTERANGE:500\nmain.mp4\n";
        let Playlist::Media { parts, .. } = parse(&base(), text).unwrap() else { panic!("expected media") };
        let ranges: Vec<_> = parts.iter().map(|p| p.range).collect();
        assert_eq!(ranges, [Some((0, 616)), Some((616, 1000)), Some((1616, 500))]);
        let orphan = "#EXTM3U\n#EXTINF:6,\n#EXT-X-BYTERANGE:500\nmain.mp4\n";
        assert!(parse(&base(), orphan).is_err());
        assert!(parse_byterange("0@5").is_none());
        assert!(parse_byterange("18446744073709551615@1").is_none());
        let overflow = "#EXTM3U\n#EXT-X-MAP:URI=\"main.mp4\",BYTERANGE=\"616@18446744073709551615\"\n#EXTINF:6,\nmain.mp4\n";
        assert!(parse(&base(), overflow).is_err(), "an init segment past the end of any file");
    }

    #[test]
    fn rejects_drm_and_garbage() {
        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:4,\na.ts\n";
        assert!(parse(&base(), drm).is_err());
        assert!(parse(&base(), "<html>").is_err());
        assert!(parse(&base(), "#EXTM3U\n").is_err());
        assert!(attr("A=\"unterminated", "A").is_none());
    }
}
