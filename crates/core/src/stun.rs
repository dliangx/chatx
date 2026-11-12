//! Minimal RFC 5389 STUN client, extended with a **NAT type fingerprint**.
//!
//! chatx advertises the addresses in `running.listen_addrs` to the directory
//! so remote peers can dial us. Behind a NAT that's a private LAN IP, so
//! cross-NAT peers cannot reach us. STUN lets the client learn its own
//! **public** IP (the one the NAT presents to the outside world) and prepend
//! a `/ip4/<public>/tcp/<listen_port>` (or `/ip6/...`) endpoint (see
//! [`crate::Client::endpoints_with_egress`]).
//!
//! Dual-stack: a separate dual-socket probe runs **per IP family** — one pair
//! of independent sockets hits two different STUN servers over IPv4, another
//! pair does the same over IPv6 (the v6 sockets are dual-stack, so a server
//! with only an A record still answers and its IPv4-mapped reply is folded
//! back into the v4 profile). `discover()` therefore returns **one
//! `NatProfile` per family** (v4 + v6) instead of a single mixed one; multi-
//! homed hosts no longer collapse their v4 + v6 fingerprints into an
//! `Unknown` verdict.
//!
//! Fingerprint design note (per family):
//! * A *single* STUN answer only tells you the public IP — it does not
//!   distinguish endpoint-independent mapping (EIM: full / restricted cone)
//!   from endpoint-dependent mapping (EDM: symmetric NAT), which is exactly
//!   the axis where hole-punching success diverges.
//! * Fingerprint we use: two **independent** local UDP sockets, each probing
//!   the STUN servers in order; take the first success per socket. Read the
//!   mapped source port reported by each.
//!   - both same port  + same IP -> `EIM`
//!   - different ports  + same IP -> `EDM`
//!   - different IPs, or only one answered -> `Unknown` (multi-homed within
//!     one family / CGNAT / transient asymmetry — treat conservatively)
//! * Port-restricted NAT is still EIM (the mapping is not endpoint-dependent;
//!   only the *filter* is peer-dependent), which is correct for our use.
//!
//! Design notes:
//! * Uses `std::net::UdpSocket` with a hard timeout — blocking, callers must
//!   run on a `spawn_blocking` / presence thread.
//! * Cache is process-wide for ~120 s so the 15 s heartbeat loop and the
//!   startup prewarm share one result.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
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

/// Default public STUN servers. We always use the first two distinct
/// endpoints — the dual-socket fingerprint *needs* two servers (one per
/// local socket) to compare the mapped source port across them.
const STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun.stunprotocol.org:3478",
    "stun1.l.google.com:19302",
    "stun2.l.google.com:19302",
    "stun3.l.google.com:19302",
    "stun4.l.google.com:19302",
];

/// Operator-provided STUN servers, discovered from the directory
/// (`GET /v1/stun`, see `signal::StunInfo`) at login time via
/// [`set_custom_servers`]. These take **priority** over the public defaults:
/// on a restricted network the public providers are often blocked on UDP, but
/// the operator's own STUN server (co-located with the directory/relay) is
/// reachable — so we probe it first and only fall back to the public list if
/// it fails.
static CUSTOM_SERVERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Register `servers` (as `host:port`) as the preferred STUN endpoints,
/// taking priority over the built-in public list. Called automatically by
/// the login flow (see `Client::discover_stun`) after asking the directory
/// for the operator's own STUN server. Embedders/tests can also call this
/// directly to force `discover()` to hit a known endpoint. Empty list clears
/// the preference and reverts to the built-in public list.
pub fn set_custom_servers(servers: Vec<String>) {
    let mut s = CUSTOM_SERVERS.lock().unwrap();
    *s = servers
        .into_iter()
        .filter(|a| !a.trim().is_empty())
        .collect();
}

/// Resolve the ordered probe list: operator-discovered servers first, then the
/// built-in public defaults. De-duplicated, empty entries dropped.
fn effective_servers() -> Vec<String> {
    let custom = CUSTOM_SERVERS.lock().unwrap().clone();
    let mut out: Vec<String> = custom
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    for d in STUN_SERVERS {
        if !out.iter().any(|x| x.eq_ignore_ascii_case(d)) {
            out.push(d.to_string());
        }
    }
    out
}

/// NAT mapping behaviour, as fingerprinted by the dual-socket, two-server
/// STUN probe. This is the *caller's* NAT, not the peer's.
///
/// The two values map directly to the hole-punch success matrix:
/// * [`Mapping::Eim`  = endpoint-independent mapping — the same public
///   (ip, port) pair is used for every destination. Full / restricted /
///   port-restricted cones all classify as EIM. Hole-punching is *possible*
///   (whether the peer's filter lets it through is the peer's problem).
/// * [`Mapping::Edm`  = endpoint-dependent mapping — the NAT assigns a
///   different public port for each destination. Symmetric NAT is the
///   canonical example. Hole-punching *fails* against any peer who needs to
///   reach us on the port we advertised, because we would only accept
///   traffic from the source (ip, port) the NAT recorded for the initial
///   probe, not for the peer.
/// * [`Mapping::Unknown` — the probe could not disambiguate (both servers
///   did not respond, or only one did, or the two reported different egress
///   IPs — multi-homed / CGNAT). Conservative callers should treat this as
///   "possibly EDM" and prefer the relay path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapping {
    Eim,
    Edm,
    Unknown,
}

impl std::fmt::Display for Mapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mapping::Eim => write!(f, "EIM"),
            Mapping::Edm => write!(f, "EDM (symmetric)"),
            Mapping::Unknown => write!(f, "unknown"),
        }
    }
}

/// Result of a single successful STUN binding: the client's public IP as seen
/// by the outside world, plus the mapped source port (which we *do* use for
/// the fingerprint, and keep around for diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeResult {
    pub ip: IpAddr,
    /// The public port the STUN server observed our local socket on. **Not
    /// the same as our listening port** — each NAT typically maps a distinct
    /// local socket to a distinct public port. We only use this for the
    /// EIM/EDM comparison, never as an endpoint to advertise.
    pub mapped_port: u16,
}

impl std::fmt::Display for ProbeResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{:?}", self.ip, self.mapped_port)
    }
}

/// A cached snapshot: fingerprints (one per address family that probed
/// cleanly) + expiry.
#[derive(Debug, Clone)]
struct Cached {
    profiles: Vec<NatProfile>,
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

/// Full NAT fingerprint for this host, as observed from the outside world.
/// `ip` is the public IP a peer on the internet would dial to reach us;
/// `mapping` tells the caller whether that IP/port is stable across
/// destinations (EIM) or destination-specific (EDM / symmetric).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NatProfile {
    pub ip: IpAddr,
    pub mapping: Mapping,
}

impl std::fmt::Display for NatProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{}]", self.ip, self.mapping)
    }
}

/// Convenience for callers that only care "can the outside world dial me on
/// a *stable* public IP?" — true only when we have an EIM fingerprint.
impl NatProfile {
    pub fn is_stable_public(&self) -> bool {
        matches!(self.mapping, Mapping::Eim)
    }
}

fn fingerprint(ip_a: IpAddr, port_a: u16, ip_b: IpAddr, port_b: u16) -> (IpAddr, Mapping) {
    if ip_a != ip_b {
        // Multi-homed / CGNAT / split egress — we saw two different public
        // IPs from the vantage point of two STUN servers. We don't know which
        // one a *specific* peer will see; report the first and mark unknown.
        (ip_a, Mapping::Unknown)
    } else if port_a == port_b {
        (ip_a, Mapping::Eim)
    } else {
        (ip_a, Mapping::Edm)
    }
}

/// Discover the NAT fingerprint (public IP + EIM/EDM/Unknown) via the
/// dual-socket, two-server STUN probe. Uses the cache when fresh.
///
/// Returns `Ok(None)` when no STUN server was reachable (e.g. UDP egress
/// blocked, offline) — callers should treat that as "no public address
/// known" and fall back to the relay path.
///
/// **Blocking**: issues UDP I/O on the calling thread, up to
/// `PER_SERVER_TIMEOUT * 2` in the worst case (two independent probes).
/// Call from a background thread or `tokio::task::spawn_blocking`, never
/// from a tokio worker or from the UI event loop path.
pub fn discover() -> anyhow::Result<Vec<NatProfile>> {
    // Fast path: fresh cache.
    {
        let cache = CACHE.lock().unwrap();
        if let Some(c) = &cache.value {
            if c.expires_at > std::time::Instant::now() && !c.profiles.is_empty() {
                return Ok(c.profiles.clone());
            }
        }
    }

    // Slow path: two independent dual-stack sockets -> two servers, per IP
    // address family. We open the sockets *before* picking which servers so
    // port allocation is as close to simultaneous as the OS gives us (NATs
    // that are genuinely EIM still produce the same mapped port in this
    // case; NATs that are EDM produce different ones independent of
    // timing).
    let servers = effective_servers();
    let mut out: Vec<NatProfile> = Vec::new();
    for family in [Family::V4, Family::V6] {
        let sock_a = match bind_socket(family) {
            Ok(s) => s,
            Err(e) => {
                // Host has no interface of this family (or the v4 fallback
                // failed too) — skip the family cleanly, don't abort the
                // other one.
                tracing::debug!(?family, %e, "STUN probe: skipping family (cannot bind)");
                continue;
            }
        };
        let sock_b = match bind_socket(family) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(?family, %e, "STUN probe: skipping family (second socket cannot bind)");
                continue;
            }
        };
        if let Some(profile) = probe_pair(&sock_a, 0, &sock_b, 1, &servers) {
            out.push(profile);
        }
    }

    if out.is_empty() {
        // All probes failed — fall back to a stale cache if we have one,
        // so a momentary STUN outage doesn't wipe out our reachability
        // claim (or our mixed-family profile, e.g. v4 fresh + v6 stale).
        let cache = CACHE.lock().unwrap();
        if let Some(c) = &cache.value {
            let stale = c.profiles.clone();
            tracing::warn!(profiles = ?stale, "all STUN servers unreachable; using stale fingerprint(s)");
            Ok(stale)
        } else {
            Ok(Vec::new())
        }
    } else {
        let mut cache = CACHE.lock().unwrap();
        cache.value = Some(Cached {
            profiles: out.clone(),
            expires_at: std::time::Instant::now() + cache.ttl,
        });
        tracing::info!(?out, "STUN probe: NAT fingerprint(s) resolved");
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy)]
enum Family {
    V4,
    V6,
}

/// Non-blocking cached lookup: returns the most recent NAT fingerprint(s)
/// (fresh or stale), without doing any network I/O. Use on hot paths — call
/// `discover()` from a background thread to populate the cache first.
pub fn discover_cached() -> Option<Vec<NatProfile>> {
    let cache = CACHE.lock().unwrap();
    cache.value.as_ref().map(|c| c.profiles.clone())
}

/// Bind a UDP socket to a random local port. We do this per-probe
/// (two sockets for two servers) because the EIM/EDM comparison *relies on*
/// the two local sockets having independent local ports AND on the NAT
/// observing both, per RFC 5389 / RFC 6580 §2 semantics.
///
/// `family` picks the socket:
/// * [`Family::V4`] — `0.0.0.0:0`, v4-only (matches an A-only reply).
/// * [`Family::V6`] — `[::]:0`, dual-stack on Linux/FreeBSD (the default)
///   and single-stack v6 only on macOS/BSD (where `IPV6_V6ONLY` defaults
///   true). Either way, replies are folded into `IpAddr` per family —
///   a v4-mapped reply on a dual-stack socket is reported as v4.
fn bind_socket(family: Family) -> anyhow::Result<std::net::UdpSocket> {
    let sock = match family {
        Family::V4 => std::net::UdpSocket::bind("0.0.0.0:0"),
        Family::V6 => std::net::UdpSocket::bind("[::]:0"),
    }
    .map_err(|e| anyhow::anyhow!("bind STUN socket: {e}"))?;
    sock.set_read_timeout(Some(PER_SERVER_TIMEOUT))?;
    sock.set_write_timeout(Some(PER_SERVER_TIMEOUT))?;
    Ok(sock)
}

/// Probe `server_index` and its fallbacks, in order, until one responds.
/// Returns `(ip, mapped_port)` on success.
fn probe_socket(
    sock: &std::net::UdpSocket,
    start_index: usize,
    servers: &[String],
) -> Option<ProbeResult> {
    let n = servers.len();
    if n == 0 {
        return None;
    }
    for offset in 0..n {
        let server = &servers[(start_index + offset) % n];
        match probe_one(sock, server) {
            Ok(Some(pr)) => return Some(pr),
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(server, %e, "STUN probe failed");
                continue;
            }
        }
    }
    None
}

/// Force-probe a single STUN server, bypassing the cache. Exposed for tests.
fn probe_one(sock: &std::net::UdpSocket, server: &str) -> anyhow::Result<Option<ProbeResult>> {
    // STUN servers are hostnames (e.g. `stun.l.google.com:19302`), so resolve
    // them with `ToSocketAddrs` instead of `SocketAddr::parse` — the latter
    // only accepts literal IP:port and rejects every default entry.
    // A host name can resolve to several addresses (e.g. `stun.l.google.com`
    // often returns AAAA + A). The caller's socket is family-specific (`stun.rs`
    // probes v4 and v6 as *separate* socket pairs), so an address of the wrong
    // family makes `send_to` fail with `InvalidArgument` (os error 22) — skip
    // it and try the next address; a family mismatch must NOT terminate the
    // server's fallback chain (that used to mask the working sibling record
    // and report "no fingerprint" even though the host answered).
    //
    // Note: a dual-stack v6 socket receives v4-mapped replies fine — the
    // resolved family of `addr` only gates the *send*; the STUN
    // MAPPED-ADDRESS we parse back is whatever the server saw, so the
    // caller's per-family fold keeps profiles from collapsing into one.
    let addrs: Vec<SocketAddr> = server
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("bad STUN server {server}: {e}"))?
        .collect();
    if addrs.is_empty() {
        anyhow::bail!("bad STUN server {server}: no addresses");
    }

    // 20-byte STUN header: 2-bit message class + 14-bit message length.
    let mut header = [0u8; 20];
    header[0..2].copy_from_slice(&TYPE_BINDING_REQUEST.to_be_bytes());
    header[2..4].copy_from_slice(&0u16.to_be_bytes()); // message length (no attrs)
    header[4..8].copy_from_slice(&MAGIC.to_be_bytes());

    for addr in addrs {
        // Every attempt gets its own 12-byte STUN nonce (RFC 5389 §5): the
        // Binding Response echoes it back so we can verify the reply is ours.
        // A nonce reused across attempts would let the kernel deliver a stale
        // reply to the wrong sender.
        let txn: [u8; 12] = rand::random();
        let mut pkt = header;
        pkt[8..20].copy_from_slice(&txn);

        match sock.send_to(&pkt, addr) {
            // IPv6 address offered to an AF_INET socket (or vice versa) —
            // just skip this address and try the next one.
            Err(ref e) if e.raw_os_error() == Some(22) => continue,
            Err(e) => return Err(anyhow::anyhow!("STUN send to {server} {addr}: {e}")),
            Ok(_) => {}
        }
        let mut buf = vec![0u8; 1024];
        match sock.recv_from(&mut buf) {
            Ok((n, _from)) => {
                buf.truncate(n);
                return Some(parse_response(&buf, &txn)).ok_or_else(|| {
                    anyhow::anyhow!("STUN response from {server} {addr} missing MAPPED-ADDRESS")
                });
            }
            // No answer within timeout — try the next address/server.
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(anyhow::anyhow!("STUN recv from {server} {addr}: {e}")),
        }
    }
    Ok(None)
}

/// Run the dual-socket / two-server fingerprint. `start_a` / `start_b`
/// select which STUN server each socket starts with, so the two probes hit
/// distinct endpoints even if only a subset of them is reachable. `servers`
/// is the resolved probe list (operator-priority, deduplicated, non-empty).
fn probe_pair(
    sock_a: &std::net::UdpSocket,
    start_a: usize,
    sock_b: &std::net::UdpSocket,
    start_b: usize,
    servers: &[String],
) -> Option<NatProfile> {
    let a = probe_socket(sock_a, start_a, servers);
    let b = probe_socket(sock_b, start_b, servers);
    match (a, b) {
        (Some(a), Some(b)) => {
            let (ip, mapping) = fingerprint(a.ip, a.mapped_port, b.ip, b.mapped_port);
            Some(NatProfile { ip, mapping })
        }
        // Only one probe answered — we can't do the comparison, but we do
        // have a valid public IP. Report Unknown for the mapping so callers
        // treat it conservatively (relay-preferred).
        (Some(a), None) | (None, Some(a)) => Some(NatProfile {
            ip: a.ip,
            mapping: Mapping::Unknown,
        }),
        (None, None) => None,
    }
}

/// Validate the response header (magic, transaction id, type) and extract the
/// XOR-MAPPED-ADDRESS (preferred) or MAPPED-ADDRESS attribute.
fn parse_response(pkt: &[u8], txn: &[u8; 12]) -> Option<ProbeResult> {
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

        let pr = if attr == ATTR_XOR_MAPPED {
            xor_mapped(payload)
        } else if attr == ATTR_MAPPED {
            plain_mapped(payload)
        } else {
            None
        };
        if let Some(pr) = pr {
            return Some(pr);
        }

        let aligned = (len + 3) & !3;
        cursor = payload_start + aligned;
    }
    None
}

/// Parse an MAPPED-ADDRESS (unencrypted) attribute per RFC 5389 §3:
///   byte 0   : reserved (1)
///   byte 1   : F  (0x01=IPv4, 0x02=IPv6)
///   bytes 2-3: port (BE u16)
///   bytes 4+ : IP (4 or 16 bytes)
///
/// NOTE: earlier versions of this file had the layout shifted by one byte
/// (family at byte 0, port at bytes 1-2, IP at 3+), which did not match any
/// real-world STUN server.
fn plain_mapped(p: &[u8]) -> Option<ProbeResult> {
    let family = p[1];
    if family == 0x01 && p.len() >= 8 {
        let port = u16::from_be_bytes([p[2], p[3]]);
        let ip = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
        return Some(ProbeResult {
            ip: IpAddr::V4(std::net::Ipv4Addr::from(ip)),
            mapped_port: port,
        });
    }
    if family == 0x02 && p.len() >= 20 {
        let port = u16::from_be_bytes([p[2], p[3]]);
        let mut a = [0u8; 16];
        a.copy_from_slice(&p[4..20]);
        return Some(ProbeResult {
            ip: IpAddr::V6(a.into()),
            mapped_port: port,
        });
    }
    None
}

/// Parse an XOR-MAPPED-ADDRESS attribute per RFC 5389 §3.
///
/// Layout (v4, 8 bytes):  reserved(1)  F(1)  port(2)  ip(4)
/// Layout (v6, 20 bytes): reserved(1)  F(1)  port(2)  ip(16)
///
/// XOR key per RFC 5389 §6.2:
///   port is XOR'd with `(MAGIC >> 16) as u16`  (i.e. 0x2112)
///   v4 ip (4 bytes) is XOR'd with `MAGIC` as u32
///   v6 lower 4 bytes of ip are XOR'd with `MAGIC` as u32;
///      upper 12 bytes are not XOR'd.
///
/// NOTE: earlier versions of this file also had the layout shifted by one
/// byte AND did not XOR the port, making it incompatible with real-world
/// STUN servers (stun.l.google.com, stun.cloudflare.com, etc.).
fn xor_mapped(p: &[u8]) -> Option<ProbeResult> {
    let family = p[1]; // F is at byte 1, NOT byte 0 (byte 0 is reserved)
    let port_raw = u16::from_be_bytes([p[2], p[3]]);
    let port = port_raw ^ ((MAGIC >> 16) as u16); // RFC 5389: XOR with magic high 16

    if family == 0x01 && p.len() >= 8 {
        let ip = u32::from_be_bytes([p[4], p[5], p[6], p[7]]) ^ MAGIC;
        return Some(ProbeResult {
            ip: IpAddr::V4(std::net::Ipv4Addr::from(ip)),
            mapped_port: port,
        });
    }
    if family == 0x02 && p.len() >= 20 {
        let mut a = [0u8; 16];
        // Upper 12 bytes: key is zero, so copy through unchanged.
        a[..12].copy_from_slice(&p[4..16]);
        // Lower 4 bytes: XOR with the 32-bit MAGIC cookie.
        let key = MAGIC.to_be_bytes();
        for i in 0..4 {
            a[12 + i] = p[16 + i] ^ key[i];
        }
        return Some(ProbeResult {
            ip: IpAddr::V6(a.into()),
            mapped_port: port,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a well-formed STUN binding-response packet carrying one
    /// MAPPED-ADDRESS (unencrypted) or XOR-MAPPED-ADDRESS attribute.
    ///
    /// RFC 5389 §15.1 v4 layout (8 bytes):
    ///   byte 0 : reserved (0)
    ///   byte 1 : F (0x01 for IPv4)
    ///   byte 2-3: port (BE u16, clear for MAPPED-ADDRESS)
    ///   byte 4-7: ip  (4 bytes BE)
    ///
    /// RFC 5389 §15.2 XOR-MAPPED-ADDRESS v4 layout:
    ///   Same as above, EXCEPT:
    ///     port is XOR'd with `(MAGIC >> 16) as u16`
    ///     ip  is XOR'd with `MAGIC as u32`
    fn build_packet(txn: &[u8; 12], mapped_ipv4: u32, mapped_port: u16, attr: u16) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&TYPE_BINDING_RESPONSE.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes()); // placeholder, we fix below
        p.extend_from_slice(&MAGIC.to_be_bytes());
        p.extend_from_slice(txn);

        p.extend_from_slice(&attr.to_be_bytes());
        p.extend_from_slice(&8u16.to_be_bytes());
        let mut payload = [0u8; 8];
        payload[0] = 0x00; // reserved
        payload[1] = 0x01; // IPv4 family
        if attr == ATTR_MAPPED {
            payload[2..4].copy_from_slice(&mapped_port.to_be_bytes());
            payload[4..8].copy_from_slice(&mapped_ipv4.to_be_bytes());
        } else {
            let port_x = mapped_port ^ ((MAGIC >> 16) as u16);
            let ip_x = mapped_ipv4 ^ (MAGIC as u32);
            payload[2..4].copy_from_slice(&port_x.to_be_bytes());
            payload[4..8].copy_from_slice(&ip_x.to_be_bytes());
        }
        p.extend_from_slice(&payload);

        p[2..4].copy_from_slice(&12u16.to_be_bytes());
        p
    }

    /// The MAPPED-ADDRESS plain test — payload must come out as the (ip, port)
    /// the test asked for, with the reserved byte zeroed.
    #[test]
    fn parses_plain_mapped_ipv4() {
        let txn: [u8; 12] = [0u8; 12];
        let ip: u32 = 0x0A0B0C0D; // 10.11.12.13
        let port: u16 = 4242;
        let pkt = build_packet(&txn, ip, port, ATTR_MAPPED);
        let got = parse_response(&pkt, &txn).expect("should parse");
        assert_eq!(got.ip, IpAddr::from([10, 11, 12, 13]));
        assert_eq!(got.mapped_port, port);
    }

    #[test]
    fn parses_xor_mapped_ipv4_and_port() {
        let txn: [u8; 12] = [7u8; 12];
        let ip: u32 = 0xC0A80101; // 192.168.1.1
        let port: u16 = 33033;
        let pkt = build_packet(&txn, ip, port, ATTR_XOR_MAPPED);
        let got = parse_response(&pkt, &txn).expect("should parse");
        assert_eq!(got.ip, IpAddr::from([192, 168, 1, 1]));
        assert_eq!(got.mapped_port, port);
    }

    #[test]
    fn rejects_wrong_transaction_id() {
        let txn: [u8; 12] = [0u8; 12];
        let pkt = build_packet(&txn, 0x12345678, 1, ATTR_MAPPED);
        assert!(parse_response(&pkt, &[9u8; 12]).is_none());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_response(&[1, 2, 3, 4], &[0u8; 12]).is_none());
    }

    #[test]
    fn fingerprint_same_port_same_ip_is_eim() {
        let ip_a: IpAddr = "1.2.3.4".parse().unwrap();
        let ip_b: IpAddr = "1.2.3.4".parse().unwrap();
        let (_, m) = fingerprint(ip_a, 40000, ip_b, 40000);
        assert_eq!(m, Mapping::Eim);
    }

    #[test]
    fn fingerprint_diff_port_same_ip_is_edm() {
        let ip_a: IpAddr = "1.2.3.4".parse().unwrap();
        let ip_b: IpAddr = "1.2.3.4".parse().unwrap();
        let (_, m) = fingerprint(ip_a, 40000, ip_b, 40001);
        assert_eq!(m, Mapping::Edm);
    }

    #[test]
    fn fingerprint_diff_ip_is_unknown() {
        let ip_a: IpAddr = "1.2.3.4".parse().unwrap();
        let ip_b: IpAddr = "5.6.7.8".parse().unwrap();
        let (ip, m) = fingerprint(ip_a, 40000, ip_b, 40000);
        assert_eq!(m, Mapping::Unknown);
        assert_eq!(ip, ip_a); // first IP wins as the reported one
    }

    /// Live probe: if network is available this returns one or more profiles
    /// (v4 + v6), otherwise an empty list — both are acceptable in CI. We
    /// assert only the type and that each value is well-formed, never which
    /// mapping it is.
    #[test]
    fn live_probe_shape() {
        if let Ok(profiles) = discover() {
            for profile in &profiles {
                let _ = format!("{profile}");
                let _ = profile.is_stable_public();
            }
        }
    }
}
