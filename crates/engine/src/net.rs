//! Network helpers: the SSRF guard (content from the Internet — redirects, playlist entries — must
//! not reach the local network) and a shared DNS cache.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
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
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || (a == 100 && (64..128).contains(&b)) // 100.64.0.0/10 carrier-grade NAT
}

fn local_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return local_v4(v4);
    }
    let first = ip.segments()[0];
    ip.is_loopback() || ip.is_unspecified() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
}

/// A hop from `from` to `to` is allowed unless it moves from the Internet into the local network.
pub fn allowed_hop(from: &Url, to: &Url) -> bool {
    is_local(from) || !is_local(to)
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
    fn blocks_only_internet_to_lan() {
        assert!(!allowed_hop(&u("https://evil.example/a.m3u8"), &u("http://192.168.1.1/admin")));
        assert!(allowed_hop(&u("http://192.168.1.10/a.m3u8"), &u("http://192.168.1.10/seg.ts")));
        assert!(allowed_hop(&u("https://cdn.example/a"), &u("https://other.example/b")));
    }
}
