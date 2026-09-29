//! How downloads reach their servers: the proxy settings turned into engine routes, one HTTP
//! client per route (kept and reused: warm connections, TLS sessions), and site logins.

use std::{sync::Arc, time::Duration};

use engine::{Client, ClientOptions, HeaderValue, Route, header};
use url::Url;

use super::{Manager, lock};
use crate::{settings::ProxyMode, tr, trf};

/// What "Test the proxy" fetches: GitHub's API root, already the one server RDM talks to.
const PROBE: &str = "https://api.github.com/";

impl Manager {
    /// The route a download takes. `via_proxy`: in automatic mode, this download was switched to
    /// the proxy (slow, or unreachable directly).
    pub(super) fn route(&self, via_proxy: bool) -> Route {
        let proxy = self.with_settings(|s| s.proxy.clone());
        let manual = || {
            if proxy.url.is_empty() {
                Route::Direct
            } else {
                let password = lock(&self.secrets).proxy_password.clone();
                Route::Proxy { url: normalize_proxy(&proxy.url), user: proxy.user.clone(), password }
            }
        };
        match proxy.mode {
            ProxyMode::Off => Route::Direct,
            ProxyMode::System => system_route(),
            ProxyMode::Manual => manual(),
            ProxyMode::Auto if via_proxy => manual(),
            ProxyMode::Auto => Route::Direct,
        }
    }

    /// Whether downloads switch to the proxy when slow (automatic mode with a proxy set).
    pub(super) fn auto_proxy(&self) -> bool {
        self.with_settings(|s| s.proxy.mode == ProxyMode::Auto && !s.proxy.url.is_empty())
    }

    /// The client for a download of `url`: its route, and — when it starts on the Internet and names
    /// are resolved here — no way into the local network through a name (see `engine::net`).
    pub(super) async fn client_for_url(&self, url: &Url, via_proxy: bool, insecure: bool) -> Result<Client, String> {
        let route = self.route(via_proxy);
        let public_only = resolves_here(&route) && !engine::net::reaches_lan(url).await;
        self.client_for(route, insecure, public_only)
    }

    /// The client for a route (built once, then shared). An unusable proxy address is an error.
    pub(super) fn client_for(&self, route: Route, insecure: bool, public_only: bool) -> Result<Client, String> {
        let options = ClientOptions { route, insecure, public_only };
        let mut clients = lock(&self.clients);
        if let Some(client) = clients.get(&options) {
            return Ok(client.clone());
        }
        let invalid = || tr!("adresse de proxy invalide (voir les paramètres)", "invalid proxy address (see the settings)").to_owned();
        if let Route::Proxy { url, .. } = &options.route
            && !supported_proxy(url)
        {
            return Err(invalid());
        }
        let client = engine::client_with(&options).map_err(|_| invalid())?;
        clients.insert(options, client.clone());
        Ok(client)
    }

    /// The client for requests on the browser's behalf about `url` (quality lists, link checks).
    pub async fn client(&self, url: &Url) -> Client {
        match self.client_for_url(url, false, false).await {
            Ok(client) => client,
            Err(_) => {
                let public_only = !engine::net::reaches_lan(url).await;
                self.client_for(Route::Direct, false, public_only).unwrap_or_else(|_| self.fallback.clone())
            }
        }
    }

    /// Settings changed: clients are rebuilt on next use (downloads running keep theirs).
    pub(super) fn forget_clients(&self) {
        lock(&self.clients).clear();
    }

    /// Adds the saved login for `url`'s site (HTTP Basic), unless the request carries one already
    /// (in its headers or in the address itself). Over plain HTTP the password would travel in
    /// clear: only for a site saved explicitly as `http://…` (a NAS, a box on the local network).
    pub(super) fn add_login(&self, url: &Url, headers: &mut engine::HeaderMap) {
        if headers.contains_key(header::AUTHORIZATION) || !url.username().is_empty() {
            return;
        }
        let secrets = lock(&self.secrets);
        let Some(login) = secrets.login_for(url) else { return };
        if url.scheme() != "https" && !login.host.trim().to_ascii_lowercase().starts_with("http://") {
            return;
        }
        let token = base64(format!("{}:{}", login.user, login.password).as_bytes());
        if let Ok(mut value) = HeaderValue::from_str(&format!("Basic {token}")) {
            value.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, value);
        }
    }

    /// "Test": reaches a server through the proxy of the settings and says how it went (a notice).
    pub fn test_proxy(self: &Arc<Self>) {
        let proxy = self.with_settings(|s| s.proxy.clone());
        let route = match proxy.mode {
            ProxyMode::Off => return self.notice(false, tr!("Aucun proxy : connexions directes.", "No proxy: direct connections.")),
            ProxyMode::System => system_route(),
            ProxyMode::Manual | ProxyMode::Auto if proxy.url.is_empty() => {
                return self.notice(true, tr!("Indiquez l'adresse du proxy.", "Enter the proxy's address."));
            }
            ProxyMode::Manual | ProxyMode::Auto => self.route(true),
        };
        let this = self.clone();
        self.rt.spawn(async move {
            let usable = !matches!(&route, Route::Proxy { url, .. } if !supported_proxy(url));
            let Some(client) = engine::client_with(&ClientOptions { route, ..ClientOptions::default() }).ok().filter(|_| usable) else {
                return this.notice(true, tr!("Adresse de proxy invalide.", "Invalid proxy address."));
            };
            let started = std::time::Instant::now();
            let res = client.get(PROBE).header("accept", "application/vnd.github+json").timeout(Duration::from_secs(15)).send().await;
            let ms = started.elapsed().as_millis();
            match res {
                Ok(r) if r.status() == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
                    this.notice(true, tr!("Le proxy refuse l'identifiant ou le mot de passe (407).", "The proxy refuses the login or password (407)."));
                }
                Ok(_) => this.notice(false, &trf!("Le proxy fonctionne ({ms} ms).", "The proxy works ({ms} ms).", ms = ms)),
                Err(e) if e.is_timeout() => this.notice(true, tr!("Le proxy ne répond pas (15 s).", "The proxy does not answer (15 s).")),
                Err(_) => this.notice(true, tr!("Connexion par le proxy impossible.", "Cannot connect through the proxy.")),
            }
        });
    }
}

/// Whether target names are resolved on this computer (so RDM can keep them out of the local
/// network): a direct connection. A proxy resolves them itself — and may be on the local network,
/// by name — the system's proxy included when one is set.
fn resolves_here(route: &Route) -> bool {
    match route {
        Route::Direct => true,
        Route::System => !system_proxy_set(),
        Route::Proxy { .. } => false,
    }
}

/// A proxy the system sets (what the engine follows in `Route::System`): the `*_PROXY` variables,
/// or on Windows the proxy of the Internet settings.
fn system_proxy_set() -> bool {
    let variable = ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()));
    #[cfg(windows)]
    {
        use winreg::{RegKey, enums::HKEY_CURRENT_USER};
        let settings = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");
        let enabled = settings.is_ok_and(|k| k.get_value::<u32, _>("ProxyEnable").is_ok_and(|v| v != 0));
        variable || enabled
    }
    #[cfg(not(windows))]
    variable
}

/// `host:port` alone means an HTTP proxy.
fn normalize_proxy(url: &str) -> String {
    if url.contains("://") { url.to_owned() } else { format!("http://{url}") }
}

/// A proxy address the engine can use: a known scheme and a host.
fn supported_proxy(url: &str) -> bool {
    url.parse::<Url>().is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h") && u.host_str().is_some_and(|h| !h.is_empty())
    })
}

/// The system's proxy. Windows and macOS settings and the `*_PROXY` variables are read by the
/// engine itself; GNOME keeps its setting elsewhere (gsettings), read here when no variable is set.
fn system_route() -> Route {
    #[cfg(target_os = "linux")]
    {
        let set = |v: &str| std::env::var_os(v).is_some_and(|s| !s.is_empty());
        let env = ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"].iter().any(|v| set(v));
        if !env && let Some(url) = gnome_proxy() {
            return Route::Proxy { url, user: String::new(), password: String::new() };
        }
    }
    Route::System
}

/// GNOME's manual proxy (`org.gnome.system.proxy`), cached: asked once per session.
#[cfg(target_os = "linux")]
fn gnome_proxy() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let get = |schema: &str, key: &str| {
                let out = std::process::Command::new("gsettings").args(["get", schema, key]).output().ok()?;
                out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_owned())
            };
            if get("org.gnome.system.proxy", "mode")? != "manual" {
                return None;
            }
            ["https", "http", "socks"].iter().find_map(|kind| {
                let schema = format!("org.gnome.system.proxy.{kind}");
                let host = get(&schema, "host").filter(|h| !h.is_empty())?;
                let port: u16 = get(&schema, "port")?.parse().ok().filter(|p| *p > 0)?;
                let scheme = if *kind == "socks" { "socks5h" } else { "http" };
                Some(format!("{scheme}://{host}:{port}"))
            })
        })
        .clone()
}

/// Standard base64 (RFC 4648) for the Basic header.
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(TABLE[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc() {
        for (plain, encoded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy"), ("user:pässword", "dXNlcjpww6Rzc3dvcmQ=")] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain}");
        }
    }

    #[test]
    fn names_are_kept_out_of_the_lan_only_where_resolved_here() {
        assert!(resolves_here(&Route::Direct));
        assert!(!resolves_here(&Route::Proxy { url: "http://proxy.lan:3128".into(), user: String::new(), password: String::new() }));
    }

    #[test]
    fn a_bare_proxy_address_is_http() {
        assert_eq!(normalize_proxy("10.0.0.1:3128"), "http://10.0.0.1:3128");
        assert_eq!(normalize_proxy("socks5h://proxy:1080"), "socks5h://proxy:1080");
        assert!(supported_proxy("socks5h://proxy:1080") && supported_proxy("http://10.0.0.1:3128"));
        assert!(!supported_proxy("ftp://proxy:21") && !supported_proxy("http://") && !supported_proxy("nonsense"));
    }
}
