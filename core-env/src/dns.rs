use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Known secure DNS providers with direct IPv4 addresses (immune to local DNS poisoning).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecureDnsProvider {
    Cloudflare,
    Google,
    Quad9,
}

impl SecureDnsProvider {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Cloudflare => "Cloudflare (1.1.1.1)",
            Self::Google => "Google (8.8.8.8)",
            Self::Quad9 => "Quad9 (9.9.9.9)",
        }
    }

    pub fn direct_doh_url(&self, domain: &str) -> String {
        match self {
            // Use direct IP addresses so DoH requests don't depend on system DNS
            Self::Cloudflare => format!("https://1.1.1.1/dns-query?name={domain}&type=A"),
            Self::Google => format!("https://8.8.8.8/resolve?name={domain}&type=A"),
            Self::Quad9 => format!("https://9.9.9.9:5053/dns-query?name={domain}&type=A"),
        }
    }
}

/// Checks if an IP is a "Bogo-IP" returned by hostile or hijacking resolvers.
/// Covers loopback, RFC 1918 private spaces, CGNAT (100.64.0.0/10), link-local, and unspecified.
pub fn is_bogo_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            let octets = ipv4.octets();
            // 0.0.0.0/8
            octets[0] == 0
                // 127.0.0.0/8 (Loopback, e.g. 127.9.9.9 by Telefonica)
                || octets[0] == 127
                // 10.0.0.0/8 (Private RFC 1918)
                || octets[0] == 10
                // 172.16.0.0/12 (Private RFC 1918)
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                // 192.168.0.0/16 (Private RFC 1918)
                || (octets[0] == 192 && octets[1] == 168)
                // 100.64.0.0/10 (Shared Address Space / CGNAT RFC 6598)
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                // 169.254.0.0/16 (Link-local RFC 3927)
                || (octets[0] == 169 && octets[1] == 254)
                // Broadcast
                || ipv4.is_broadcast()
        }
        IpAddr::V6(ipv6) => ipv6.is_loopback() || ipv6.is_unspecified(),
    }
}

/// In-memory cache for DoH resolutions to avoid querying DoH on every HTTP call.
#[derive(Clone, Default)]
pub struct DnsCache {
    entries: Arc<RwLock<HashMap<String, (Vec<IpAddr>, Instant)>>>,
}

impl DnsCache {
    pub fn get(&self, domain: &str) -> Option<Vec<IpAddr>> {
        let entries = self.entries.read().ok()?;
        let (ips, expires_at) = entries.get(domain)?;
        if Instant::now() < *expires_at {
            Some(ips.clone())
        } else {
            None
        }
    }

    pub fn insert(&self, domain: String, ips: Vec<IpAddr>, ttl: Duration) {
        if let Ok(mut entries) = self.entries.write() {
            entries.insert(domain, (ips, Instant::now() + ttl));
        }
    }
}

/// Resolves a domain using direct DNS-over-HTTPS (DoH).
pub async fn resolve_via_doh(
    client: &reqwest::Client,
    domain: &str,
    provider: SecureDnsProvider,
) -> Result<Vec<IpAddr>, String> {
    let url = provider.direct_doh_url(domain);
    let mut req = client
        .get(&url)
        .header("Accept", "application/dns-json")
        .timeout(Duration::from_secs(4));

    if provider == SecureDnsProvider::Cloudflare {
        req = req.header("Host", "cloudflare-dns.com");
    } else if provider == SecureDnsProvider::Google {
        req = req.header("Host", "dns.google");
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("DoH HTTP request failed: {e}"))?;

    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read DoH response body: {e}"))?;

    #[derive(serde::Deserialize)]
    struct DohAnswer {
        r#type: u16,
        data: String,
    }

    #[derive(serde::Deserialize)]
    struct DohResponse {
        #[serde(rename = "Status")]
        status: u32,
        #[serde(rename = "Answer", default)]
        answer: Vec<DohAnswer>,
    }

    let parsed: DohResponse = serde_json::from_str(&text)
        .map_err(|e| format!("Failed to parse DoH JSON response: {e}"))?;

    if parsed.status != 0 {
        return Err(format!("DoH returned error status: {}", parsed.status));
    }

    let mut ips = Vec::new();
    for ans in parsed.answer {
        // Type 1 is A record (IPv4)
        if ans.r#type == 1 {
            if let Ok(v4) = ans.data.trim().parse::<Ipv4Addr>() {
                ips.push(IpAddr::V4(v4));
            }
        }
    }

    if ips.is_empty() {
        return Err("DoH returned no A records".to_string());
    }

    Ok(ips)
}
