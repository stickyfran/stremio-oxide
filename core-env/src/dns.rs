use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, Instant};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Selectable DNS providers for the application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsProvider {
    /// Cloudflare DNS (`1.1.1.1` / `1.0.0.1` + DoH). Default.
    Cloudflare,
    /// Google Public DNS (`8.8.8.8` / `8.8.4.4` + DoH).
    Google,
    /// Quad9 DNS (`9.9.9.9` / `149.112.112.112` + DoH).
    Quad9,
    /// AdGuard DNS (`94.140.14.14` / `94.140.15.15`).
    AdGuard,
    /// System Default DNS (with automatic DoH fallback on hijacked/bogo IPs).
    System,
    /// Custom user-specified DNS server IP (`IP[:port]`) or DoH URL (`https://...`).
    Custom(String),
}

impl DnsProvider {
    pub fn from_config(provider_id: &str, custom_server: &str) -> Self {
        match provider_id.trim().to_ascii_lowercase().as_str() {
            "cloudflare" | "cloudflare (1.1.1.1)" | "1.1.1.1" | "" => Self::Cloudflare,
            "google" | "google (8.8.8.8)" | "8.8.8.8" => Self::Google,
            "quad9" | "quad9 (9.9.9.9)" | "9.9.9.9" => Self::Quad9,
            "adguard" | "adguard (94.140.14.14)" | "94.140.14.14" => Self::AdGuard,
            "system" | "sistema (por defecto)" | "system (default)" => Self::System,
            "custom" | "personalizado" => Self::Custom(custom_server.trim().to_owned()),
            other => {
                if !custom_server.trim().is_empty() && other.starts_with("custom") {
                    Self::Custom(custom_server.trim().to_owned())
                } else {
                    Self::Cloudflare
                }
            }
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            Self::Cloudflare => "cloudflare",
            Self::Google => "google",
            Self::Quad9 => "quad9",
            Self::AdGuard => "adguard",
            Self::System => "system",
            Self::Custom(_) => "custom",
        }
    }

    pub fn display_label(&self) -> &'static str {
        match self {
            Self::Cloudflare => "Cloudflare (1.1.1.1)",
            Self::Google => "Google (8.8.8.8)",
            Self::Quad9 => "Quad9 (9.9.9.9)",
            Self::AdGuard => "AdGuard (94.140.14.14)",
            Self::System => "Sistema (Por defecto)",
            Self::Custom(_) => "Personalizado",
        }
    }

    pub fn udp_servers(&self) -> Vec<SocketAddr> {
        match self {
            Self::Cloudflare => vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 0, 0, 1)), 53),
            ],
            Self::Google => vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 53),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4)), 53),
            ],
            Self::Quad9 => vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)), 53),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(149, 112, 112, 112)), 53),
            ],
            Self::AdGuard => vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(94, 140, 14, 14)), 53),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(94, 140, 15, 15)), 53),
            ],
            Self::System => Vec::new(),
            Self::Custom(raw) => parse_custom_udp_servers(raw),
        }
    }
}

fn parse_custom_udp_servers(raw: &str) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for part in raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        let trimmed = part.trim();
        if trimmed.is_empty() || trimmed.starts_with("https://") || trimmed.starts_with("http://") {
            continue;
        }
        if let Ok(sock) = trimmed.parse::<SocketAddr>() {
            out.push(sock);
        } else if let Ok(ip) = trimmed.parse::<IpAddr>() {
            out.push(SocketAddr::new(ip, 53));
        }
    }
    out
}

/// Checks if an IP is a "Bogo-IP" returned by hostile or hijacking resolvers.
pub fn is_bogo_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            let octets = ipv4.octets();
            octets[0] == 0
                || octets[0] == 127
                || octets[0] == 10
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 168)
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 169 && octets[1] == 254)
                || ipv4.is_broadcast()
        }
        IpAddr::V6(ipv6) => ipv6.is_loopback() || ipv6.is_unspecified(),
    }
}

/// In-memory cache for DNS resolutions to avoid querying on every HTTP call.
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

    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.clear();
        }
    }
}

static ACTIVE_DNS_PROVIDER: LazyLock<RwLock<DnsProvider>> =
    LazyLock::new(|| RwLock::new(DnsProvider::Cloudflare));
static GLOBAL_DNS_CACHE: LazyLock<DnsCache> = LazyLock::new(DnsCache::default);
static GLOBAL_RESOLVER: LazyLock<Arc<ConfigurableDnsResolver>> =
    LazyLock::new(|| Arc::new(ConfigurableDnsResolver::new(GLOBAL_DNS_CACHE.clone())));

/// Updates the active DNS provider and clears the resolution cache so new
/// connections immediately use the selected DNS server.
pub fn configure_dns(provider_id: &str, custom_server: &str) {
    let next = DnsProvider::from_config(provider_id, custom_server);
    tracing::info!(
        provider = next.id(),
        label = next.display_label(),
        custom = custom_server,
        "Updated active DNS resolver configuration"
    );
    if let Ok(mut guard) = ACTIVE_DNS_PROVIDER.write() {
        *guard = next;
    }
    GLOBAL_DNS_CACHE.clear();
}

pub fn active_dns_provider() -> DnsProvider {
    ACTIVE_DNS_PROVIDER
        .read()
        .map(|g| g.clone())
        .unwrap_or(DnsProvider::Cloudflare)
}

pub fn shared_dns_resolver() -> Arc<ConfigurableDnsResolver> {
    GLOBAL_RESOLVER.clone()
}

/// Builds a DNS query packet (RFC 1035) for the given domain and record type (`1` = A, `28` = AAAA).
fn build_dns_query_packet(domain: &str, qtype: u16, tx_id: u16) -> Result<Vec<u8>, String> {
    let mut packet = Vec::with_capacity(64);
    packet.extend_from_slice(&tx_id.to_be_bytes());
    // Flags: Standard query with Recursion Desired (0x0100)
    packet.extend_from_slice(&0x0100u16.to_be_bytes());
    // QDCOUNT = 1, ANCOUNT = 0, NSCOUNT = 0, ARCOUNT = 0
    packet.extend_from_slice(&1u16.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());

    let trimmed = domain.trim_end_matches('.');
    for label in trimmed.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return Err(format!("invalid DNS label in {domain}"));
        }
        packet.push(bytes.len() as u8);
        packet.extend_from_slice(bytes);
    }
    packet.push(0); // Root terminator

    packet.extend_from_slice(&qtype.to_be_bytes());
    packet.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN (1)
    Ok(packet)
}

fn skip_dns_name(buf: &[u8], mut offset: usize) -> Option<usize> {
    let mut jumps = 0;
    loop {
        if offset >= buf.len() || jumps > 16 {
            return None;
        }
        let len = buf[offset];
        if len == 0 {
            return Some(offset + 1);
        }
        if (len & 0xC0) == 0xC0 {
            if offset + 1 >= buf.len() {
                return None;
            }
            return Some(offset + 2);
        }
        offset += 1 + (len as usize);
        jumps += 1;
    }
}

fn parse_dns_response_packet(buf: &[u8], expected_tx_id: u16) -> Result<(Vec<IpAddr>, u32), String> {
    if buf.len() < 12 {
        return Err("DNS response too short".to_owned());
    }
    let tx_id = u16::from_be_bytes([buf[0], buf[1]]);
    if tx_id != expected_tx_id {
        return Err("DNS transaction ID mismatch".to_owned());
    }
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    let rcode = flags & 0x000F;
    if rcode != 0 {
        return Err(format!("DNS server returned RCODE {rcode}"));
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;

    let mut offset = 12;
    for _ in 0..qdcount {
        offset = skip_dns_name(buf, offset).ok_or_else(|| "malformed DNS question name".to_owned())?;
        if offset + 4 > buf.len() {
            return Err("truncated DNS question section".to_owned());
        }
        offset += 4;
    }

    let mut ips = Vec::new();
    let mut min_ttl = 300u32;

    for _ in 0..ancount {
        offset = skip_dns_name(buf, offset).ok_or_else(|| "malformed DNS answer name".to_owned())?;
        if offset + 10 > buf.len() {
            return Err("truncated DNS answer header".to_owned());
        }
        let rtype = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
        let rclass = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]);
        let ttl = u32::from_be_bytes([
            buf[offset + 4],
            buf[offset + 5],
            buf[offset + 6],
            buf[offset + 7],
        ]);
        let rdlength = u16::from_be_bytes([buf[offset + 8], buf[offset + 9]]) as usize;
        offset += 10;

        if offset + rdlength > buf.len() {
            return Err("truncated DNS answer rdata".to_owned());
        }

        if rclass == 1 {
            if rtype == 1 && rdlength == 4 {
                let ip = Ipv4Addr::new(
                    buf[offset],
                    buf[offset + 1],
                    buf[offset + 2],
                    buf[offset + 3],
                );
                let addr = IpAddr::V4(ip);
                if !is_bogo_ip(&addr) {
                    ips.push(addr);
                    min_ttl = min_ttl.min(ttl.clamp(60, 3600));
                }
            } else if rtype == 28 && rdlength == 16 {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&buf[offset..offset + 16]);
                let ip = Ipv6Addr::from(octets);
                let addr = IpAddr::V6(ip);
                if !is_bogo_ip(&addr) {
                    ips.push(addr);
                    min_ttl = min_ttl.min(ttl.clamp(60, 3600));
                }
            }
        }
        offset += rdlength;
    }

    if ips.is_empty() {
        return Err("no usable A/AAAA records in DNS response".to_owned());
    }

    Ok((ips, min_ttl))
}

async fn resolve_via_udp(server: SocketAddr, domain: &str) -> Result<(Vec<IpAddr>, u32), String> {
    let bind_addr: SocketAddr = if server.is_ipv4() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
    };

    let socket = tokio::net::UdpSocket::bind(bind_addr)
        .await
        .map_err(|e| format!("UDP bind failed: {e}"))?;

    let tx_id = (Instant::now().elapsed().subsec_nanos() as u16) ^ 0x5A3C;
    let query = build_dns_query_packet(domain, 1, tx_id)?;

    socket
        .send_to(&query, server)
        .await
        .map_err(|e| format!("UDP send to {server} failed: {e}"))?;

    let mut buf = [0u8; 1024];
    let (len, _) = tokio::time::timeout(Duration::from_millis(1800), socket.recv_from(&mut buf))
        .await
        .map_err(|_| format!("UDP DNS timeout from {server}"))?
        .map_err(|e| format!("UDP recv from {server} failed: {e}"))?;

    parse_dns_response_packet(&buf[..len], tx_id)
}

/// Resolves a domain using direct DNS-over-HTTPS (DoH) JSON API.
pub async fn resolve_via_doh(
    client: &reqwest::Client,
    domain: &str,
    provider: &DnsProvider,
) -> Result<Vec<IpAddr>, String> {
    let (url, host_header) = match provider {
        DnsProvider::Cloudflare | DnsProvider::System => (
            format!("https://1.1.1.1/dns-query?name={domain}&type=A"),
            Some("cloudflare-dns.com"),
        ),
        DnsProvider::Google => (
            format!("https://8.8.8.8/resolve?name={domain}&type=A"),
            Some("dns.google"),
        ),
        DnsProvider::Quad9 => (
            format!("https://9.9.9.9:5053/dns-query?name={domain}&type=A"),
            None,
        ),
        DnsProvider::AdGuard => (
            format!("https://1.1.1.1/dns-query?name={domain}&type=A"),
            Some("cloudflare-dns.com"),
        ),
        DnsProvider::Custom(custom) => {
            let trimmed = custom.trim();
            if trimmed.starts_with("https://") {
                let sep = if trimmed.contains('?') { '&' } else { '?' };
                (format!("{trimmed}{sep}name={domain}&type=A"), None)
            } else {
                (
                    format!("https://1.1.1.1/dns-query?name={domain}&type=A"),
                    Some("cloudflare-dns.com"),
                )
            }
        }
    };

    let mut req = client
        .get(&url)
        .header("Accept", "application/dns-json")
        .timeout(Duration::from_secs(3));

    if let Some(h) = host_header {
        req = req.header("Host", h);
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
        if ans.r#type == 1 {
            if let Ok(v4) = ans.data.trim().parse::<Ipv4Addr>() {
                let ip = IpAddr::V4(v4);
                if !is_bogo_ip(&ip) {
                    ips.push(ip);
                }
            }
        }
    }

    if ips.is_empty() {
        return Err("DoH returned no A records".to_string());
    }

    Ok(ips)
}

#[derive(Clone)]
pub struct ConfigurableDnsResolver {
    cache: DnsCache,
    doh_client: reqwest::Client,
}

impl ConfigurableDnsResolver {
    pub fn new(cache: DnsCache) -> Self {
        let doh_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(4))
            .connect_timeout(Duration::from_secs(3))
            .build()
            .unwrap_or_default();
        Self { cache, doh_client }
    }
}

impl Resolve for ConfigurableDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let cache = self.cache.clone();
        let doh_client = self.doh_client.clone();
        Box::pin(async move {
            match resolve_host(cache, doh_client, &host).await {
                Ok(addrs) => Ok(addrs),
                Err(error) => Err(
                    Box::new(std::io::Error::new(std::io::ErrorKind::Other, error))
                        as Box<dyn std::error::Error + Send + Sync>,
                ),
            }
        })
    }
}

async fn resolve_host(
    cache: DnsCache,
    doh_client: reqwest::Client,
    host: &str,
) -> Result<Addrs, String> {
    // IP literals and local-only names must never be sent to an external resolver.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(Box::new(std::iter::once(SocketAddr::new(ip, 0))));
    }
    let is_local_name = host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".local")
        || host.ends_with(".localdomain")
        || host.ends_with(".lan");
    if is_local_name {
        return Ok(Box::new(lookup_system_host(host).await?.into_iter()));
    }

    if let Some(ips) = cache.get(host) {
        return Ok(Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0))));
    }

    let provider = active_dns_provider();

    // 1. If the user selected a specific DNS provider (Cloudflare by default, Google, Quad9, AdGuard, or Custom):
    if provider != DnsProvider::System {
        for server in provider.udp_servers() {
            if let Ok((ips, ttl)) = resolve_via_udp(server, host).await {
                cache.insert(host.to_owned(), ips.clone(), Duration::from_secs(ttl as u64));
                return Ok(Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0))));
            }
        }

        // If UDP 53 is blocked or custom is a DoH URL, try DoH:
        if let Ok(ips) = resolve_via_doh(&doh_client, host, &provider).await {
            cache.insert(host.to_owned(), ips.clone(), Duration::from_secs(300));
            return Ok(Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0))));
        }
    }

    // 2. System DNS lookup (primary when System is selected, fallback otherwise)
    let (system_addrs, system_error) = match lookup_system_host(host).await {
        Ok(addrs) => (addrs, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let usable: Vec<SocketAddr> = system_addrs
        .iter()
        .cloned()
        .filter(|addr| !is_bogo_ip(&addr.ip()))
        .collect();
    if !usable.is_empty() {
        let ips: Vec<IpAddr> = usable.iter().map(|a| a.ip()).collect();
        cache.insert(host.to_owned(), ips, Duration::from_secs(180));
        return Ok(Box::new(usable.into_iter()));
    }

    // 3. Last-resort DoH bypass when System DNS failed or returned a hijacked/bogo IP
    for fallback in [
        DnsProvider::Cloudflare,
        DnsProvider::Google,
        DnsProvider::Quad9,
    ] {
        if let Ok(ips) = resolve_via_doh(&doh_client, host, &fallback).await {
            cache.insert(host.to_owned(), ips.clone(), Duration::from_secs(300));
            return Ok(Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0))));
        }
    }

    if !system_addrs.is_empty() {
        return Ok(Box::new(system_addrs.into_iter()));
    }

    Err(system_error.unwrap_or_else(|| format!("DNS resolution failed for {host}")))
}

async fn lookup_system_host(host: &str) -> Result<Vec<SocketAddr>, String> {
    let addrs = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|error| format!("system DNS lookup failed for {host}: {error}"))?
        .collect::<Vec<_>>();
    Ok(addrs)
}
