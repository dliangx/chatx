//! Built-in libp2p circuit-relay v2 server.
//!
//! Runs as a task inside `chatx-signal`. Clients discover the relay address
//! automatically via `GET /v1/relays` (or manually via `CHATX_RELAYS`).
//! Peers that cannot dial each other directly (cross-NAT) then reach one
//! another through the relay. The relay only shuttles already-encrypted byte
//! streams — it never sees message plaintext nor holds any chatx keys.
//!
//! Deployment:
//!   cargo run -p chatx-signal                     # relay on 0.0.0.0:4039
//!   CHATX_RELAY_ADDR=0.0.0.0:5000 cargo run -p chatx-signal
//!   CHATX_RELAY_ADDR=off ...                      # disable

use libp2p::futures::StreamExt as _;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, SwarmBuilder};

/// Start the circuit-relay server. When the relay is listening (first
/// `NewListenAddr` event fires) the fully-qualified advertise multiaddr
/// (e.g. `/ip4/1.2.3.4/tcp/4039/p2p/12D3Koo...`) is sent via `ready_tx`.
/// The function blocks until the relay exits.
pub async fn start(
    addr: std::net::SocketAddr,
    ready_tx: tokio::sync::oneshot::Sender<String>,
) -> anyhow::Result<()> {
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
        .map_err(|e| anyhow::anyhow!("parse listen addr: {e}"))?;
    swarm.listen_on(tcp.clone()).map_err(|e| anyhow::anyhow!("listen {tcp}: {e}"))?;
    tracing::info!(%tcp, "relay listening");

    let mut announced = Some(ready_tx);
    loop {
        match swarm.next().await {
            Some(SwarmEvent::NewListenAddr { address, .. }) if announced.is_some() => {
                let base = base(&address);
                let advertise = format!("{base}/p2p/{peer_id}");
                tracing::info!(%address, %advertise, "relay ready — advertise address computed");
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
