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
use std::num::NonZeroU32;
use std::time::Duration;
use libp2p::futures::StreamExt as _;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{Multiaddr, PeerId, SwarmBuilder};

/// Compose the relay v2 behaviour with the AutoNAT server side, so the same
/// listener also answers AutoNAT dial-back probes from clients (which need a
/// peer with a *global* IP that the client itself is reaching — which is
/// exactly the situation when the client has dialed the relay).
#[derive(NetworkBehaviour)]
#[behaviour(prelude = "libp2p::swarm::derive_prelude")]
struct RelayNet {
    relay: libp2p::relay::Behaviour,
    autonat: libp2p::autonat::Behaviour,
}

/// The libp2p relay defaults are tuned for low-concurrency public relays and
/// are far too tight for a *private* chatx relay that has to bridge DM media
/// (megabytes of image/file/voice) plus the DCUtR hole-punch control stream
/// for every peer conversation. Out-of-the-box values:
///   max_circuits = 16, max_circuits_per_peer = 4,
///   max_circuit_duration = 120s, max_circuit_bytes = 128 KiB,
///   circuit rate limiters = 30/peer/2min and 60/ip/minute.
/// Under the client's heavy dial/retry churn those limits trip and the relay
/// answers `RESOURCE_LIMIT_EXCEEDED`, killing both the media stream and the
/// hole-punch. Relax them to comfortably fit a small trusted user base while
/// remaining bounded (this is not a public abuse-prone relay).
fn relay_config() -> libp2p::relay::Config {
    let mut cfg = libp2p::relay::Config {
        max_reservations: 512,
        max_reservations_per_peer: 8,
        max_circuits: 512,
        max_circuits_per_peer: 16,
        max_circuit_duration: Duration::from_secs(30 * 60),
        // Advisory budget the relay advertises to clients; raised so large
        // media frames are not flagged as over-budget upstream.
        max_circuit_bytes: 1 << 28, // 256 MiB
        ..libp2p::relay::Config::default()
    };

    // Replace the tight default circuit rate limiters with generous ones
    // (600/min per peer and per IP) so bursty dial churn no longer trips them.
    cfg.circuit_src_rate_limiters.clear();
    let per = |n: u32| NonZeroU32::new(n).expect("n > 0");
    cfg = cfg
        .circuit_src_per_peer(per(600), Duration::from_secs(60))
        .circuit_src_per_ip(per(600), Duration::from_secs(60));

    cfg
}

/// Resolve the public hosts to use in the announced advertise addresses,
/// replacing "locally non-dialable" IPs (`0.0.0.0` any, `127.*` loopback,
/// private-LAN 10/172.16-31/192.168) that are meaningless to a client on
/// **another network**.
///
/// `advertise_env` is the name of the environment variable the operator sets
/// to pin the public host (e.g. `CHATX_RELAY_ADVERTISE` for the relay,
/// `CHATX_STUN_ADVERTISE` for the STUN server) — the logic is identical for
/// both, only the knob name differs.
///
/// Resolution priority:
/// 1. `$advertise_env=<public-host:port>` (explicit operator config, most reliable)
/// 2. Egress probe (v4 → 1.1.1.1, v6 → `2001:4860:4860::8888`) — yields the
///    public IP of each family when running on a bare public line, but still
///    a private IP behind a cloud VPC or NAT
///
/// Returns a de-duplicated list of (v4, v6) public IPs in that order, empty
/// when no public host could be determined for a family (a warn is emitted above).
pub async fn resolve_public_hosts(advertise_env: &str) -> (Option<IpAddr>, Option<IpAddr>) {
    if let Ok(spec) = std::env::var(advertise_env) {
        let s = spec.trim().to_ascii_lowercase();
        if !s.is_empty() {
            let host = spec
                .split_once(':')
                .map(|(h, _)| h.to_string())
                .unwrap_or(spec.to_string());
            if let Ok(ip) = host.parse::<IpAddr>() {
                tracing::info!(%ip, env = advertise_env, "advertise IP from operator env");
                return match ip {
                    IpAddr::V4(_) => (Some(ip), None),
                    IpAddr::V6(_) => (None, Some(ip)),
                };
            }
            let addrs: Vec<_> = tokio::net::lookup_host(&host).await.ok().map(|it| it.collect()).unwrap_or_default();
            let mut picked: [Option<IpAddr>; 2] = [None; 2];
            for a in addrs.iter() {
                let slot = match a.ip() {
                    IpAddr::V4(_) => 0usize,
                    IpAddr::V6(_) => 1,
                };
                if picked[slot].is_none() {
                    picked[slot] = Some(a.ip());
                }
            }
            if picked[0].is_some() || picked[1].is_some() {
                for p in picked.iter().flatten() {
                    tracing::info!(%p, %host, env = advertise_env, "advertise IP resolved via DNS");
                }
                return (picked[0], picked[1]);
            }
            tracing::warn!(host, env = advertise_env, "advertise host not resolvable; falling back to egress probe");
        }
    }
    (probe_egress_v4().await, probe_egress_v6().await)
}

/// Egress probe for IPv4: UDP `connect` to 1.1.1.1 and read the local source IP.
async fn probe_egress_v4() -> Option<IpAddr> {
    let out = tokio::task::spawn_blocking(|| {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        sock.connect("1.1.1.1:53").ok()?;
        let local = sock.local_addr().ok()?;
        Some(local.ip())
    })
    .await
    .ok()
    .flatten();
    match out {
        Some(ip) if is_public_ip(ip) => {
            tracing::info!(%ip, "advertise IPv4 inferred from egress probe");
            Some(ip)
        }
        Some(ip) => {
            tracing::warn!(
                %ip,
                "egress probe returned a private IPv4 — the service is behind NAT \
                 and cannot be reached by cross-network peers; set the \
                 corresponding advertise env var to the public IP"
            );
            None
        }
        None => None,
    }
}

/// Egress probe for IPv6: UDP `connect` to `2001:4860:4860::8888` (Google DNS)
/// and read the local source IP.
async fn probe_egress_v6() -> Option<IpAddr> {
    let out = tokio::task::spawn_blocking(|| {
        let sock = std::net::UdpSocket::bind("[::]:0").ok()?;
        sock.connect("[2001:4860:4860::8888]:53").ok()?;
        let local = sock.local_addr().ok()?;
        Some(local.ip())
    })
    .await
    .ok()
    .flatten();
    match out {
        Some(ip) if is_public_ip(ip) => {
            tracing::info!(%ip, "advertise IPv6 inferred from egress probe");
            Some(ip)
        }
        Some(ip) => {
            tracing::warn!(
                %ip,
                "egress probe returned a non-public IPv6 — set the \
                 corresponding advertise env var to the public address"
            );
            None
        }
        None => None,
    }
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_unspecified()
                && !v4.is_private()
                && !v4.is_link_local()
        }
        // IPv6: keep only addresses that are not loopback / unspecified /
        // link-local / ULA (fc00::/7) / site-local (fec0::/10).
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_unicast_link_local()
                && !v6.is_unique_local()
        }
    }
}

/// Start the circuit-relay server. When the relay is listening (first
/// `NewListenAddr` event fires) the list of advertise multiaddrs (v4 + v6
/// when both are configured; v4-only otherwise) is sent via `ready_tx`.
/// The function blocks until the relay exits.
pub async fn start(
    addr: std::net::SocketAddr,
    ready_tx: tokio::sync::oneshot::Sender<Vec<String>>,
) -> anyhow::Result<()> {
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let peer_id: PeerId = keypair.public().to_peer_id();
    tracing::info!(%peer_id, "relay peer id");

    // Public hosts used in the advertise multiaddrs that clients dial. This
    // is resolved before the listen loop so we can replace the listen
    // address's IP (which is `0.0.0.0` / private by default) with something
    // routable. `None` for a family ⇒ announce without it (warn above).
    let (public_v4, public_v6) = resolve_public_hosts("CHATX_RELAY_ADVERTISE").await;

    // AutoNAT server config — this side is *only* answering inbound probes,
    // so the client-side knobs (boot_delay, retry_interval) are irrelevant to
    // us, but the server-side knobs matter: `throttle_clients_global_max` and
    // `throttle_clients_peer_max` bound how many clients we can answer per
    // second. Relays see many clients probing at once (a new app boot does
    // not rate-limit the autonat probe), so raise these to a generous but
    // still bounded value. `only_global_ips: false` because the client we
    // are answering is, from the relay's viewpoint, an *inbound* connection
    // whose public-vs-private nature is exactly what we are probing.
    let autonat_cfg = libp2p::autonat::Config {
        throttle_clients_global_max: 600,
        throttle_clients_peer_max: 60,
        only_global_ips: false,
        ..libp2p::autonat::Config::default()
    };
    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(Default::default(), libp2p::noise::Config::new, libp2p::yamux::Config::default)
        .expect("tcp+noise+yamux")
        .with_quic()
        .with_behaviour(|_k| RelayNet {
            relay: libp2p::relay::Behaviour::new(peer_id, relay_config()),
            autonat: libp2p::autonat::Behaviour::new(peer_id, autonat_cfg),
        })?
        .build();

    // Bind the relay to the families the operator's advertise config
    // (or the egress probe) reported. v4 is always kept as a fallback (it is
    // what most clients can reach anyway); v6 is added when the operator's
    // `CHATX_RELAY_ADVERTISE` resolves to an IPv6 address.
    //
    // The listen multiaddrs use `0.0.0.0` / `[::]` so the OS picks the
    // right interface; only the *advertisements* (which clients dial) carry
    // the public IP(s).
    let port = addr.port();
    // v4 is always bound — it is the fallback most clients can reach. v6 is
    // bound only when the operator's advertise config (or the egress probe)
    // actually resolved to an IPv6 address; binding `[::]` on hosts without
    // IPv6 just produces a noisy warn, so we skip it in that case.
    let wants_v4 = true;
    let wants_v6 = public_v6.is_some();
    let mut listens: Vec<String> = Vec::new();
    if wants_v4 {
        listens.push(format!("/ip4/0.0.0.0/tcp/{port}"));
        listens.push(format!("/ip4/0.0.0.0/udp/{port}/quic-v1"));
    }
    if wants_v6 {
        listens.push(format!("/ip6/::/tcp/{port}"));
        listens.push(format!("/ip6/::/udp/{port}/quic-v1"));
    }
    for spec in &listens {
        let m = spec.parse::<Multiaddr>().map_err(|e| anyhow::anyhow!("parse {spec}: {e}"))?;
        if let Err(e) = swarm.listen_on(m) {
            tracing::warn!(%spec, %e, "listen skipped (family not available?); continuing");
        }
    }

    // CRITICAL for reservation: the relay v2 server must include the client's
    // observed address in the reservation success response. It sources that
    // list from `swarm.external_addresses` (see `libp2p::relay::Behaviour` →
    // `AcceptReservationReq { addrs: self.external_addresses … }`). When we
    // only listen on `0.0.0.0`/private addrs and never register an external
    // address, that list is empty, the client sees
    // `NoAddressesInReservation`, and it drops the circuit listener — so no
    // `/p2p-circuit` endpoint ever surfaces.
    // Register each public family (v4 and/or v6) for both transports so the
    // handshake completes for whichever address the client actually dialed.
    let mut registered = false;
    for (family, host) in [("ip4", public_v4), ("ip6", public_v6)] {
        let Some(host) = host else { continue };
        for spec in [format!("{family}/{host}/tcp/{port}"), format!("{family}/{host}/udp/{port}/quic-v1")] {
            if let Ok(m) = spec.parse::<Multiaddr>() {
                swarm.add_external_address(m.clone());
                tracing::info!(%m, "relay registered external address");
                registered = true;
            }
        }
    }
    if !registered {
        tracing::warn!(
            "no public host — relay reservation responses will carry no address \
             and clients will fail with NoAddressesInReservation. Set \
             CHATX_RELAY_ADVERTISE=<public-host:port>"
        );
    }

    let mut announced = Some(ready_tx);
    loop {
        match swarm.next().await {
            Some(SwarmEvent::NewListenAddr { .. }) if announced.is_some() => {
                let addresses = advertise_addresses(port, public_v4, public_v6, &peer_id);
                for a in &addresses {
                    tracing::info!(%a, "relay ready — advertise address computed");
                }
                if let Some(ready_tx) = announced.take() {
                    let _ = ready_tx.send(addresses);
                }
            }
            Some(SwarmEvent::NewListenAddr { address, .. }) => {
                tracing::debug!(%address, "relay additional listen addr");
            }
            Some(SwarmEvent::ConnectionEstablished { peer_id, .. }) => {
                tracing::info!(%peer_id, "relay connected");
            }
            Some(SwarmEvent::Behaviour(relay_ev)) => match relay_ev {
                RelayNetEvent::Relay(libp2p::relay::Event::ReservationReqAccepted {
                    src_peer_id,
                    renewed,
                }) => {
                    tracing::info!(%src_peer_id, renewed, "relay reservation accepted");
                }
                RelayNetEvent::Relay(libp2p::relay::Event::CircuitReqAccepted {
                    src_peer_id,
                    dst_peer_id,
                }) => {
                    tracing::info!(%src_peer_id, %dst_peer_id, "relay circuit");
                }
                RelayNetEvent::Autonat(libp2p::autonat::Event::InboundProbe(p)) => {
                    tracing::debug!(?p, "relay autonat inbound probe (acting as probe server)");
                }
                RelayNetEvent::Autonat(libp2p::autonat::Event::OutboundProbe(p)) => {
                    tracing::debug!(?p, "relay autonat outbound probe");
                }
                _ => {}
            },
            Some(_) => {}
            None => break,
        }
    }
    Ok(())
}

/// Build the full advertise multiaddr list for a running relay: one entry
/// per (family × transport), each of the form
///
///   /ip4/<host>/tcp/<port>/p2p/<peer_id>
///   /ip4/<host>/udp/<port>/quic-v1/p2p/<peer_id>
///
/// (with the `/ip6/...` counterpart when `public_v6` is `Some`). The order
/// is deterministic: v4-tcp, v4-udp, v6-tcp, v6-udp — so the first entry in
/// the list is always the family the client is most likely to be able to dial.
fn advertise_addresses(
    port: u16,
    public_v4: Option<IpAddr>,
    public_v6: Option<IpAddr>,
    peer_id: &PeerId,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(host) = public_v4 {
        out.push(format!("/ip4/{host}/tcp/{port}/p2p/{peer_id}"));
        out.push(format!("/ip4/{host}/udp/{port}/quic-v1/p2p/{peer_id}"));
    }
    if let Some(host) = public_v6 {
        out.push(format!("/ip6/{host}/tcp/{port}/p2p/{peer_id}"));
        out.push(format!("/ip6/{host}/udp/{port}/quic-v1/p2p/{peer_id}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> PeerId {
        libp2p::identity::Keypair::generate_ed25519().public().to_peer_id()
    }

    #[test]
    fn v4_only_yields_two_entries() {
        let pub4: IpAddr = "139.159.201.87".parse().unwrap();
        let p = peer();
        let addrs = advertise_addresses(1883, Some(pub4), None, &p);
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], format!("/ip4/{pub4}/tcp/1883/p2p/{p}"));
        assert_eq!(addrs[1], format!("/ip4/{pub4}/udp/1883/quic-v1/p2p/{p}"));
    }

    #[test]
    fn v6_only_yields_two_entries() {
        let pub6: IpAddr = "2001:db8::1".parse().unwrap();
        let p = peer();
        let addrs = advertise_addresses(1883, None, Some(pub6), &p);
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], format!("/ip6/{pub6}/tcp/1883/p2p/{p}"));
        assert_eq!(addrs[1], format!("/ip6/{pub6}/udp/1883/quic-v1/p2p/{p}"));
    }

    #[test]
    fn dual_stack_yields_four_entries_v4_first() {
        let pub4: IpAddr = "139.159.201.87".parse().unwrap();
        let pub6: IpAddr = "2001:db8::1".parse().unwrap();
        let p = peer();
        let addrs = advertise_addresses(1883, Some(pub4), Some(pub6), &p);
        assert_eq!(addrs.len(), 4);
        assert!(addrs[0].starts_with("/ip4/"));
        assert!(addrs[2].starts_with("/ip6/"));
    }

    #[test]
    fn no_public_hosts_yields_empty_list() {
        let p = peer();
        let addrs = advertise_addresses(1883, None, None, &p);
        assert!(addrs.is_empty());
    }
}
