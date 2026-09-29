//! Network helpers: the SSRF guard (content from the Internet — redirects, playlist entries — must
//! not reach the local network), credentials kept within their site, and a shared DNS cache.

use std::{
    borrow::Cow,
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    header::{self, HeaderMap},
};
use url::{Host, Url};

/// How long a resolved name is reused: short enough to follow CDN changes (their TTLs are about a
/// minute), long enough that a download's many connections and retries cost one lookup.
const DNS_TTL: Duration = Duration::from_secs(60);
const DNS_MAX_ENTRIES: usize = 512;

/// When a name was resolved, and to what.
type Entry = (Instant, Arc<[SocketAddr]>);

/// A DNS cache shared by every client of the engine. The system resolver answers; failures are not
/// cached (the next connection asks again).
#[derive(Default)]
pub struct CachedDns {
    entries: Mutex<HashMap<String, Entry>>,
}

/// The cached addresses, handed out without copying them.
struct Shared {
    addrs: Arc<[SocketAddr]>,
    next: usize,
}

impl Iterator for Shared {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<SocketAddr> {
        let addr = self.addrs.get(self.next).copied();
        self.next += 1;
        addr
    }
}

impl CachedDns {
    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<CachedDns>> = OnceLock::new();
        SHARED.get_or_init(Arc::default).clone()
    }

    fn fresh(&self, host: &str) -> Option<Arc<[SocketAddr]>> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.get(host).filter(|(at, _)| at.elapsed() < DNS_TTL).map(|(_, addrs)| addrs.clone())
    }

    fn forget(&self, host: &str) {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner).remove(&host.to_ascii_lowercase());
    }

    fn store(&self, host: String, addrs: Arc<[SocketAddr]>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= DNS_MAX_ENTRIES {
            entries.retain(|_, (at, _)| at.elapsed() < DNS_TTL);
            if entries.len() >= DNS_MAX_ENTRIES {
                entries.clear();
            }
        }
        entries.insert(host, (Instant::now(), addrs));
    }
}

impl Resolve for CachedDns {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_ascii_lowercase();
        if let Some(addrs) = self.fresh(&host) {
            return Box::pin(std::future::ready(Ok(Box::new(Shared { addrs, next: 0 }) as Addrs)));
        }
        let cache = Self::shared();
        Box::pin(async move {
            let addrs: Arc<[SocketAddr]> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if addrs.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no address").into());
            }
            cache.store(host, addrs.clone());
            Ok(Box::new(Shared { addrs, next: 0 }) as Addrs)
        })
    }
}

/// The shared cache, keeping only addresses on the Internet: the resolver of a download that starts
/// on the Internet. Whatever a redirect or a playlist names — and whatever its DNS answers, now or
/// later (DNS rebinding) — never leads into the local network: literal addresses are checked by
/// the redirect policy and the playlist reader, names here, at the address actually connected to.
#[derive(Default)]
pub struct PublicDns;

/// Why a name was not resolved: all its addresses are on the local network.
#[derive(Debug)]
pub struct LanBlocked(String);

impl std::fmt::Display for LanBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: local network address refused for Internet content", self.0)
    }
}

impl std::error::Error for LanBlocked {}

impl Resolve for PublicDns {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let all = CachedDns::shared().resolve(name);
        Box::pin(async move {
            let public: Vec<SocketAddr> = all.await?.filter(|a| !local_ip(a.ip())).collect();
            if public.is_empty() {
                return Err(Box::new(LanBlocked(host)) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

/// Whether `url` is on the local network (a NAS, a router, this computer): by its address, or by
/// the addresses its name resolves to. A download from there may stay there; one from the Internet
/// gets [`PublicDns`].
pub async fn reaches_lan(url: &Url) -> bool {
    if is_local(url) {
        return true;
    }
    let Some(Host::Domain(host)) = url.host() else { return false };
    let Ok(name) = host.parse::<Name>() else { return false };
    // Through the cache: the download's first connection reuses the answer.
    CachedDns::shared().resolve(name).await.is_ok_and(|mut addrs| addrs.any(|a| local_ip(a.ip())))
}

/// Whether an error comes from [`PublicDns`] refusing a local address.
pub fn is_lan_blocked(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(err);
    while let Some(e) = source {
        if e.is::<LanBlocked>() {
            return true;
        }
        source = e.source();
    }
    false
}

fn local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => local_v4(v4),
        IpAddr::V6(v6) => local_v6(v6),
    }
}

/// Drops the cached addresses of `url`'s host (connecting to them failed).
pub fn forget(url: &Url) {
    if let Some(Host::Domain(host)) = url.host() {
        CachedDns::shared().forget(host);
    }
}

/// `true` for loopback, private, link-local, CGNAT, unspecified and `.localhost` / `.local` hosts.
/// Literal hosts only: names are not resolved here (no DNS round-trip on the hot path).
pub fn is_local(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => local_v4(ip),
        Some(Host::Ipv6(ip)) => local_v6(ip),
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost") || d.ends_with(".local") || d.ends_with(".internal")
        }
        None => true,
    }
}

fn local_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || a == 0 // 0.0.0.0/8 "this network": 0.0.0.0 reaches this computer on Linux
        || a >= 224 // multicast (224.0.0.0/4), reserved (240.0.0.0/4) and the broadcast address
        || (a == 100 && (64..128).contains(&b)) // 100.64.0.0/10 carrier-grade NAT
        || (a == 198 && (b & 0xfe) == 18) // 198.18.0.0/15 benchmarking, used inside some routers
        || (a, b, c) == (192, 0, 0) // 192.0.0.0/24 protocol assignments (DS-Lite, NAT64 discovery)
}

fn local_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped().or_else(|| embedded_v4(ip)) {
        return local_v4(v4);
    }
    let first = ip.segments()[0];
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (first & 0xfe00) == 0xfc00 // unique local
        || (first & 0xffc0) == 0xfe80 // link-local
        || (first & 0xffc0) == 0xfec0 // site-local (deprecated, still routed by some systems)
}

/// The IPv4 address an IPv6 one carries, where the network translates it to IPv4: NAT64
/// (`64:ff9b::/96`, `64:ff9b:1::/48`), 6to4 (`2002::/16`) and the old IPv4-compatible `::a.b.c.d`.
/// `64:ff9b::192.168.1.1` is the router on a NAT64 network: judged as 192.168.1.1.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let low = || Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7]));
    match s {
        [0x64, 0xff9b, 0, 0, 0, 0, ..] | [0x64, 0xff9b, 1, ..] => Some(low()),
        [0x2002, hi, lo, ..] => Some(Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo))),
        [0, 0, 0, 0, 0, 0, ..] if !ip.is_loopback() && !ip.is_unspecified() => Some(low()),
        _ => None,
    }
}

/// A hop from `from` to `to` is allowed unless it moves from the Internet into the local network.
pub fn allowed_hop(from: &Url, to: &Url) -> bool {
    is_local(from) || !is_local(to)
}

/// Whether two URLs belong to the same site, as browsers scope cookies: the same host, or two
/// hosts under the same registrable domain — approximated by its last two labels (three under a
/// country's second level, `co.uk`: stricter when in doubt). An IP address: that address only.
pub fn same_site(a: &Url, b: &Url) -> bool {
    match (a.host(), b.host()) {
        (Some(Host::Domain(x)), Some(Host::Domain(y))) => site(x) == site(y),
        (Some(Host::Ipv4(x)), Some(Host::Ipv4(y))) => x == y,
        (Some(Host::Ipv6(x)), Some(Host::Ipv6(y))) => x == y,
        _ => false,
    }
}

fn site(host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let keep = match labels.as_slice() {
        [.., second, tld] if tld.len() == 2 && second.len() <= 3 => 3,
        _ => 2,
    };
    labels[labels.len().saturating_sub(keep)..].join(".")
}

/// The request headers fit for `to` when they were given for `from`: the credentials (cookies, a
/// login) stay within `from`'s site. A playlist may point anywhere: its segments on another site
/// must not receive them — what browsers do, and what the HTTP client does on redirects.
pub fn headers_for<'h>(headers: &'h HeaderMap, from: &Url, to: &Url) -> Cow<'h, HeaderMap> {
    let credentials = [header::COOKIE, header::AUTHORIZATION, header::PROXY_AUTHORIZATION];
    if same_site(from, to) || !credentials.iter().any(|h| headers.contains_key(h)) {
        return Cow::Borrowed(headers);
    }
    let mut stripped = headers.clone();
    for h in credentials {
        stripped.remove(h);
    }
    Cow::Owned(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        s.parse().unwrap()
    }

    #[test]
    fn classifies_hosts() {
        for local in [
            "http://127.0.0.1/", "http://10.0.0.8/", "http://192.168.1.1/", "http://172.16.0.1/", "http://169.254.1.1/",
            "http://100.64.0.1/", "http://[::1]/", "http://[fd00::1]/", "http://[fe80::1]/", "http://[::ffff:192.168.0.1]/",
            "http://localhost:8080/", "http://printer.local/", "http://0.0.0.0/",
        ] {
            assert!(is_local(&u(local)), "{local}");
        }
        for public in ["https://example.com/", "http://8.8.8.8/", "http://[2001:4860::8888]/", "http://172.32.0.1/"] {
            assert!(!is_local(&u(public)), "{public}");
        }
    }

    /// IPv6 addresses a network translates to IPv4 are judged by the IPv4 address they carry.
    #[test]
    fn ipv4_inside_ipv6_is_judged_as_ipv4() {
        for local in [
            "http://[64:ff9b::192.168.1.1]/", // NAT64: the router
            "http://[64:ff9b:1::a00:1]/",     // local-use NAT64 prefix: 10.0.0.1
            "http://[2002:c0a8:101::1]/",     // 6to4 of 192.168.1.1
            "http://[::127.0.0.1]/",          // IPv4-compatible
            "http://[fec0::1]/",              // site-local
            "http://[ff02::1]/",              // multicast
            "http://239.255.255.250/",        // multicast (SSDP)
            "http://198.18.0.1/",             // benchmarking
            "http://192.0.0.8/",
        ] {
            assert!(is_local(&u(local)), "{local}");
        }
        for public in ["http://[64:ff9b::8.8.8.8]/", "http://[2002:808:808::1]/", "http://198.20.0.1/", "http://192.0.2.1/"] {
            assert!(!is_local(&u(public)), "{public}");
        }
    }

    #[tokio::test]
    async fn dns_answers_are_cached_and_shared() {
        let dns = CachedDns::shared();
        let first: Vec<SocketAddr> = dns.resolve("localhost".parse().unwrap()).await.unwrap().collect();
        assert!(!first.is_empty());
        assert!(dns.fresh("localhost").is_some(), "kept for the next connections");
        let again: Vec<SocketAddr> = dns.resolve("LOCALHOST".parse().unwrap()).await.unwrap().collect();
        assert_eq!(first, again, "case-insensitive");
        forget(&u("http://LocalHost:8080/x"));
        assert!(dns.fresh("localhost").is_none(), "forgotten after a failed connection");
    }

    #[test]
    fn sites_as_cookies_see_them() {
        let same = |a: &str, b: &str| same_site(&u(a), &u(b));
        assert!(same("https://www.example.com/a.m3u8", "https://cdn.example.com/s.ts"));
        assert!(same("https://example.com/", "https://EXAMPLE.com.:8443/x"));
        assert!(same("https://www.bbc.co.uk/", "https://media.bbc.co.uk/"));
        assert!(!same("https://www.bbc.co.uk/", "https://evil.co.uk/"), "a country's second level is not a site");
        assert!(!same("https://example.com/", "https://example.com.evil.io/"));
        assert!(!same("https://example.com/", "https://evil-example.com/"));
        assert!(same("http://10.0.0.2/a", "http://10.0.0.2:8080/b"));
        assert!(!same("http://10.0.0.2/", "http://10.0.0.3/"));
        assert!(!same("http://127.0.0.1/", "http://localhost/"));
    }

    #[test]
    fn credentials_stay_on_their_site() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "sid=1".parse().unwrap());
        h.insert(header::AUTHORIZATION, "Basic dTpw".parse().unwrap());
        h.insert(header::REFERER, "https://www.example.com/".parse().unwrap());
        let from = u("https://www.example.com/master.m3u8");
        assert!(matches!(headers_for(&h, &from, &u("https://cdn.example.com/seg.ts")), Cow::Borrowed(_)));
        let foreign = headers_for(&h, &from, &u("https://tracker.example.net/seg.ts"));
        assert!(!foreign.contains_key(header::COOKIE) && !foreign.contains_key(header::AUTHORIZATION));
        assert!(foreign.contains_key(header::REFERER), "only credentials are dropped");
    }

    #[test]
    fn blocks_only_internet_to_lan() {
        assert!(!allowed_hop(&u("https://evil.example/a.m3u8"), &u("http://192.168.1.1/admin")));
        assert!(allowed_hop(&u("http://192.168.1.10/a.m3u8"), &u("http://192.168.1.10/seg.ts")));
        assert!(allowed_hop(&u("https://cdn.example/a"), &u("https://other.example/b")));
    }
}
