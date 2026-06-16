//! Native Tailscale support: discover tailnet peers (MagicDNS names + tailnet
//! IPs) from `tailscale status --json`, for shell completion (`posh tailnet`,
//! see completions.rs) and as a hostname-resolution fallback for the roaming
//! transport (remote/client.rs).
//!
//! Everything degrades silently: no `tailscale` binary, not logged in, or
//! unparseable output yields an empty result — never an error or panic. A
//! tailnet name otherwise completes and connects like an ssh_config alias.

use std::net::IpAddr;
use std::process::Command;

use poshterity::json::{self, Value};

/// A tailnet node (self or a peer): the names it answers to and its tailnet IPs.
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub names: Vec<String>,
    pub ips: Vec<IpAddr>,
}

/// All tailnet peer names — short MagicDNS labels and FQDNs — de-duplicated and
/// sorted. Empty when Tailscale is unavailable. This is what `posh tailnet`
/// prints and the completion scripts source.
pub fn names() -> Vec<String> {
    let Some(json) = status_json() else {
        return Vec::new();
    };
    let mut out: Vec<String> = parse_status(&json)
        .into_iter()
        .flat_map(|p| p.names)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Resolve `host` to a tailnet IP by matching it case-insensitively against any
/// peer's names (short label or FQDN). Prefers IPv4. `None` if nothing matches
/// or Tailscale is unavailable. Used only as a fallback when the system
/// resolver cannot reach a MagicDNS name.
pub fn resolve(host: &str) -> Option<IpAddr> {
    let json = status_json()?;
    resolve_among(&parse_status(&json), host)
}

fn resolve_among(peers: &[Peer], host: &str) -> Option<IpAddr> {
    let want = host.trim_end_matches('.').to_ascii_lowercase();
    peers
        .iter()
        .find(|p| p.names.iter().any(|n| n.eq_ignore_ascii_case(&want)))
        .and_then(|p| {
            // Prefer IPv4 (the common roaming path), else the first address.
            p.ips
                .iter()
                .find(|ip| ip.is_ipv4())
                .or_else(|| p.ips.first())
                .copied()
        })
}

/// Run `tailscale status --json`; `None` on any failure (binary missing,
/// non-zero exit — e.g. not logged in — or non-UTF-8 output).
fn status_json() -> Option<String> {
    let out = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Parse `tailscale status --json` into nodes (`Self` plus each `Peer`). Pure
/// and tolerant: missing keys or shape surprises are skipped, never fatal.
pub fn parse_status(json: &str) -> Vec<Peer> {
    let Ok(root) = json::parse(json) else {
        return Vec::new();
    };
    let mut peers = Vec::new();
    if let Some(node) = root.get("Self") {
        if let Some(p) = node_to_peer(node) {
            peers.push(p);
        }
    }
    // `Peer` is an object keyed by node id; each value is a node object.
    if let Some(Value::Obj(entries)) = root.get("Peer") {
        for (_, node) in entries {
            if let Some(p) = node_to_peer(node) {
                peers.push(p);
            }
        }
    }
    peers
}

fn node_to_peer(node: &Value) -> Option<Peer> {
    let mut names = Vec::new();
    if let Some(dns) = node.get("DNSName").and_then(Value::as_str) {
        let fqdn = dns.trim_end_matches('.');
        if !fqdn.is_empty() {
            names.push(fqdn.to_string());
            // The short MagicDNS label (the bit before the first dot).
            if let Some((short, _)) = fqdn.split_once('.') {
                names.push(short.to_string());
            }
        }
    }
    if let Some(host) = node.get("HostName").and_then(Value::as_str) {
        if !host.is_empty() {
            names.push(host.to_string());
        }
    }
    let mut ips = Vec::new();
    if let Some(Value::Arr(arr)) = node.get("TailscaleIPs") {
        for v in arr {
            if let Some(ip) = v.as_str().and_then(|s| s.parse::<IpAddr>().ok()) {
                ips.push(ip);
            }
        }
    }
    names.sort();
    names.dedup();
    if names.is_empty() && ips.is_empty() {
        return None;
    }
    Some(Peer { names, ips })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A representative `tailscale status --json` (trimmed to the fields posh
    // reads): a Self node and two peers.
    const SAMPLE: &str = r#"{
        "Self": {
            "HostName": "mylaptop",
            "DNSName": "mylaptop.tail1234.ts.net.",
            "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"]
        },
        "Peer": {
            "nodekey:aaa": {
                "HostName": "server",
                "DNSName": "server.tail1234.ts.net.",
                "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"]
            },
            "nodekey:bbb": {
                "HostName": "nas",
                "DNSName": "nas.tail1234.ts.net.",
                "TailscaleIPs": ["100.64.0.3"]
            }
        }
    }"#;

    #[test]
    fn parses_self_and_peers_with_names_and_ips() {
        let peers = parse_status(SAMPLE);
        assert_eq!(peers.len(), 3);
        // Each node carries its short label + FQDN (+ HostName, deduped).
        let server = peers
            .iter()
            .find(|p| p.names.iter().any(|n| n == "server"))
            .unwrap();
        assert!(server.names.contains(&"server.tail1234.ts.net".to_string()));
        assert_eq!(server.ips[0], "100.64.0.2".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn names_are_sorted_deduped_and_include_short_and_fqdn() {
        let peers = parse_status(SAMPLE);
        let names: Vec<String> = peers.into_iter().flat_map(|p| p.names).collect();
        assert!(names.contains(&"server".to_string()));
        assert!(names.contains(&"nas".to_string()));
        assert!(names.contains(&"mylaptop.tail1234.ts.net".to_string()));
    }

    #[test]
    fn resolve_matches_short_and_fqdn_case_insensitively_prefers_ipv4() {
        let peers = parse_status(SAMPLE);
        assert_eq!(
            resolve_among(&peers, "server"),
            Some("100.64.0.2".parse().unwrap())
        );
        assert_eq!(
            resolve_among(&peers, "SERVER.tail1234.ts.net"),
            Some("100.64.0.2".parse().unwrap())
        );
        // Trailing dot tolerated.
        assert_eq!(
            resolve_among(&peers, "nas."),
            Some("100.64.0.3".parse().unwrap())
        );
        assert_eq!(resolve_among(&peers, "ghost"), None);
    }

    #[test]
    fn malformed_or_empty_json_yields_no_peers() {
        assert!(parse_status("not json").is_empty());
        assert!(parse_status("{}").is_empty());
        assert!(parse_status("").is_empty());
        assert_eq!(resolve_among(&parse_status("garbage"), "server"), None);
    }
}
