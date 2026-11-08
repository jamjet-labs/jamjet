//! The egress guard for Agent Card discovery (`POST /agents/discover`).
//!
//! The URL comes from the caller, and the runtime fetches it. Without a guard
//! a developer token could point the runtime at an internal address
//! (loopback, the private network, link-local cloud metadata) and read the
//! answer back, or probe ports through the error text. So:
//!
//! * the URL must be `https` with no userinfo, and its host must not be, or
//!   obviously name, a non-public address;
//! * the host is resolved once, every address must be public, and the client
//!   is pinned to the checked address with redirects off, so DNS cannot be
//!   swapped between the check and the request and a 3xx cannot bounce it
//!   inward;
//! * the card is read with a size cap and a timeout;
//! * the caller gets a generic error; the detail goes to the log.
//!
//! [`ALLOW_PRIVATE_ENV`] lifts the address and scheme rules for local
//! development (an agent on `http://localhost`). Redirects stay off and the
//! limits stay on. The address checks follow jamjet-cloud's outbound guard.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::Url;

/// Set to `1` (or `true`, `yes`, `on`) to let discovery reach private,
/// loopback and link-local addresses, and plain `http`. For local
/// development only.
pub const ALLOW_PRIVATE_ENV: &str = "JAMJET_DISCOVERY_ALLOW_PRIVATE";

/// The largest Agent Card discovery reads.
pub const MAX_CARD_BYTES: usize = 256 * 1024;

/// How long one discovery fetch may take, end to end.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// What a caller is told when discovery fails. Deliberately the same for a
/// refused connection, a timeout, an HTTP error and an unreadable card, so
/// the route cannot be used to map internal ports.
pub const FETCH_FAILED: &str = "could not fetch an Agent Card from that URL";

/// Why discovery refused or failed, as the caller sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    /// The URL is not one discovery may fetch. Says why: it is about the
    /// caller's own input, not about the network.
    UrlRefused(String),
    /// Anything that went wrong fetching or reading the card: always
    /// [`FETCH_FAILED`].
    FetchFailed,
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UrlRefused(why) => write!(f, "agent discovery refused the URL: {why}"),
            Self::FetchFailed => f.write_str(FETCH_FAILED),
        }
    }
}

/// Whether [`ALLOW_PRIVATE_ENV`] is on.
pub fn private_allowed() -> bool {
    std::env::var(ALLOW_PRIVATE_ENV)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// True only for addresses on the public internet. Everything else (private,
/// loopback, link-local, CGNAT, unspecified, multicast, documentation,
/// reserved, unique-local, and every IPv6 form that embeds or routes to an
/// IPv4 address) is refused.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let blocked = ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0 // 0.0.0.0/8
        || (a == 100 && (64..=127).contains(&b)) // 100.64.0.0/10 CGNAT
        || (a == 192 && b == 0 && c == 0) // 192.0.0.0/24
        || (a == 198 && (b == 18 || b == 19)) // 198.18.0.0/15 benchmarking
        || a >= 240; // 240.0.0.0/4 reserved
    !blocked
}

/// IPv6 is checked as an allowlist: only global unicast (2000::/3) can be
/// public. That alone refuses ::/8 (loopback, unspecified, IPv4-compatible,
/// IPv4-mapped and IPv4-translated), 64:ff9b::/96 and 64:ff9b:1::/48 (NAT64),
/// 100::/64, fc00::/7, fe80::/10, fec0::/10 and ff00::/8. Inside 2000::/3 the
/// special-purpose blocks are refused too.
fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    let global_unicast = (s[0] & 0xe000) == 0x2000; // 2000::/3
    let special = (s[0] == 0x2001 && s[1] < 0x0200) // 2001::/23, incl. Teredo
        || (s[0] == 0x2001 && s[1] == 0x0db8) // 2001:db8::/32 documentation
        || s[0] == 0x2002 // 2002::/16 6to4, embeds an IPv4 address
        || (s[0] & 0xfff0) == 0x3ff0; // 3fff::/20 documentation
    global_unicast && !special
}

/// Host names that name the local machine or an internal zone, or have no
/// dot (resolved by a search domain).
fn is_public_name(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    let internal = h == "localhost"
        || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
        || h == "metadata.google.internal";
    !h.is_empty() && h.contains('.') && !internal
}

/// The Agent Card URL for `base`: `base` itself when it already ends in
/// `/.well-known/agent.json`, else that path appended. Checked before
/// anything is fetched.
pub fn card_url(base: &str, allow_private: bool) -> Result<Url, DiscoveryError> {
    let refused = |why: &str| DiscoveryError::UrlRefused(why.to_string());
    let base = base.trim();
    let raw = if base.ends_with("/.well-known/agent.json") {
        base.to_string()
    } else {
        format!("{}/.well-known/agent.json", base.trim_end_matches('/'))
    };
    let url = Url::parse(&raw).map_err(|_| refused("not a valid URL"))?;
    match url.scheme() {
        "https" => {}
        "http" if allow_private => {}
        _ => return Err(refused("the URL must use https")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refused("the URL must not contain credentials"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| refused("the URL has no host"))?;
    if !allow_private {
        // IPv6 literals come back bracketed ("[::1]").
        let public = match host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        {
            Ok(ip) => is_public_ip(ip),
            Err(_) => is_public_name(host),
        };
        if !public {
            return Err(refused("the host is not a public address"));
        }
    }
    Ok(url)
}

/// Fetch the Agent Card at `url` (already checked by [`card_url`]): resolve
/// the host, require every address to be public (unless `allow_private`),
/// pin the connection to the first one, refuse redirects, and read at most
/// [`MAX_CARD_BYTES`] within [`FETCH_TIMEOUT`].
pub async fn fetch_card(url: &Url, allow_private: bool) -> Result<Vec<u8>, DiscoveryError> {
    let host = url
        .host_str()
        .ok_or_else(|| DiscoveryError::UrlRefused("the URL has no host".into()))?
        .to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let lookup_host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((lookup_host.as_str(), port)).await {
        Ok(a) => a.collect(),
        Err(e) => {
            tracing::warn!(%host, "agent discovery: cannot resolve: {e}");
            return Err(DiscoveryError::FetchFailed);
        }
    };
    if addrs.is_empty() {
        tracing::warn!(%host, "agent discovery: the host resolved to nothing");
        return Err(DiscoveryError::FetchFailed);
    }
    if !allow_private {
        if let Some(bad) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
            tracing::warn!(%host, address = %bad.ip(), "agent discovery: refused a non-public address");
            return Err(DiscoveryError::UrlRefused(
                "the host resolves to a non-public address".into(),
            ));
        }
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, addrs[0])
        .connect_timeout(FETCH_TIMEOUT)
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| {
            tracing::warn!("agent discovery: cannot build an HTTP client: {e}");
            DiscoveryError::FetchFailed
        })?;
    let mut resp = client.get(url.clone()).send().await.map_err(|e| {
        tracing::warn!(%url, "agent discovery: request failed: {e}");
        DiscoveryError::FetchFailed
    })?;
    if !resp.status().is_success() {
        tracing::warn!(%url, status = %resp.status(), "agent discovery: not a success");
        return Err(DiscoveryError::FetchFailed);
    }
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_CARD_BYTES as u64)
    {
        tracing::warn!(%url, "agent discovery: the card is larger than the cap");
        return Err(DiscoveryError::FetchFailed);
    }
    let mut body = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > MAX_CARD_BYTES {
                    tracing::warn!(%url, "agent discovery: the card is larger than the cap");
                    return Err(DiscoveryError::FetchFailed);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(e) => {
                tracing::warn!(%url, "agent discovery: reading the card failed: {e}");
                return Err(DiscoveryError::FetchFailed);
            }
        }
    }
}

/// [`card_url`], [`fetch_card`] and a parse: the Agent Card at `base`.
pub async fn discover_card(
    base: &str,
    allow_private: bool,
) -> Result<crate::card::AgentCard, DiscoveryError> {
    let url = card_url(base, allow_private)?;
    let bytes = fetch_card(&url, allow_private).await?;
    serde_json::from_slice(&bytes).map_err(|e| {
        tracing::warn!(%url, "agent discovery: the card does not parse: {e}");
        DiscoveryError::FetchFailed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_special_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "192.0.0.1",
            "198.18.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2002:a9fe:a9fe::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn card_url_refuses_what_it_must_not_fetch() {
        for bad in [
            "http://agents.example.com",
            "https://127.0.0.1",
            "https://[::1]:8443",
            "https://169.254.169.254/latest",
            "https://localhost",
            "https://metadata.google.internal",
            "https://printer.local",
            "https://intranet",
            "https://user:pass@agents.example.com",
            "ftp://agents.example.com",
            "not a url",
        ] {
            assert!(
                matches!(card_url(bad, false), Err(DiscoveryError::UrlRefused(_))),
                "{bad}"
            );
        }
        assert_eq!(
            card_url("https://agents.example.com/", false)
                .unwrap()
                .as_str(),
            "https://agents.example.com/.well-known/agent.json"
        );
        assert_eq!(
            card_url("https://agents.example.com/.well-known/agent.json", false)
                .unwrap()
                .as_str(),
            "https://agents.example.com/.well-known/agent.json"
        );
    }

    #[test]
    fn the_opt_in_allows_local_development_urls() {
        assert!(card_url("http://127.0.0.1:8080", true).is_ok());
        assert!(card_url("http://localhost:8080", true).is_ok());
        // Still no credentials, and still http or https only.
        assert!(card_url("http://u:p@localhost:8080", true).is_err());
        assert!(card_url("file:///etc/passwd", true).is_err());
    }
}
