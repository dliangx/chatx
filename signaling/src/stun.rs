//! RFC 5389 STUN Binding server.
//!
//! Co-located with the relay (and advertised through the same `App` state)
//! so a client on a network where the built-in public STUN providers
//! (`stun.l.google.com:19302`, `stun.cloudflare.com:3478`, ...) are blocked
//! — carrier NAT, mobile CGNAT, restricted egress — can still learn its
//! public IP + NAT fingerprint (EIM vs EDM) from the operator's own infra.
//!
//! Wire format (RFC 5389 §6 message header, §15 attributes):
//!
//!   Binding request  0x0001 : 20-byte fixed header, no attributes required
//!   Binding response 0x0101 : 20-byte fixed header + attributes
//!
//!   MAPPED-ADDRESS     0x0001
//!     1 reserved byte (0) | 1 family (0x01 v4 / 0x02 v6) | 2 port (BE clear) | 4|16 ip
//!
//!   XOR-MAPPED-ADDRESS 0x0020
//!     Same layout as MAPPED-ADDRESS, EXCEPT:
//!       * port is XOR'd with `(MAGIC >> 16) as u16`  (MAGIC = 0x21_12_A4_42)
//!       * IPv4 ip (4 bytes) is XOR'd with `MAGIC` as a 32-bit cookie
//!       * IPv6 ip — only the LOW 4 bytes are XOR'd with the 32-bit cookie;
//!         the upper 12 pass through
//!
//! We emit **both** attributes per response: the MAPPED-ADDRESS in plain
//! form and the XOR-MAPPED-ADDRESS in the per-RFC-5389 XOR form, both
//! carrying the sender's observed (ip, port). Real-world public STUN
//! servers (stun.l.google.com, stun.cloudflare.com) emit only XOR-MAPPED;
//! adding MAPPED-ADDRESS in addition means the existing chatx client
//! (`crates/core/src/stun.rs::parse_response`, which prefers MAPPED when it
//! sees one) and any standard client can talk to us.
//!
//! The `host:port` a client actually dials is resolved from
//! `CHATX_STUN_ADVERTISE` (preferred, dialable public host) or an egress probe,
//! and printed on startup so the operator knows what to point clients at.
//!
//! This is a **standalone binary** (`chatx-stun`, see `[[bin]]` in
//! Cargo.toml) so it runs in its own process and never shares the signal
//! server's runtime — keeping the UDP STUN loop from ever competing with the
//! TCP/HTTP signal server (which historically starved the health endpoint).
//! Clients discover the address via the signal server's `GET /v1/stun`, which
//! reports the same `CHATX_STUN_ADVERTISE` / `CHATX_STUN_ADDR` config.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

const MAGIC: u32 = 0x21_12_A4_42;
const TYPE_BINDING_REQUEST: u16 = 0x0001;
const TYPE_BINDING_RESPONSE: u16 = 0x0101;
const ATTR_MAPPED: u16 = 0x0001;
const ATTR_XOR_MAPPED: u16 = 0x0020;

/// Standalone entry point. `CHATX_STUN_ADDR` is the bind address (default
/// `0.0.0.0:3478`); `CHATX_STUN_ADVERTISE` pins the dialable public host.
/// `RUST_LOG` controls verbosity.
#[tokio::main]
async fn main() {
    let bind = std::env::var("CHATX_STUN_ADDR").unwrap_or_else(|_| "0.0.0.0:3478".into());
    let addr: SocketAddr = bind
        .parse()
        .unwrap_or_else(|e| panic!("CHATX_STUN_ADDR must be host:port, got {bind}: {e}"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let (tx, _rx) = tokio::sync::oneshot::channel::<String>();
    start(addr, tx).await;
}

/// Resolve the public/dialable IP to advertise. Order:
/// 1. `CHATX_STUN_ADVERTISE=<host:port>` — explicit operator value, most reliable.
/// 2. Egress probe (UDP `connect` to 8.8.8.8, read local source IP) — correct on
///    a bare public line, still private behind a VPC/NAT.
/// Returns `None` when the advertise host cannot be determined; the caller
/// then announces the raw bind address and logs a warning.
async fn resolve_advertise_ip() -> Option<IpAddr> {
    if let Ok(spec) = std::env::var("CHATX_STUN_ADVERTISE") {
        let s = spec.trim();
        if !s.is_empty() {
            let host = s
                .split_once(':')
                .map(|(h, _)| h.to_string())
                .unwrap_or_else(|| s.to_string());
            if let Ok(ip) = host.parse::<IpAddr>() {
                tracing::info!(%ip, "advertise IP from CHATX_STUN_ADVERTISE");
                return Some(ip);
            }
            if let Some(a) = lookup_host(host.clone()).await {
                let ip = a.ip();
                tracing::info!(%ip, %host, "advertise IP resolved via DNS");
                return Some(ip);
            }
            tracing::warn!(%host, "CHATX_STUN_ADVERTISE not resolvable; falling back to egress probe");
        }
    }
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:53").is_ok() {
            if let Ok(local) = sock.local_addr() {
                tracing::info!(%local, "advertise IP inferred from egress probe");
                return Some(local.ip());
            }
        }
    }
    tracing::warn!("no public host known — will advertise the local bind address");
    None
}

/// Async wrapper over `ToSocketAddrs::to_socket_addrs` (which is blocking).
/// Takes an owned `String` because the closure passed to `spawn_blocking`
/// must be `Send + 'static`. Returns the first address resolved, if any.
async fn lookup_host(host: String) -> Option<SocketAddr> {
    tokio::task::spawn_blocking(move || {
        host.to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
    })
    .await
    .ok()
    .flatten()
}

/// Listen on `addr` and answer STUN Binding requests until the process
/// exits (or the socket errors). `ready_tx` optionally receives the
/// advertised `host:port` string exactly once, immediately after bind.
async fn start(addr: SocketAddr, ready_tx: tokio::sync::oneshot::Sender<String>) {
    let public_host = resolve_advertise_ip().await;
    let sock = tokio::net::UdpSocket::bind(addr)
        .await
        .inspect_err(|e| tracing::error!(%e, "STUN bind failed"))
        .expect("STUN bind");
    let advertise = match public_host {
        Some(ip) => format!("{ip}:{}", addr.port()),
        None => {
            let local = sock.local_addr().unwrap();
            format!("{local}")
        }
    };
    tracing::info!(%advertise, "chatx-stun listening — point clients at this address");
    if let Err(e) = ready_tx.send(advertise) {
        tracing::warn!(%e, "STUN ready channel closed before announcing");
    }
    let mut buf = [0u8; 2048];
    loop {
        let (n, from) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(%e, "STUN recv_from failed; exiting");
                break;
            }
        };
        handle_request(&buf[..n], from, &sock).await;
    }
}

/// Build a full Binding-success response (header + MAPPED-ADDRESS +
/// XOR-MAPPED-ADDRESS) for the given sender address, and send it back over
/// `sock`. The `source` is the OS-observed sender (ip, port) — the exact
/// value to echo back, whether the peer's NAT rewrote it or not.
async fn handle_request(pkt: &[u8], source: SocketAddr, sock: &tokio::net::UdpSocket) {
    if pkt.len() < 20 {
        tracing::debug!(len = pkt.len(), "STUN short request — dropped");
        return;
    }
    let mtype = u16::from_be_bytes([pkt[0], pkt[1]]);
    if mtype != TYPE_BINDING_REQUEST {
        tracing::debug!(mtype, "STUN non-binding request — dropped");
        return;
    }
    let mlen = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
    if 20 + mlen > pkt.len() {
        tracing::debug!(mlen, "STUN truncated — dropped");
        return;
    }
    if u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]) != MAGIC {
        tracing::debug!(?source, "STUN bad magic — dropped");
        return;
    }
    let txn: [u8; 12] = pkt[8..20].try_into().unwrap();
    let response = build_response(&txn, source);
    if let Err(e) = sock.send_to(&response, source).await {
        tracing::warn!(?source, %e, "STUN send_to failed");
    } else {
        tracing::debug!(?source, "STUN binding answered");
    }
}

/// Build the full Binding-success response body for a sender's (ip, port).
/// Returns a Vec sized to 20 (header) + the two attribute slots.
fn build_response(txn: &[u8; 12], source: SocketAddr) -> Vec<u8> {
    let (family_byte, port, ip_payload_bytes) = match source {
        SocketAddr::V4(v4) => {
            let ip = v4.ip().to_bits().to_be_bytes();
            (0x01u8, v4.port(), ip.to_vec())
        }
        SocketAddr::V6(v6) => {
            let ip = v6.ip().octets();
            (0x02u8, v6.port(), ip.to_vec())
        }
    };

    // Compute both attribute payloads per RFC 5389 §15.1 and §15.2.
    // MAPPED-ADDRESS: reserved(1) Family(1) Port(2 clear) IP(4|16)
    let mut mapped_payload = Vec::with_capacity(4 + ip_payload_bytes.len());
    mapped_payload.push(0x00);
    mapped_payload.push(family_byte);
    mapped_payload.extend_from_slice(&port.to_be_bytes());
    mapped_payload.extend_from_slice(&ip_payload_bytes);

    // XOR-MAPPED-ADDRESS:
    //   port  ^= (MAGIC >> 16) as u16
    //   IP    : v4 — all 4 bytes XOR'd with MAGIC (32-bit)
    //           v6 — low 4 bytes XOR'd with MAGIC (32-bit), high 12 unchanged
    let magic_high: u16 = (MAGIC >> 16) as u16;
    let magic_low: u32 = MAGIC as u32;
    let xor_port = port ^ magic_high;
    let mut xor_ip = ip_payload_bytes.clone();
    if family_byte == 0x01 {
        let key = magic_low.to_be_bytes();
        for i in 0..4 {
            xor_ip[i] ^= key[i];
        }
    } else {
        let key = magic_low.to_be_bytes();
        for i in 0..4 {
            xor_ip[12 + i] ^= key[i];
        }
    }
    let mut xor_payload = Vec::with_capacity(mapped_payload.len());
    xor_payload.push(0x00);
    xor_payload.push(family_byte);
    xor_payload.extend_from_slice(&xor_port.to_be_bytes());
    xor_payload.extend_from_slice(&xor_ip);

    // Serialize two attributes with 4-byte alignment padding.
    fn write_attr(out: &mut Vec<u8>, typ: u16, payload: &[u8]) {
        out.extend_from_slice(&typ.to_be_bytes());
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        let pad = (4 - (payload.len() % 4)) % 4;
        out.extend(std::iter::repeat(0u8).take(pad));
    }

    let mut attrs = Vec::new();
    write_attr(&mut attrs, ATTR_MAPPED, &mapped_payload);
    write_attr(&mut attrs, ATTR_XOR_MAPPED, &xor_payload);

    // Header: type(2) msglen(2) magic(4) txn(12) = 20 bytes.
    let mut resp = Vec::with_capacity(20 + attrs.len());
    resp.extend_from_slice(&TYPE_BINDING_RESPONSE.to_be_bytes());
    resp.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
    resp.extend_from_slice(&MAGIC.to_be_bytes());
    resp.extend_from_slice(txn);
    resp.extend_from_slice(&attrs);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Layout check for each attribute against a known (ip, port).
    /// v4:  reserved(1) Family(=1) Port(2 clear) Ip(4) = 8 bytes total.
    /// v6:  reserved(1) Family(=2) Port(2 clear) Ip(16) = 20 bytes total.
    fn split_attrs(msg: &[u8]) -> std::collections::BTreeMap<u16, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        let mut cursor = 20usize;
        while cursor + 4 <= msg.len() {
            let typ = u16::from_be_bytes([msg[cursor], msg[cursor + 1]]);
            let len = u16::from_be_bytes([msg[cursor + 2], msg[cursor + 3]]) as usize;
            let ps = cursor + 4;
            if ps + len > msg.len() {
                break;
            }
            out.insert(typ, msg[ps..ps + len].to_vec());
            cursor = ps + ((len + 3) & !3);
        }
        out
    }

    #[test]
    fn v4_mapped_roundtrip() {
        let src: SocketAddr = "1.2.3.4:54321".parse().unwrap();
        let resp = build_response(&[0u8; 12], src);
        let attrs = split_attrs(&resp);
        let mapped = attrs.get(&ATTR_MAPPED).expect("MAPPED-ADDRESS present");
        assert_eq!(mapped[0], 0x00, "reserved");
        assert_eq!(mapped[1], 0x01, "v4 family");
        assert_eq!(u16::from_be_bytes([mapped[2], mapped[3]]), 54321, "plain port");
        assert_eq!(u32::from_be_bytes([mapped[4], mapped[5], mapped[6], mapped[7]]), 0x0102_0304u32, "plain ip");

        let xor = attrs.get(&ATTR_XOR_MAPPED).expect("XOR-MAPPED-ADDRESS present");
        assert_eq!(xor[0], 0x00, "reserved");
        assert_eq!(xor[1], 0x01, "v4 family");
        let port_x = u16::from_be_bytes([xor[2], xor[3]]);
        let ip_x = u32::from_be_bytes([xor[4], xor[5], xor[6], xor[7]]);
        assert_eq!(port_x ^ (MAGIC >> 16) as u16, 54321);
        assert_eq!(ip_x ^ (MAGIC as u32), 0x0102_0304u32);
    }

    #[test]
    fn v6_mapped_roundtrip() {
        let src: SocketAddr = "[::1]:1234".parse().unwrap();
        let resp = build_response(&[0u8; 12], src);
        let attrs = split_attrs(&resp);
        let mapped = attrs.get(&ATTR_MAPPED).expect("MAPPED-ADDRESS present");
        assert_eq!(mapped[0], 0x00, "reserved");
        assert_eq!(mapped[1], 0x02, "v6 family");
        assert_eq!(u16::from_be_bytes([mapped[2], mapped[3]]), 1234, "plain port");
        let ip_raw: [u8; 16] = mapped[4..20].try_into().unwrap();
        // ::1 == [0 x 15, 1]
        assert_eq!(&ip_raw[..15], &[0u8; 15]);
        assert_eq!(ip_raw[15], 1);

        let xor = attrs.get(&ATTR_XOR_MAPPED).expect("XOR-MAPPED-ADDRESS present");
        assert_eq!(xor[1], 0x02, "v6 family");
        let port_x = u16::from_be_bytes([xor[2], xor[3]]);
        assert_eq!(port_x ^ ((MAGIC >> 16) as u16), 1234, "port de-xored");
        let ip_x: [u8; 16] = xor[4..20].try_into().unwrap();
        // Low 4 bytes XOR'd with the 32-bit magic; upper 12 unchanged (zero here).
        let key = (MAGIC as u32).to_be_bytes();
        let mut de = ip_x;
        for i in 0..4 {
            de[12 + i] ^= key[i];
        }
        // de must equal the plain mapped IP back (roundtrip).
        assert_eq!(de, ip_raw, "ip de-xored must equal plain ip");
    }

    /// End-to-end: a Binding request → `handle_request` → real UDP datagram
    /// → the loopback client that issued the request reads its own
    /// MAPPED-ADDRESS back, with (ip, port) that matches the source
    /// `recv_from` reports.
    #[tokio::test]
    async fn handle_request_e2e_via_build() {
        let send_sock = tokio::net::UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let recv_sock = tokio::net::UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let recv_local = recv_sock.local_addr().unwrap();

        let txn: [u8; 12] = [0xABu8; 12];
        let mut req = [0u8; 20];
        req[0..2].copy_from_slice(&TYPE_BINDING_REQUEST.to_be_bytes());
        req[4..8].copy_from_slice(&MAGIC.to_be_bytes());
        req[8..20].copy_from_slice(&txn);

        // `handle_request` treats `source` as the OS-observed sender and
        // replies to it. We pass `recv_local` as the source so the response
        // lands on the receiving socket below.
        handle_request(&req, recv_local, &send_sock).await;

        let mut buf = [0u8; 256];
        let (n, _from) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            recv_sock.recv_from(&mut buf),
        )
        .await
        .expect("no response received")
        .unwrap();
        let resp = &buf[..n];

        assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), TYPE_BINDING_RESPONSE);
        assert_eq!(u32::from_be_bytes([resp[4], resp[5], resp[6], resp[7]]), MAGIC);
        assert_eq!(&resp[8..20], &txn, "txn must echo");

        let attrs = split_attrs(resp);
        let mapped = attrs.get(&ATTR_MAPPED).expect("MAPPED-ADDRESS present");
        let xor = attrs.get(&ATTR_XOR_MAPPED).expect("XOR-MAPPED-ADDRESS present");

        let mapped_port = u16::from_be_bytes([mapped[2], mapped[3]]);
        let xor_port = u16::from_be_bytes([xor[2], xor[3]]);
        let fam = mapped[1];

        // Expected (ip, port) from the source socket.
        let (src_ip_u32, src_ip_v6, src_port) = match recv_local {
            SocketAddr::V4(v) => (v.ip().to_bits(), [0u8; 16], v.port()),
            SocketAddr::V6(v) => (0u32, v.ip().octets(), v.port()),
        };

        assert_eq!(mapped_port, src_port, "MAPPED port == source port");
        assert_eq!(
            xor_port ^ ((MAGIC >> 16) as u16),
            src_port,
            "XOR-MAPPED port must de-xor back to source port"
        );

        let mapped_payload_len: usize = if fam == 0x01 { 4 } else { 16 };
        let mapped_ip_v4 = u32::from_be_bytes([
            mapped[4], mapped[5], mapped[6], mapped[7]
        ]);
        let xor_ip_v4 = u32::from_be_bytes([
            xor[4], xor[5], xor[6], xor[7]
        ]);

        if fam == 0x01 {
            assert_eq!(mapped_ip_v4, src_ip_u32, "MAPPED v4 ip");
            assert_eq!(xor_ip_v4 ^ (MAGIC as u32), src_ip_u32, "XOR-MAPPED v4 ip de-xor");
        } else {
            let m_v6: [u8; 16] = mapped[4..20].try_into().unwrap();
            let x_v6: [u8; 16] = xor[4..20].try_into().unwrap();
            assert_eq!(m_v6, src_ip_v6, "MAPPED v6 ip");
            let key = (MAGIC as u32).to_be_bytes();
            let mut de = x_v6;
            for i in 0..4 {
                de[12 + i] ^= key[i];
            }
            assert_eq!(de, src_ip_v6, "XOR-MAPPED v6 ip de-xor");
        }
        let _ = mapped_payload_len;
    }
}
