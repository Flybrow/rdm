use reqwest::{Client, StatusCode, header::{self, HeaderMap}};
use url::Url;

use crate::EngineError;

#[derive(Debug, Clone)]
pub struct Probe {
    pub size: Option<u64>,
    pub ranges: bool,
    pub file_name: String,
    /// HLS playlist: downloaded segment by segment (see [`crate::hls_info`] for the container).
    pub hls: bool,
    /// What identifies this version of the file on the server (`Last-Modified`, else a strong
    /// `ETag`): a download resumed later must continue the same file, not a newer one.
    pub version: Option<String>,
}

/// The file's version as the server states it. `Last-Modified` first: it stays the same across
/// the servers of a CDN or a mirror pool, where an `ETag` often differs from one to the next
/// (Apache puts the inode in it). A weak `ETag` (`W/…`) says nothing about the bytes, and neither
/// does a date that is the moment of the answer: download scripts stamp every response "now",
/// which would make every resume start over.
fn version_of(headers: &HeaderMap) -> Option<String> {
    let text = |name| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let answered = text(header::DATE).and_then(|d| httpdate::parse_http_date(d).ok());
    let stamped_now = |modified: &str| match (httpdate::parse_http_date(modified), answered) {
        // Within a minute of the answer (or after it): not a file's date.
        (Ok(modified), Some(answered)) => answered.duration_since(modified).unwrap_or_default() < std::time::Duration::from_secs(60),
        _ => false,
    };
    text(header::LAST_MODIFIED)
        .filter(|date| !stamped_now(date))
        .map(|date| format!("date:{date}"))
        .or_else(|| text(header::ETAG).filter(|tag| !tag.starts_with("W/")).map(|tag| format!("etag:{tag}")))
}

/// Attempts for the initial request: transient failures (timeout, reset, 5xx, 429) are retried.
const PROBE_ATTEMPTS: u32 = 5;

/// One `Range: bytes=0-0` request reveals size, range support and file name at once.
pub async fn probe(client: &Client, url: &Url, headers: &HeaderMap) -> Result<Probe, EngineError> {
    let mut attempt = 1;
    loop {
        match try_probe(client, url, headers).await {
            Err(e) if !e.is_permanent() && attempt < PROBE_ATTEMPTS => attempt += 1,
            other => return other,
        }
        tokio::time::sleep(std::time::Duration::from_millis(500 << attempt)).await;
    }
}

/// A single attempt, for when an answer is only nice to have (naming a new download quickly).
pub async fn probe_once(client: &Client, url: &Url, headers: &HeaderMap) -> Result<Probe, EngineError> {
    try_probe(client, url, headers).await
}

async fn try_probe(client: &Client, url: &Url, headers: &HeaderMap) -> Result<Probe, EngineError> {
    let res = client
        .get(url.clone())
        .headers(headers.clone())
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await?
        .error_for_status()?;

    let h = res.headers();
    let ranges = res.status() == StatusCode::PARTIAL_CONTENT;
    let size = if ranges {
        h.get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse().ok())
    } else {
        res.content_length()
    };
    let version = version_of(h);
    let disposition = h.get(header::CONTENT_DISPOSITION).and_then(|v| v.to_str().ok());
    let content_type = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let hls = crate::hls::is_playlist(res.url(), content_type);
    let mut file_name = suggest_file_name(res.url(), disposition);
    if hls {
        let stem = file_name.rsplit_once('.').map_or(file_name.as_str(), |(s, _)| s);
        file_name = format!("{stem}.ts");
    }
    // Drain the 1-byte body so this warm (TLS-established) connection returns to the pool and
    // the first worker reuses it. Never for a 200: that body would be the whole file.
    if ranges {
        let _ = res.bytes().await;
    }

    Ok(Probe { size, ranges: ranges && size.is_some() && !hls, file_name, hls, version })
}

pub fn suggest_file_name(url: &Url, disposition: Option<&str>) -> String {
    disposition
        .and_then(parse_disposition)
        .or_else(|| url.path_segments()?.next_back().filter(|s| !s.is_empty()).map(percent_decode))
        .map(|n| sanitize_file_name(&n))
        .unwrap_or_else(|| "download".into())
}

/// RFC 6266: `filename*` (RFC 8187 `charset'lang'%XX…`) wins over `filename`, in any order.
fn parse_disposition(v: &str) -> Option<String> {
    let params = disposition_params(v);
    let get = |key: &str| params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    get("filename*").and_then(decode_ext_value).or_else(|| get("filename").map(str::to_owned)).filter(|n| !n.is_empty())
}

/// `type; key=token; key="quoted; \"value\""` → (lower-case key, value) pairs. A `;` inside quotes
/// is part of the value; parameter names are case-insensitive.
fn disposition_params(v: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    let Some((_, mut rest)) = v.split_once(';') else { return params };
    loop {
        rest = rest.trim_start_matches([' ', '\t', ';']);
        let Some((key, after)) = rest.split_once('=') else { return params };
        let after = after.trim_start();
        let (value, next) = match after.strip_prefix('"') {
            Some(quoted) => {
                let (mut value, mut escaped, mut end) = (String::new(), false, quoted.len());
                for (i, c) in quoted.char_indices() {
                    match c {
                        _ if escaped => {
                            value.push(c);
                            escaped = false;
                        }
                        '\\' => escaped = true,
                        '"' => {
                            end = i + 1;
                            break;
                        }
                        _ => value.push(c),
                    }
                }
                (value, &quoted[end..])
            }
            None => {
                let (value, next) = after.split_once(';').unwrap_or((after, ""));
                (value.trim().to_owned(), next)
            }
        };
        params.push((key.trim().to_ascii_lowercase(), value));
        rest = next;
    }
}

/// `UTF-8''%E2%82%AC.pdf` (or ISO-8859-1) → text.
fn decode_ext_value(v: &str) -> Option<String> {
    let mut parts = v.splitn(3, '\'');
    let (charset, _lang, value) = (parts.next()?, parts.next()?, parts.next()?);
    let bytes: Vec<u8> = percent_encoding::percent_decode_str(value).collect();
    Some(if charset.eq_ignore_ascii_case("iso-8859-1") {
        bytes.iter().map(|&b| char::from(b)).collect()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

const MAX_NAME_CHARS: usize = 180;

/// Untrusted name (server, web page) → a single safe path component on Windows and Linux:
/// no separators or traversal, no reserved device names, no text-direction tricks, bounded
/// length, extension kept.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            let unsafe_char = matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() || is_bidi_control(c);
            if unsafe_char { '_' } else { c }
        })
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.']);

    let (stem, ext) = match cleaned.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && e.chars().count() <= 16 => (s, Some(e)),
        _ => (cleaned, None),
    };
    let budget = MAX_NAME_CHARS.saturating_sub(ext.map_or(0, |e| e.chars().count() + 1));
    let mut stem: String = stem.chars().take(budget).collect::<String>().trim_end().to_owned();

    // Windows ignores trailing spaces here: "NUL .txt" is the NUL device too.
    let device = stem.split('.').next().unwrap_or_default().trim_end().to_uppercase();
    let numbered = |prefix: &str| {
        device.strip_prefix(prefix).is_some_and(|n| {
            let mut digits = n.chars();
            matches!((digits.next(), digits.next()), (Some('0'..='9' | '¹' | '²' | '³'), None))
        })
    };
    let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT");
    if reserved {
        stem.insert(0, '_');
    }
    match (stem.is_empty(), ext) {
        (true, _) => "download".into(),
        (false, Some(ext)) => format!("{stem}.{ext}"),
        (false, None) => stem,
    }
}

/// Characters that reorder the text around them: "invoice\u{202E}fdp.exe" reads "invoiceexe.pdf".
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let u: Url = "https://x.io/dir/My%20File.zip?x=1".parse().unwrap();
        assert_eq!(suggest_file_name(&u, None), "My File.zip");
        assert_eq!(suggest_file_name(&u, Some("attachment; filename=\"a:b.iso\"")), "a_b.iso");
        assert_eq!(suggest_file_name(&u, Some("attachment; filename*=UTF-8''%C3%A9t%C3%A9.mp4")), "été.mp4");
    }

    #[test]
    fn a_files_version() {
        let headers = |pairs: &[(header::HeaderName, &str)]| pairs.iter().map(|(k, v)| (k.clone(), v.parse().unwrap())).collect::<HeaderMap>();
        let date = "Tue, 21 Oct 2025 07:28:00 GMT";
        assert_eq!(version_of(&headers(&[(header::ETAG, "\"abc\""), (header::LAST_MODIFIED, date)])), Some(format!("date:{date}")));
        assert_eq!(version_of(&headers(&[(header::ETAG, "\"abc\"")])).as_deref(), Some("etag:\"abc\""));
        assert_eq!(version_of(&headers(&[(header::ETAG, "W/\"abc\"")])), None, "a weak tag says nothing");
        assert_eq!(version_of(&HeaderMap::new()), None);
        // A script stamping each answer "now": the date is not the file's.
        let now = "Mon, 28 Sep 2026 12:00:00 GMT";
        assert_eq!(version_of(&headers(&[(header::DATE, now), (header::LAST_MODIFIED, now)])), None);
        assert_eq!(version_of(&headers(&[(header::DATE, now), (header::LAST_MODIFIED, now), (header::ETAG, "\"v2\"")])).as_deref(), Some("etag:\"v2\""));
        let older = format!("date:{date}");
        assert_eq!(version_of(&headers(&[(header::DATE, now), (header::LAST_MODIFIED, date)])), Some(older), "a real, older date");
    }

    #[test]
    fn content_disposition_edge_cases() {
        let name = |d: &str| parse_disposition(d);
        assert_eq!(name(r#"attachment; filename="a;b.zip""#).as_deref(), Some("a;b.zip"), "';' inside quotes");
        assert_eq!(name("attachment; Filename=Report.pdf").as_deref(), Some("Report.pdf"), "case-insensitive key");
        assert_eq!(name(r#"attachment; filename="say \"hi\".txt"; size=3"#).as_deref(), Some(r#"say "hi".txt"#));
        assert_eq!(name("attachment; filename*=UTF-8''%E2%82%AC.pdf; filename=\"EUR.pdf\"").as_deref(), Some("€.pdf"));
        assert_eq!(name("attachment; filename=\"EUR.pdf\"; filename*=utf-8'en'%E2%82%AC.pdf").as_deref(), Some("€.pdf"));
        assert_eq!(name("attachment; filename*=ISO-8859-1''%E9t%E9.txt").as_deref(), Some("été.txt"));
        assert_eq!(name(r#"attachment; filename="unterminated.zip"#).as_deref(), Some("unterminated.zip"));
        assert_eq!(name("inline"), None);
        assert_eq!(name("attachment; filename=\"\""), None);
    }

    #[test]
    fn sanitizes_hostile_names() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_file_name("..\\..\\Windows\\x.exe"), "_.._Windows_x.exe");
        assert_eq!(sanitize_file_name("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_file_name("com1"), "_com1");
        assert_eq!(sanitize_file_name("NUL .txt"), "_NUL.txt");
        assert_eq!(sanitize_file_name("nul .tar.gz"), "_nul .tar.gz");
        assert_eq!(sanitize_file_name("COM¹.log"), "_COM¹.log");
        assert_eq!(sanitize_file_name("conout$"), "_conout$");
        assert_eq!(sanitize_file_name("COM10.txt"), "COM10.txt");
        assert_eq!(sanitize_file_name("Console.txt"), "Console.txt");
        assert_eq!(sanitize_file_name("invoice\u{202E}fdp.exe"), "invoice_fdp.exe", "right-to-left override");
        assert_eq!(sanitize_file_name("a\u{2067}b\u{200F}.zip"), "a_b_.zip");
        assert_eq!(sanitize_file_name(" ... "), "download");
        let long = format!("{}.mp4", "a".repeat(500));
        let safe = sanitize_file_name(&long);
        assert!(safe.chars().count() <= MAX_NAME_CHARS && safe.ends_with(".mp4"));
    }
}
