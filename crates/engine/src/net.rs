//! SSRF guard: content from the Internet (redirects, playlist entries) must not reach the local network.

use std::net::{Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

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

    #[test]
    fn blocks_only_internet_to_lan() {
        assert!(!allowed_hop(&u("https://evil.example/a.m3u8"), &u("http://192.168.1.1/admin")));
        assert!(allowed_hop(&u("http://192.168.1.10/a.m3u8"), &u("http://192.168.1.10/seg.ts")));
        assert!(allowed_hop(&u("https://cdn.example/a"), &u("https://other.example/b")));
    }
}
