//! Built-in libp2p circuit-relay v2 server.
//!
//! Runs as a task inside `chatx-signal`. Clients discover the relay address
//! automatically via `GET /v1/relays` (or manually via `CHATX_RELAYS`).
//! Peers that cannot dial each other directly (cross-NAT) then reach one
//! another through the relay. The relay only shuttles already-encrypted byte
//! streams — it never sees message plaintext nor holds any chatx keys.
//!
//! Deployment:
//!   cargo run -p chatx-signal                     # relay on 0.0.0.0:1883
//!   CHATX_RELAY_ADDR=0.0.0.0:5000 cargo run -p chatx-signal
//!   CHATX_RELAY_ADDR=off ...                      # disable

use std::net::IpAddr;
use libp2p::futures::StreamExt as _;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, SwarmBuilder};

/// Resolve the public host to use in the announced advertise address,
/// replacing "locally non-dialable" IPs (`0.0.0.0` any, `127.*` loopback,
/// private-LAN 10/172.16-31/192.168) that are meaningless to a client on
/// **another network**.
///
/// Resolution priority:
/// 1. `CHATX_RELAY_ADVERTISE=<public-host:port>` (explicit operator config, most reliable)
/// 2. Egress probe (`UdpSocket::connect` to read the local IP) — yields the
///    public IP when running on a bare public line, but still a private IP
///    behind a cloud VPC or NAT
/// 3. Fall back to the original address (with a warn) — at least passes localhost self-test
///
/// Returns `None` when no public host can be determined; the caller keeps
/// the listen address as-is (and a warn is emitted above).
async fn resolve_public_host() -> Option<IpAddr> {
    if let Ok(spec) = std::env::var("CHATX_RELAY_ADVERTISE") {
        let s = spec.trim().to_ascii_lowercase();
        if s.is_empty() {
            return None;
        }
        let host = spec
            .split_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or(spec.to_string());
        if let Ok(ip) = host.parse::<IpAddr>() {
            tracing::info!(%ip, "relay advertise IP from CHATX_RELAY_ADVERTISE");
            return Some(ip);
        }
        if let Ok(addrs) = tokio::net::lookup_host(&host).await
        {
            // Prefer the first IPv4, otherwise fall back to the first address.
            let mut any = None;
            for a in addrs {
                if a.ip().is_ipv4() {
                    any = Some(a.ip());
                    break;
                }
                if any.is_none() {
                    any = Some(a.ip());
                }
            }
            if let Some(ip) = any {
                tracing::info!(%ip, %host, "relay advertise IP resolved via DNS from CHATX_RELAY_ADVERTISE");
                return Some(ip);
            }
        }
        tracing::warn!(host, "CHATX_RELAY_ADVERTISE not resolvable; falling back to egress probe");
    }

    // Egress probe: UDP connect to 8.8.8.8 and read the local source IP.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:53").is_ok() {
            if let Ok(local) = sock.local_addr() {
                if is_public_ip(local.ip()) {
                    tracing::info!(%local, "relay advertise IP inferred from egress probe");
                    return Some(local.ip());
                }
                tracing::warn!(
                    %local,
                    "egress probe returned a private IP — the relay is behind NAT \
                     and cannot be reached by cross-network peers; set \
                     CHATX_RELAY_ADVERTISE to the public IP"
                );
            }
        }
    }
    tracing::warn!(
        "no public host known for the relay; advertise address will be \
         non-routable. Set CHATX_RELAY_ADVERTISE=<public-host:port>"
    );
    None
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_unspecified()
                && !v4.is_private()
                && !v4.is_link_local()
        }
        // IPv6: keep only addresses that are not loopback / unspecified / link-local.
        IpAddr::V6(v6) => {
            !v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local()
        }
    }
}

/// Start the circuit-relay server. When the relay is listening (first
/// `NewListenAddr` event fires) the fully-qualified advertise multiaddr
/// (e.g. `/ip4/1.2.3.4/tcp/1883/p2p/12D3Koo...`) is sent via `ready_tx`.
/// The function blocks until the relay exits.
pub async fn start(
    addr: std::net::SocketAddr,
    ready_tx: tokio::sync::oneshot::Sender<String>,
) -> anyhow::Result<()> {
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let peer_id: PeerId = keypair.public().to_peer_id();
    tracing::info!(%peer_id, "relay peer id");

    // Public host used in the advertise multiaddr that clients dial. This
    // is resolved before the listen loop so we can replace the listen
    // address's IP (which is `0.0.0.0` / private by default) with something
    // routable. `None` ⇒ announce the address as-is (with a warn above).
    let public_host = resolve_public_host().await;

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(Default::default(), libp2p::noise::Config::new, libp2p::yamux::Config::default)
        .expect("tcp+noise+yamux")
        .with_quic()
        .with_behaviour(|_k| libp2p::relay::Behaviour::new(peer_id, libp2p::relay::Config::default()))?
        .build();

    // Bind to all interfaces (force `0.0.0.0` when the operator supplied a
    // specific bind IP so the relay is reachable from outside), but advertise
    // with the public host instead — the listen address the OS reports is
    // meaningless for remote peers.
    let listen_ip = if addr.ip() == std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        || is_public_ip(addr.ip())
    {
        addr.ip()
    } else {
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
    };
    let tcp: Multiaddr = format!("/ip4/{}/tcp/{}", listen_ip, addr.port())
        .parse()
        .map_err(|e| anyhow::anyhow!("parse listen addr: {e}"))?;
    swarm.listen_on(tcp.clone()).map_err(|e| anyhow::anyhow!("listen {tcp}: {e}"))?;
    let udp: Multiaddr = format!("/ip4/{}/udp/{}/quic-v1", listen_ip, addr.port())
        .parse()
        .map_err(|e| anyhow::anyhow!("parse listen addr: {e}"))?;
    swarm.listen_on(udp.clone()).map_err(|e| anyhow::anyhow!("listen quic {udp}: {e}"))?;

    let mut announced = Some(ready_tx);
    loop {
        match swarm.next().await {
            Some(SwarmEvent::NewListenAddr { address, .. }) if announced.is_some() => {
                let base = with_public_host(base(&address), addr.port(), public_host);
                let advertise = format!("{base}/p2p/{peer_id}");
                tracing::info!(%advertise, "relay ready — advertise address computed");
                // Notify the HTTP layer; ignore error if receiver dropped.
                let _ = announced.take().map(|tx| tx.send(advertise));
            }
            Some(SwarmEvent::NewListenAddr { address, .. }) => {
                tracing::debug!(%address, "relay additional listen addr");
            }
            Some(SwarmEvent::ConnectionEstablished { peer_id, .. }) => {
                tracing::info!(%peer_id, "relay connected");
            }
            Some(SwarmEvent::Behaviour(libp2p::relay::Event::ReservationReqAccepted {
                src_peer_id,
                renewed,
            })) => {
                tracing::info!(%src_peer_id, renewed, "relay reservation accepted");
            }
            Some(SwarmEvent::Behaviour(libp2p::relay::Event::CircuitReqAccepted {
                src_peer_id,
                dst_peer_id,
            })) => {
                tracing::info!(%src_peer_id, %dst_peer_id, "relay circuit");
            }
            Some(_) => {}
            None => break,
        }
    }
    Ok(())
}

/// Replace the listen address's IP with `public_host` (when given), keeping
/// whatever transport component the swarm reported (`/tcp/` or `/udp/...`).
/// Returns the base unchanged when there is no public host to substitute.
fn with_public_host(base: Multiaddr, _port: u16, public_host: Option<IpAddr>) -> Multiaddr {
    let Some(host) = public_host else {
        return base;
    };
    // Find the transport segment so we rebuild the right form.
    let is_udp = base.iter().any(|c| matches!(
        c,
        libp2p::multiaddr::Protocol::Udp(_)
    ));
    let port: u16 = base.iter().find_map(|c| match c {
        libp2p::multiaddr::Protocol::Tcp(p) => Some(p),
        libp2p::multiaddr::Protocol::Udp(p) => Some(p),
        _ => None,
    })
    .unwrap_or(_port);
    if is_udp {
        format!("/ip4/{host}/udp/{port}/quic-v1").parse().unwrap_or(base)
    } else {
        format!("/ip4/{host}/tcp/{port}").parse().unwrap_or(base)
    }
}

/// Strip trailing `/p2p/...` components so we only have the dial-able base.
fn base(addr: &Multiaddr) -> Multiaddr {
    let mut out = Multiaddr::empty();
    for p in addr.iter() {
        if matches!(p, libp2p::multiaddr::Protocol::P2p(_)) {
            break;
        }
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_advertise_uses_public_host() {
        let listen: Multiaddr = "/ip4/0.0.0.0/tcp/1883".parse().unwrap();
        let pub_ip: IpAddr = "139.159.201.87".parse().unwrap();
        let advertised = format!(
            "{}",
            with_public_host(base(&listen), 1883, Some(pub_ip))
        );
        assert_eq!(advertised, "/ip4/139.159.201.87/tcp/1883");
    }

    #[test]
    fn udp_advertise_uses_public_host() {
        let listen: Multiaddr = "/ip4/0.0.0.0/udp/1883/quic-v1".parse().unwrap();
        let pub_ip: IpAddr = "139.159.201.87".parse().unwrap();
        let advertised = format!(
            "{}",
            with_public_host(base(&listen), 1883, Some(pub_ip))
        );
        assert_eq!(advertised, "/ip4/139.159.201.87/udp/1883/quic-v1");
    }

    #[test]
    fn no_public_host_keeps_original() {
        let listen: Multiaddr = "/ip4/192.168.5.13/tcp/1883".parse().unwrap();
        let advertised = format!("{}", with_public_host(base(&listen), 1883, None));
        assert_eq!(advertised, "/ip4/192.168.5.13/tcp/1883");
    }
}
