//! Built-in libp2p circuit-relay v2 server.
//!
//! Runs as a task inside `chatx-signal`. Clients point at it via
//! `CHATX_RELAYS=/ip4/<host>/tcp/<port>/p2p/<peer_id>` and reserve a slot;
//! peers that cannot dial each other directly (cross-NAT) then reach one
//! another through it. The relay only shuttles already-encrypted byte
//! streams — it never sees message plaintext nor holds any chatx keys.
//!
//! Deployment:
//!   cargo run -p chatx-signal                     # 0.0.0.0:4039
//!   CHATX_RELAY_ADDR=0.0.0.0:5000 cargo run -p chatx-signal
//!   CHATX_RELAY_ADDR=off ...                      # disable
//! then copy the printed advertised multiaddr into each client's CHATX_RELAYS.

use libp2p::futures::StreamExt as _;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, SwarmBuilder};

/// Run the circuit-relay server until it fails.
pub async fn serve(addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let peer_id: PeerId = keypair.public().to_peer_id();
    tracing::info!(%peer_id, "relay peer id");

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(Default::default(), libp2p::noise::Config::new, libp2p::yamux::Config::default)
        .expect("tcp+noise+yamux")
        .with_behaviour(|_k| libp2p::relay::Behaviour::new(peer_id, libp2p::relay::Config::default()))?
        .build();

    let tcp: Multiaddr = format!("/ip4/{}/tcp/{}", addr.ip(), addr.port())
        .parse()
        .unwrap();
    swarm
        .listen_on(tcp.clone())
        .map_err(|e| anyhow::anyhow!("listen {tcp}: {e}"))?;

    tracing::info!(%tcp, "relay listening");

    loop {
        match swarm.next().await {
            Some(SwarmEvent::NewListenAddr { address, .. }) => {
                let base = base(&address);
                tracing::info!(%address, "relay listening");
                tracing::info!(advertise = %format!("{base}/p2p/{peer_id}"), "relay advertise to clients as");
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

/// Strip trailing `/p2p/...` components so we only print the dial-able base.
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
