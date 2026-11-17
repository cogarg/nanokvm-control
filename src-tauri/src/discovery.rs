//! "Find devices": scans an address range the user enters and recognises the device types the app
//! supports. Each address gets a quick TCP check on a few ports; addresses that answer are then
//! identified with unauthenticated requests (page titles, the Redfish service root, the VNC
//! greeting). Nothing is logged in to and nothing is changed.

use serde::Serialize;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{io::AsyncReadExt, net::TcpStream, sync::Semaphore};

/// Largest scan allowed in one go (a /20).
pub const MAX_ADDRESSES: usize = 4096;
const PORTS: [u16; 5] = [80, 443, 8006, 5900, 5901];
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const PARALLEL: usize = 128;

/// Parses "192.168.1.0/24", "192.168.1.10-192.168.1.50", "192.168.1.10-50" or single addresses,
/// separated by commas, spaces or new lines. Networks leave out their network and broadcast
/// address (except /31 and /32).
pub fn parse_ranges(input: &str) -> Result<Vec<Ipv4Addr>, String> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for part in input.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()) {
        let bad = || format!("\"{part}\" isn't an IPv4 address, network (a.b.c.d/nn) or range (a.b.c.d-e.f.g.h)");
        let (start, end): (u32, u32) = if let Some((ip, bits)) = part.split_once('/') {
            let ip: Ipv4Addr = ip.parse().map_err(|_| bad())?;
            let bits: u32 = bits.parse().map_err(|_| bad())?;
            if bits > 32 {
                return Err(bad());
            }
            if bits < 20 {
                return Err(format!("{part} is too large; scan at most {MAX_ADDRESSES} addresses (a /20) at a time"));
            }
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            let net = u32::from(ip) & mask;
            let last = net | !mask;
            if bits >= 31 { (net, last) } else { (net + 1, last - 1) }
        } else if let Some((a, b)) = part.split_once('-') {
            let a: Ipv4Addr = a.trim().parse().map_err(|_| bad())?;
            // "192.168.1.10-50": the end is the last octet.
            let b: Ipv4Addr = match b.trim().parse::<u8>() {
                Ok(last) => {
                    let o = a.octets();
                    Ipv4Addr::new(o[0], o[1], o[2], last)
                }
                Err(_) => b.trim().parse().map_err(|_| bad())?,
            };
            let (a, b) = (u32::from(a), u32::from(b));
            if b < a {
                return Err(format!("In {part} the end comes before the start"));
            }
            (a, b)
        } else {
            let ip: Ipv4Addr = part.parse().map_err(|_| bad())?;
            (u32::from(ip), u32::from(ip))
        };
        if (end - start) as usize + 1 + out.len() > MAX_ADDRESSES {
            return Err(format!("That's more than {MAX_ADDRESSES} addresses; scan a smaller range"));
        }
        out.extend((start..=end).map(Ipv4Addr::from));
    }
    if out.is_empty() {
        return Err("Enter an address range to scan, e.g. 192.168.1.0/24".into());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// This computer's IPv4 networks (as suggestions for the range), largest first trimmed to /24
/// around this computer's address when they are bigger than a scan allows.
pub fn local_networks() -> Vec<String> {
    let mut nets: Vec<String> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .filter(|v4| !v4.ip.is_link_local())
        .map(|v4| {
            let bits = (v4.prefixlen as u32).max(24).min(32);
            let mask = u32::MAX << (32 - bits);
            format!("{}/{bits}", Ipv4Addr::from(u32::from(v4.ip) & mask))
        })
        .collect();
    nets.sort();
    nets.dedup();
    nets
}

/// A recognised device, with what the Add dialog needs.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub address: String,
    /// Device kind as stored (`nanokvm`, `pikvm`, `supermicro-ikvm`, `idrac`, `vnc`, `proxmox`).
    pub kind: String,
    /// Suggested address for the device (with scheme / port).
    pub url: String,
    /// Human-readable detail, e.g. the page title or VNC version.
    pub detail: String,
}

async fn open(ip: Ipv4Addr, port: u16) -> Option<TcpStream> {
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(SocketAddr::new(IpAddr::V4(ip), port)))
        .await
        .ok()?
        .ok()
}

async fn vnc_banner(ip: Ipv4Addr, port: u16) -> Option<String> {
    let mut s = open(ip, port).await?;
    let mut buf = [0u8; 12];
    tokio::time::timeout(Duration::from_secs(2), s.read_exact(&mut buf)).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&buf).trim().to_string();
    text.starts_with("RFB ").then_some(text)
}

async fn get(http: &reqwest::Client, url: &str) -> Option<String> {
    let resp = http.get(url).send().await.ok()?;
    let text = resp.text().await.ok()?;
    Some(text.chars().take(200_000).collect())
}

fn title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title")?;
    let start = start + lower[start..].find('>')? + 1;
    let end = start + lower[start..].find("</title>")?;
    let t = html[start..end].split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}

/// Identifies what runs at `ip`, given which ports answered.
async fn identify(http: &reqwest::Client, ip: Ipv4Addr, open_ports: &[u16]) -> Option<Found> {
    let has = |p: u16| open_ports.contains(&p);
    let found = |kind: &str, url: String, detail: String| {
        Some(Found { address: ip.to_string(), kind: kind.into(), url, detail })
    };
    if has(8006) {
        if let Some(body) = get(http, &format!("https://{ip}:8006/")).await {
            if body.contains("Proxmox") {
                return found("proxmox", format!("https://{ip}:8006"), title(&body).unwrap_or("Proxmox VE".into()));
            }
        }
    }
    if has(443) || has(80) {
        let base = if has(443) { format!("https://{ip}") } else { format!("http://{ip}") };
        // BMCs with Redfish name their vendor in the (unauthenticated) service root.
        if let Some(body) = get(http, &format!("{base}/redfish/v1")).await {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                let vendor = json.get("Vendor").and_then(|v| v.as_str()).unwrap_or_default();
                let dell = vendor.eq_ignore_ascii_case("dell") || json.pointer("/Oem/Dell").is_some();
                if dell {
                    let product = json.get("Product").and_then(|v| v.as_str()).unwrap_or("iDRAC");
                    let detail = if product.starts_with("Dell") { product.to_string() } else { format!("Dell {product}") };
                    return found("idrac", format!("https://{ip}"), detail);
                }
            }
        }
        for scheme_base in [format!("http://{ip}"), format!("https://{ip}")] {
            if (scheme_base.starts_with("https") && !has(443)) || (scheme_base.starts_with("http:") && !has(80)) {
                continue;
            }
            let Some(body) = get(http, &format!("{scheme_base}/")).await else { continue };
            let t = title(&body).unwrap_or_default();
            if t.contains("NanoKVM") || body.contains("<title>NanoKVM") {
                return found("nanokvm", scheme_base, if t.is_empty() { "NanoKVM".into() } else { t });
            }
            if body.to_lowercase().contains("pikvm") {
                return found("pikvm", format!("https://{ip}"), if t.is_empty() { "PiKVM".into() } else { t });
            }
            // Older Supermicro (and other ATEN) BMCs: their login page carries an ATEN notice.
            if body.contains("ATEN International") {
                return found("supermicro-ikvm", format!("https://{ip}"), "Supermicro / ATEN BMC".into());
            }
        }
    }
    for port in [5900, 5901] {
        if has(port) {
            if let Some(banner) = vnc_banner(ip, port).await {
                let url = if port == 5900 { ip.to_string() } else { format!("{ip}:{port}") };
                return found("vnc", url, format!("VNC server ({banner})"));
            }
        }
    }
    None
}

/// Scans the addresses and returns the recognised devices, sorted by address.
pub async fn scan(addresses: Vec<Ipv4Addr>) -> Vec<Found> {
    let http = match reqwest::Client::builder()
        .use_native_tls() // older BMCs only speak old TLS (see ikvm.rs)
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(0)
        .build()
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let limit = Arc::new(Semaphore::new(PARALLEL));
    let mut tasks = Vec::new();
    for ip in addresses {
        let (limit, http) = (limit.clone(), http.clone());
        tasks.push(tauri::async_runtime::spawn(async move {
            let open_ports = {
                let _permit = limit.acquire().await.ok()?;
                let checks = PORTS.map(|p| async move { open(ip, p).await.map(|_| p) });
                futures_util::future::join_all(checks).await.into_iter().flatten().collect::<Vec<_>>()
            };
            if open_ports.is_empty() {
                return None;
            }
            identify(&http, ip, &open_ports).await
        }));
    }
    let mut found = Vec::new();
    for t in tasks {
        if let Ok(Some(f)) = t.await {
            found.push(f);
        }
    }
    found.sort_by_key(|f| f.address.parse::<Ipv4Addr>().map(u32::from).unwrap_or(0));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_ranges("192.0.2.0/30").unwrap(), [Ipv4Addr::new(192, 0, 2, 1), Ipv4Addr::new(192, 0, 2, 2)]);
        assert_eq!(parse_ranges("192.0.2.10-12").unwrap().len(), 3);
        assert_eq!(parse_ranges("192.0.2.10-192.0.2.11, 192.0.2.11 192.0.2.40").unwrap().len(), 3);
        assert_eq!(parse_ranges("192.0.2.0/24").unwrap().len(), 254);
        assert_eq!(parse_ranges("192.0.2.7/32").unwrap(), [Ipv4Addr::new(192, 0, 2, 7)]);
        assert!(parse_ranges("10.0.0.0/8").unwrap_err().contains("too large"));
        assert!(parse_ranges("192.0.2.20-10").is_err());
        assert!(parse_ranges("example.com").is_err());
        assert!(parse_ranges("").is_err());
    }

    #[test]
    fn finds_titles() {
        assert_eq!(title("<html><head><TITLE> NanoKVM </title>").as_deref(), Some("NanoKVM"));
        assert_eq!(title("<p>none</p>"), None);
    }
}
