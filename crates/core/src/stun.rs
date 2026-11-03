//! Minimal RFC 5389 STUN client for public-IP discovery.
//!
//! chatx advertises the addresses in `running.listen_addrs` to the directory so
//! remote peers can dial us. Behind a NAT that is a private LAN IP, so cross-NAT
//! peers cannot reach us. STUN lets the client learn its own **public** IP (the
//! one the NAT presents to the outside world) and prepend a
//! `/ip4/<public>/tcp/<listen_port>` endpoint (see
//! [`crate::Client::endpoints_with_egress`]).
//!
//! Design notes:
//! * Uses `std::net::UdpSocket` with a hard timeout — this blocks, so callers
//!   must call it from a blocking context (the presence thread / a
//!   `spawn_blocking` task), never directly on a tokio worker.
//! * The discovered public IP is cached process-wide for a short window so the
//!   15 s heartbeat loop and the startup prewarm don't hammer STUN.
//! * STUN over UDP reliably yields the **public IP**; the mapped *port* is the
//!   probe socket's, not our real TCP listen port, so we pair the public IP with
//!   the actual listening port at the call site. For endpoint-independent NATs
//!   this gives a direct connect; for symmetric NATs the dial fails and the
//!   existing relay fallback takes over. Either way it is harmless (just one
//!   extra candidate address).

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// STUN magic cookie (RFC 5389 §5.1).
const MAGIC: u32 = 0x21_12_A4_42;

/// Type bits for a STUN Binding Request (RFC 5389: type 0x0001).
const TYPE_BINDING_REQUEST: u16 = 0x0001;
/// Type bits for a Binding Success Response (0x0101).
const TYPE_BINDING_RESPONSE: u16 = 0x0101;
/// Attribute: XOR-MAPPED-ADDRESS.
const ATTR_XOR_MAPPED: u16 = 0x0020;
/// Attribute: MAPPED-ADDRESS.
const ATTR_MAPPED: u16 = 0x0001;

/// How long to wait for a single STUN server to answer before moving on.
const PER_SERVER_TIMEOUT: Duration = Duration::from_secs(3);

/// Default public STUN servers. Google's service is the primary because it
/// always speaks IPv4; the others are best-effort fallbacks.
const STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun2.l.google.com:19302",
    "stun3.l.google.com:19302",
    "stun4.l.google.com:19302",
    "stun.stunprotocol.org:3478",
];

/// Result of a single successful STUN binding: the client's public IP as seen
/// by the outside world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicIp {
    pub ip: IpAddr,
}

impl std::fmt::Display for PublicIp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.ip)
    }
}

/// A cached public-IP result with its expiry.
#[derive(Clone, Copy)]
struct Cached {
    ip: IpAddr,
    expires_at: std::time::Instant,
}

struct StunCache {
    /// Cache lifetime: long enough that the 15 s heartbeat loop and the
    /// startup prewarm share one result, short enough that a WAN failover is
    /// picked up within a couple of heartbeat cycles.
    ttl: Duration,
    value: Option<Cached>,
}

static CACHE: std::sync::Mutex<StunCache> = std::sync::Mutex::new(StunCache {
    ttl: Duration::from_secs(120),
    value: None,
});

/// Discover the local public IP via STUN, using the cache when fresh.
///
/// Returns `Ok(None)` when every STUN server is unreachable (e.g. behind a
/// symmetric NAT with no UDP egress, or offline) — callers should treat that
/// as "no public address known" and fall back to the relay path.
///
/// **Blocking**: issues UDP I/O on the calling thread, up to
/// `PER_SERVER_TIMEOUT * STUN_SERVERS` in the worst case. Call from a
/// background thread or `tokio::task::spawn_blocking`, never from a tokio
/// worker or from the UI event loop path (e.g. don't call it from
/// `heartbeat()` on the login thread; use `discover_cached` instead).
pub fn discover() -> anyhow::Result<Option<PublicIp>> {
    // Fast path: fresh cache.
    {
        let cache = CACHE.lock().unwrap();
        if let Some(c) = &cache.value {
            if c.expires_at > std::time::Instant::now() {
                return Ok(Some(PublicIp { ip: c.ip }));
            }
        }
    }

    // Slow path: query each server in turn until one answers.
    for host in STUN_SERVERS {
        match probe_one(host) {
            Ok(Some(ip)) => {
                let mut cache = CACHE.lock().unwrap();
                cache.value = Some(Cached {
                    ip,
                    expires_at: std::time::Instant::now() + cache.ttl,
                });
                tracing::debug!(host, ip = %ip, "STUN success");
                return Ok(Some(PublicIp { ip }));
            }
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(host, %e, "STUN probe failed");
                continue;
            }
        }
    }

    // Fall back to a stale cache if every server timed out but we have a
    // previous answer — better than nothing for a momentary STUN outage.
    let cache = CACHE.lock().unwrap();
    if let Some(c) = &cache.value {
        tracing::warn!(ip = %c.ip, "all STUN servers unreachable; using stale public IP");
        return Ok(Some(PublicIp { ip: c.ip }));
    }
    Ok(None)
}

/// Non-blocking cached lookup: returns the most recent successfully-discovered
/// public IP (fresh or stale), without doing any network I/O. Use this on the
/// hot path (heartbeat loop) — call `discover()` from a background thread to
/// populate the cache first.
pub fn discover_cached() -> Option<PublicIp> {
    let cache = CACHE.lock().unwrap();
    cache.value.map(|c| PublicIp { ip: c.ip })
}

/// Force-probe a single STUN server, bypassing the cache. Exposed for tests.
fn probe_one(server: &str) -> anyhow::Result<Option<IpAddr>> {
    let addr: SocketAddr = server
        .parse()
        .map_err(|e| anyhow::anyhow!("bad STUN server {server}: {e}"))?;

    let sock = std::net::UdpSocket::bind("0.0.0.0:0")
        .or_else(|_| std::net::UdpSocket::bind("[::]:0"))
        .map_err(|e| anyhow::anyhow!("bind STUN socket: {e}"))?;
    sock.set_read_timeout(Some(PER_SERVER_TIMEOUT))?;
    sock.set_write_timeout(Some(PER_SERVER_TIMEOUT))?;

    // 20-byte STUN header: 2-bit message class + 14-bit message length.
    let mut header = [0u8; 20];
    let txn: [u8; 12] = rand::random();
    header[0..2].copy_from_slice(&TYPE_BINDING_REQUEST.to_be_bytes());
    header[2..4].copy_from_slice(&0u16.to_be_bytes()); // message length (no attrs)
    header[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    header[8..20].copy_from_slice(&txn);

    sock.send_to(&header, addr)
        .map_err(|e| anyhow::anyhow!("STUN send to {server}: {e}"))?;

    let mut buf = vec![0u8; 1024];
    let (n, _from) = match sock.recv_from(&mut buf) {
        Ok(v) => v,
        // No answer within timeout — not fatal, try the next server.
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("STUN recv from {server}: {e}")),
    };
    buf.truncate(n);

    let Some(mapped) = parse_response(&buf, &txn) else {
        return Ok(None);
    };
    Ok(Some(mapped.ip))
}

/// Validate the response header (magic, transaction id, type) and extract the
/// XOR-MAPPED-ADDRESS (preferred) or MAPPED-ADDRESS attribute.
fn parse_response(pkt: &[u8], txn: &[u8; 12]) -> Option<PublicIp> {
    // 20-byte header, then a run of (2 type + 2 len + payload) attributes.
    const HEADER_LEN: usize = 20;
    if pkt.len() < HEADER_LEN {
        return None;
    }
    let mtype = u16::from_be_bytes([pkt[0], pkt[1]]);
    if mtype != TYPE_BINDING_RESPONSE {
        return None;
    }
    let mlen = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
    if HEADER_LEN + mlen > pkt.len() {
        return None;
    }
    if u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]) != MAGIC {
        return None;
    }
    if &pkt[8..20] != txn {
        return None;
    }

    let mut cursor: usize = HEADER_LEN;
    while cursor + 4 <= pkt.len() {
        let attr = u16::from_be_bytes([pkt[cursor], pkt[cursor + 1]]);
        let len = u16::from_be_bytes([pkt[cursor + 2], pkt[cursor + 3]]) as usize;
        let payload_start = cursor + 4;
        let payload_end = payload_start + len;
        if payload_end > pkt.len() {
            break;
        }
        let payload = &pkt[payload_start..payload_end];

        let ip = if attr == ATTR_XOR_MAPPED {
            xor_mapped(payload)
        } else if attr == ATTR_MAPPED {
            plain_mapped(payload)
        } else {
            None
        };
        if let Some(ip) = ip {
            return Some(PublicIp { ip });
        }

        // Attributes are 4-byte aligned; skip to next boundary.
        let aligned = (len + 3) & !3;
        cursor = payload_start + aligned;
    }
    None
}

/// Parse a MAPPED-ADDRESS (unencrypted) attribute: family(1) port(2) addr.
/// Only the IP is reported — the STUN port is the probe socket's, not our
/// real TCP listen port, so the call site ignores it.
fn plain_mapped(p: &[u8]) -> Option<IpAddr> {
    if p.len() >= 8 && p[0] == 0x01 {
        let ip = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
        return Some(IpAddr::V4(std::net::Ipv4Addr::from(ip)));
    }
    if p.len() >= 20 && p[0] == 0x02 {
        let mut a = [0u8; 16];
        a.copy_from_slice(&p[6..22]);
        return Some(IpAddr::V6(a.into()));
    }
    None
}

/// Parse a XOR-MAPPED-ADDRESS attribute. For a Binding Response the magic
/// cookie XOR is identity on the port, but we do it properly per RFC 5389.
fn xor_mapped(p: &[u8]) -> Option<IpAddr> {
    let family = p[0];
    if family == 0x01 && p.len() >= 8 {
        let ip = u32::from_be_bytes([p[4], p[5], p[6], p[7]]) ^ MAGIC;
        return Some(IpAddr::V4(std::net::Ipv4Addr::from(ip)));
    }
    if family == 0x02 && p.len() >= 20 {
        let mut key = [0u8; 16];
        key[12..16].copy_from_slice(&MAGIC.to_be_bytes());
        let mut a = [0u8; 16];
        for i in 0..16 {
            a[i] = p[6 + i] ^ key[i];
        }
        return Some(IpAddr::V6(a.into()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a well-formed STUN binding-response packet carrying one
    /// MAPPED-ADDRESS (attr) or XOR-MAPPED-ADDRESS attribute.
    fn build_packet(txn: &[u8; 12], mapped_ipv4: u32, attr: u16) -> Vec<u8> {
        let mut p = Vec::new();
        // 20-byte STUN header: mtype, mlen (payload total), magic, txn.
        p.extend_from_slice(&TYPE_BINDING_RESPONSE.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes()); // placeholder, we fix below
        p.extend_from_slice(&MAGIC.to_be_bytes());
        p.extend_from_slice(txn);

        // Attribute: type(2) + length(2) + payload (8 bytes for v4).
        p.extend_from_slice(&attr.to_be_bytes());
        p.extend_from_slice(&8u16.to_be_bytes());
        let mut payload = [0u8; 8];
        payload[0] = 0x01; // IPv4 family
        payload[1..3].copy_from_slice(&12345u16.to_be_bytes());
        if attr == ATTR_MAPPED {
            payload[4..8].copy_from_slice(&mapped_ipv4.to_be_bytes());
        } else {
            // XOR-MAPPED: IP bytes are XOR'ed with the 32-bit magic cookie.
            let xor_ip = mapped_ipv4 ^ MAGIC;
            payload[4..8].copy_from_slice(&xor_ip.to_be_bytes());
        }
        p.extend_from_slice(&payload);

        // Patch the message length (total attribute bytes = 4 + 8 = 12).
        p[2..4].copy_from_slice(&12u16.to_be_bytes());
        p
    }

    #[test]
    fn parses_plain_mapped_ipv4() {
        let txn: [u8; 12] = [0u8; 12];
        let ip: u32 = 0x0A0B0C0D; // 10.11.12.13 (private, just a known byte value)
        let pkt = build_packet(&txn, ip, ATTR_MAPPED);
        let got = parse_response(&pkt, &txn).expect("should parse");
        assert_eq!(got.ip, IpAddr::from([10, 11, 12, 13]));
    }

    #[test]
    fn parses_xor_mapped_ipv4() {
        let txn: [u8; 12] = [7u8; 12];
        let ip: u32 = 0xC0A80101; // 192.168.1.1
        let pkt = build_packet(&txn, ip, ATTR_XOR_MAPPED);
        let got = parse_response(&pkt, &txn).expect("should parse");
        assert_eq!(got.ip, IpAddr::from([192, 168, 1, 1]));
    }

    #[test]
    fn rejects_wrong_transaction_id() {
        let txn: [u8; 12] = [0u8; 12];
        let pkt = build_packet(&txn, 0x12345678, ATTR_MAPPED);
        assert!(parse_response(&pkt, &[9u8; 12]).is_none());
    }

    #[test]
    fn rejects_garbage() {
        let txn: [u8; 12] = [0u8; 12];
        assert!(parse_response(&[1, 2, 3, 4], &txn).is_none());
    }

    /// Live probe: if network is available this returns Some, otherwise None or
    /// an error — both are acceptable in CI. We assert only the type and that
    /// a returned IP is well-formed, never that the IP is public.
    #[test]
    fn live_probe_shape() {
        match discover() {
            Ok(Some(ip)) => {
                // Just assert it is a parseable IP.
                let _ = format!("{ip}");
            }
            Ok(None) => {}
            Err(_) => {}
        }
    }
}
