use crate::identity::DeviceIdentity;
use crate::message::{AudioPayload, ChatRequest, ChatResponse};
use anyhow::Result;
use libp2p::futures::StreamExt as _;
pub use libp2p::gossipsub::IdentTopic;
use libp2p::mdns::tokio::Behaviour as Mdns;
use libp2p::mdns::{Config as MdnsConfig, Event as MdnsEvent};
use libp2p::request_response::json::Behaviour as Chat;
use libp2p::request_response::{
    Config as RrConfig, Event as RrEvent, Message as RrMessage, ProtocolSupport,
};
use libp2p::swarm::NetworkBehaviour;
use libp2p::swarm::{StreamProtocol, SwarmEvent};
use libp2p::upnp::tokio::Behaviour as Upnp;
use libp2p::{Multiaddr, PeerId, Swarm, SwarmBuilder};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

pub const TEXT_PROTOCOL: StreamProtocol = StreamProtocol::new("/chatx/text/1");

pub fn group_topic_name(group_id: &str) -> String {
    format!("/chatx/group/{}", group_id)
}

pub fn topic_to_group_id(topic: &str) -> Option<String> {
    let prefix = "/chatx/group/";
    topic
        .strip_prefix(prefix)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

#[derive(NetworkBehaviour)]
#[behaviour(prelude = "libp2p::swarm::derive_prelude")]
pub struct Net {
    pub mdns: Mdns,
    pub upnp: Upnp,
    pub relay: libp2p::relay::client::Behaviour,
    pub chat: Chat<ChatRequest, ChatResponse>,
    pub group: libp2p::gossipsub::Behaviour,
    // libp2p ping keeps the muxed TCP connection alive across idle periods:
    // a ping sub-stream is exchanged every 15s on every established connection,
    // so NAT / host firewalls don't time out a dormant yamux session. Without
    // it, an otherwise-idle P2P session gets reaped by the peer's NAT after a
    // few seconds — the exact "CONNECTED then DISCONNECTED" symptom we fixed.
    pub ping: libp2p::ping::Behaviour,
}

#[derive(Debug, Clone)]
pub enum ChatEvent {
    Text {
        peer: PeerId,
        req: ChatRequest,
    },

    Audio {
        peer: PeerId,
        rate: u32,
        ch: u32,
        data: Vec<u8>,
    },

    Webrtc {
        peer: PeerId,
        json: String,
    },
    Acked {
        id: u64,
        e2e: String,
    },
    Discovered {
        peer: PeerId,
        addr: Multiaddr,
    },
    SendFailed {
        peer: PeerId,
        reason: String,
    },
    Connected(PeerId),
    Disconnected(PeerId),
    Listening(Multiaddr),
    GroupPacket {
        topic: String,
        from: PeerId,
        data: Vec<u8>,
    },

    GroupMail {
        topic: String,
        from: PeerId,
        data: Vec<u8>,
        seq: u64,
    },
    GroupPeerSubscribed {
        peer: PeerId,
        topic: String,
    },
    GroupPeerUnsubscribed {
        peer: PeerId,
        topic: String,
    },
}

pub enum Cmd {
    Connect {
        peer: PeerId,
        addr: Multiaddr,
    },
    SendText {
        peer: PeerId,
        from: String,
        e2e: String,
        text: String,
        ack_tx: Option<oneshot::Sender<anyhow::Result<()>>>,
    },
    SendAudio {
        peer: PeerId,
        from: String,
        e2e: String,
        rate: u32,
        ch: u32,
        data: Vec<u8>,
    },
    SendWebrtc {
        peer: PeerId,
        from: String,
        e2e: String,
        payload: String,
    },
    GroupKeyDirect {
        peer: PeerId,
        from: String,
        e2e: String,
        group_id: String,
        sealed: String,
    },
    GroupSubscribe {
        group_id: String,
    },
    GroupUnsubscribe {
        group_id: String,
    },
    GroupPublish {
        topic: IdentTopic,
        data: Vec<u8>,
    },
    ReserveRelay {
        relay_peer: PeerId,
        relay_addr: Multiaddr,
    },
    /// Tear down every connection to this peer (and drop the ping handler
    /// that was keeping those conns alive). Used when a user leaves a group
    /// chat after the "grace" period, so we stop maintaining NAT mappings
    /// for members we no longer need to reach.
    Disconnect {
        peer: PeerId,
    },
}

#[derive(Clone)]
pub struct Running {
    pub cmd_tx: mpsc::UnboundedSender<Cmd>,
    pub listen_addrs: Arc<Mutex<Vec<Multiaddr>>>,
    pub my_peer_id: PeerId,
    pub pending_acks: Arc<Mutex<HashMap<u64, (PeerId, oneshot::Sender<anyhow::Result<()>>)>>>,
    pub connected: Arc<Mutex<HashMap<PeerId, u32>>>,
    pub dial_backoff: Arc<Mutex<HashMap<String, (u64, u32)>>>,
    pub dial_fail_log: Arc<Mutex<HashMap<String, u64>>>,
    pub relays: Arc<Mutex<Vec<(PeerId, Multiaddr)>>>,
    pub injected: mpsc::UnboundedSender<ChatEvent>,
}

/// True if `addr` is a local/loopback endpoint or one of our own advertised
/// listen addresses (e.g. a sibling device on this host, or a stale self
/// endpoint left in the directory) — dialing it would loop back to ourselves.
fn is_own_address(addr: &Multiaddr, running: &Running) -> bool {
    use libp2p::multiaddr::Protocol;
    for p in addr.iter() {
        match p {
            Protocol::Ip4(ip) => {
                if ip.is_loopback() {
                    return true;
                }
            }
            Protocol::Ip6(ip) => {
                if ip.is_loopback() {
                    return true;
                }
            }
            _ => {}
        }
    }
    running
        .listen_addrs
        .lock()
        .unwrap()
        .iter()
        .any(|a| a == addr)
}

pub fn split_relay_addr(full: &Multiaddr) -> Option<(PeerId, Multiaddr)> {
    let mut base = Multiaddr::empty();
    for p in full.iter() {
        match p {
            libp2p::multiaddr::Protocol::P2p(peer) => return Some((peer, base)),
            other => base.push(other),
        }
    }
    None
}

fn parse_relay_env() -> Vec<(PeerId, Multiaddr)> {
    let spec: std::collections::BTreeSet<String> = std::env::var("CHATX_RELAYS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut out = Vec::new();
    for s in spec {
        match s.parse::<Multiaddr>() {
            Ok(addr) => match split_relay_addr(&addr) {
                Some(pair) => out.push(pair),
                None => eprintln!("[relay] {s} has no /p2p/.../ component; ignored"),
            },
            Err(e) => eprintln!("[relay] {s}: {e}"),
        }
    }
    out
}

pub const MAX_DIALS_PER_TICK: usize = 64;
pub const DIAL_BACKOFF_BASE_MS: u64 = 10_000;
pub const DIAL_BACKOFF_MAX_MS: u64 = 300_000;
pub const FAIL_LOG_REPEAT_MS: u64 = 60_000;

pub type EventRx = mpsc::UnboundedReceiver<ChatEvent>;

pub async fn boot(
    device: &DeviceIdentity,
) -> Result<(Arc<Running>, mpsc::UnboundedReceiver<ChatEvent>)> {
    let my_peer = device.peer_id();
    let gossip_cfg = libp2p::gossipsub::ConfigBuilder::default()
        .allow_self_origin(true)
        .build()
        .map_err(|e| anyhow::anyhow!("gossipsub config: {e}"))?;
    let mdns = Mdns::new(MdnsConfig::default(), my_peer)?;
    let chat: Chat<ChatRequest, ChatResponse> = Chat::new(
        [(TEXT_PROTOCOL, ProtocolSupport::Full)],
        RrConfig::default(),
    );
    let group: libp2p::gossipsub::Behaviour = libp2p::gossipsub::Behaviour::new(
        libp2p::gossipsub::MessageAuthenticity::Signed(device.keypair()),
        gossip_cfg,
    )
    .map_err(|e| anyhow::anyhow!("gossipsub init: {e}"))?;
    let ping: libp2p::ping::Behaviour = libp2p::ping::Behaviour::new(
        libp2p::ping::Config::new()
            .with_interval(std::time::Duration::from_secs(15))
            .with_timeout(std::time::Duration::from_secs(20)),
    );

    let mut swarm = SwarmBuilder::with_existing_identity(device.keypair())
        .with_tokio()
        .with_tcp(
            Default::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )
        .expect("tcp+noise+yamux")
        .with_relay_client(libp2p::noise::Config::new, libp2p::yamux::Config::default)?
        .with_behaviour(move |_k, relay| Net {
            mdns,
            upnp: Upnp::default(),
            relay,
            chat,
            group,
            ping,
        })?
        .build();

    let tcp0: Multiaddr = "/ip4/0.0.0.0/tcp/0".parse().unwrap();
    swarm
        .listen_on(tcp0)
        .map_err(|e| anyhow::anyhow!("listen tcp: {e}"))?;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    let (injected_tx, injected_rx) = mpsc::unbounded_channel();
    let listen_addrs: Arc<Mutex<Vec<Multiaddr>>> = Arc::new(Mutex::new(Vec::new()));
    let pending_acks = Arc::new(Mutex::new(HashMap::new()));
    let connected = Arc::new(Mutex::new(HashMap::new()));
    let dial_backoff = Arc::new(Mutex::new(HashMap::new()));
    let dial_fail_log = Arc::new(Mutex::new(HashMap::new()));
    let relays = Arc::new(Mutex::new(parse_relay_env()));

    let me = *swarm.local_peer_id();
    for (peer, base) in relays.lock().unwrap().iter().cloned() {
        let circuit = base
            .with(libp2p::multiaddr::Protocol::P2p(peer))
            .with(libp2p::multiaddr::Protocol::P2pCircuit)
            .with(libp2p::multiaddr::Protocol::P2p(me));
        match swarm.listen_on(circuit.clone()) {
            Ok(_) => eprintln!("[swarm] reserving relay slot on {circuit}"),
            Err(e) => tracing::warn!("reserve relay {peer}: {e}"),
        }
    }

    let running = Arc::new(Running {
        cmd_tx: cmd_tx.clone(),
        listen_addrs: Arc::clone(&listen_addrs),
        my_peer_id: my_peer,
        pending_acks: Arc::clone(&pending_acks),
        connected: Arc::clone(&connected),
        dial_backoff: Arc::clone(&dial_backoff),
        dial_fail_log: Arc::clone(&dial_fail_log),
        relays: Arc::clone(&relays),
        injected: injected_tx.clone(),
    });

    // Fan injected (server-fanout / B-mode) events into the app event stream.
    let ev_fan = evt_tx.clone();
    tokio::spawn(async move {
        let mut rx = injected_rx;
        while let Some(evt) = rx.recv().await {
            if ev_fan.send(evt).is_err() {
                break;
            }
        }
    });

    tokio::spawn(swarm_loop(swarm, cmd_rx, evt_tx, running.clone()));

    Ok((running, evt_rx))
}

async fn swarm_loop(
    mut swarm: Swarm<Net>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    ev: mpsc::UnboundedSender<ChatEvent>,
    running: Arc<Running>,
) {
    let pending_acks = running.pending_acks.clone();
    loop {
        tokio::select! {
            maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                Some(Cmd::Connect { peer, addr }) => {
                    let own = is_own_address(&addr, &running);
                    if peer == *swarm.local_peer_id() {
                        eprintln!("[swarm] DIAL SKIPPED self peer={peer} addr={addr}");
                    } else if own {
                        // Dialing one of our own listen / confirmed-external
                        // addresses (e.g. a sibling device on this host, or a
                        // stale self endpoint in the directory) — skip.
                        eprintln!("[swarm] DIAL SKIPPED own addr={addr} peer={peer}");
                    } else {
                        eprintln!("[swarm] DIAL peer={peer} addr={addr}");
                        // Fresh endpoint comes from the directory (i.e. the peer
                        // just reported a possibly-changed NAT mapping). Reset
                        // our local dial backoff so we actually try — otherwise
                        // a stale `dial_backoff` entry pins us off the peer
                        // forever, which is the "A/B disconnect and can never
                        // reconnect" bug.
                        running
                            .dial_backoff
                            .lock()
                            .unwrap()
                            .remove(&peer.to_base58());
                        swarm.add_peer_address(peer, addr.clone());
                        let _ = swarm.dial(peer);
                    }
                }
                Some(Cmd::SendText {
                    peer,
                    from,
                    e2e,
                    text,
                    ack_tx,
                }) => {
                    let id: u64 = rand::random();
                    if let Some(tx) = ack_tx {
                        pending_acks
                            .lock()
                            .unwrap()
                            .insert(id, (peer, tx));
                    }
                    let req = ChatRequest {
                        id,
                        from,
                        e2e,
                        text: Some(text),
                        sealed: None,
                        kind: crate::message::MsgKind::Dm,
                        group_id: None,
                        audio: None,
                        signal: None,
                    };
                    let _rid = swarm.behaviour_mut().chat.send_request(&peer, req);
                }
                Some(Cmd::SendAudio {
                    peer,
                    from,
                    e2e,
                    rate,
                    ch,
                    data,
                }) => {
                    let id: u64 = rand::random();
                    let req = ChatRequest {
                        id,
                        from,
                        e2e,
                        text: None,
                        sealed: None,
                        kind: crate::message::MsgKind::Audio,
                        group_id: None,
                        audio: Some(AudioPayload {
                            rate,
                            ch,
                            data: crate::account::b64(&data),
                        }),
                        signal: None,
                    };
                    let _rid = swarm.behaviour_mut().chat.send_request(&peer, req);
                }
                Some(Cmd::SendWebrtc {
                    peer,
                    from,
                    e2e,
                    payload,
                }) => {
                    let id: u64 = rand::random();
                    let req = ChatRequest {
                        id,
                        from,
                        e2e,
                        text: None,
                        sealed: None,
                        kind: crate::message::MsgKind::Webrtc,
                        group_id: None,
                        audio: None,
                        signal: Some(payload),
                    };
                    let _rid = swarm.behaviour_mut().chat.send_request(&peer, req);
                }
                Some(Cmd::GroupKeyDirect {
                    peer,
                    from,
                    e2e,
                    group_id,
                    sealed,
                }) => {
                    let id: u64 = rand::random();
                    let req = ChatRequest {
                        id,
                        from,
                        e2e,
                        text: None,
                        sealed: Some(sealed),
                        kind: crate::message::MsgKind::GroupKey,
                        group_id: Some(group_id),
                        audio: None,
                        signal: None,
                    };
                    let _rid = swarm.behaviour_mut().chat.send_request(&peer, req);
                }
                Some(Cmd::GroupSubscribe { group_id }) => {
                    let t = IdentTopic::new(group_topic_name(&group_id));
                    let _ = swarm.behaviour_mut().group.subscribe(&t);
                }
                Some(Cmd::GroupUnsubscribe { group_id }) => {
                    let t = IdentTopic::new(group_topic_name(&group_id));
                    swarm.behaviour_mut().group.unsubscribe(&t);
                }
                Some(Cmd::GroupPublish { topic, data }) => {
                    let _ = swarm.behaviour_mut().group.publish(topic, data);
                }
                Some(Cmd::ReserveRelay { relay_peer, relay_addr }) => {
                    let me = *swarm.local_peer_id();
                    let circuit = relay_addr
                        .clone()
                        .with(libp2p::multiaddr::Protocol::P2p(relay_peer))
                        .with(libp2p::multiaddr::Protocol::P2pCircuit)
                        .with(libp2p::multiaddr::Protocol::P2p(me));
                    if let Err(e) = swarm.listen_on(circuit.clone()) {
                        tracing::warn!("reserve relay {relay_peer}: {e}");
                    } else {
                        eprintln!("[swarm] reserve relay via {circuit}");
                    }
                }
                Some(Cmd::Disconnect { peer }) => {
                    // `disconnect_peer_id` returns `Ok(())` if a live conn was
                    // closed, `Err(())` if the peer was not connected (no-op).
                    // Both outcomes are fine — we intentionally do not remove
                    // the peer from the address book, so mDNS / identify can
                    // rediscover them if the user re-enters the chat.
                    match swarm.disconnect_peer_id(peer) {
                        Ok(()) => eprintln!("[swarm] DISCONNECTED {peer} (via cmd)"),
                        Err(()) => eprintln!("[swarm] DISCONNECT {peer}: was not connected"),
                    }
                }
                None => return,
            },
            Some(item) = swarm.next() => match item {
                SwarmEvent::IncomingConnection { local_addr, .. } => {
                    // Liveness probe for the accept path: if this stops
                    // appearing after a disconnect, the libp2p acceptor task
                    // itself has stopped (the rest of the runtime is alive
                    // because the loop still spins and we still see other
                    // events).
                    eprintln!("[swarm] INCOMING conn on {local_addr:?}");
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    running.listen_addrs.lock().unwrap().push(address.clone());
                    let _ = ev.send(ChatEvent::Listening(address));
                }
                // UPnP / relay-confirmed external addresses (e.g.
                // `/ip4/{public}/tcp/{port}` or a `/p2p-circuit/{me}` form).
                // Push into the local address set so `endpoints()` and the
                // heartbeat/`refresh` path re-announces them to the directory.
                SwarmEvent::ExternalAddrConfirmed { address } => {
                    let mut addrs = running.listen_addrs.lock().unwrap();
                    if !addrs.iter().any(|a| a == &address) {
                        addrs.push(address.clone());
                    }
                    eprintln!("[swarm] EXTERNAL ADDR {address}");
                }
                SwarmEvent::ExternalAddrExpired { address } => {
                    running
                        .listen_addrs
                        .lock()
                        .unwrap()
                        .retain(|a| a != &address);
                    eprintln!("[swarm] EXTERNAL ADDR EXPIRED {address}");
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    eprintln!("[swarm] CONNECTED to {peer_id}");
                    *running.connected.lock().unwrap().entry(peer_id).or_insert(0) += 1;
                    running.dial_backoff.lock().unwrap().remove(&peer_id.to_base58()); // reachable again — clear backoff
                    let _ = ev.send(ChatEvent::Connected(peer_id));
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    eprintln!("[swarm] DISCONNECTED from {peer_id}");
                    // Hold the `connected` guard ONCE. Re-locking the same
                    // std::sync::Mutex while the `if let` guard is still alive
                    // is a self-deadlock (Mutex is not re-entrant): it used to
                    // fire here the moment a peer's connection count dropped to
                    // 0, permanently freezing the whole swarm loop (no more
                    // heartbeats, dead acceptor) — the root cause of the
                    // "A can never reconnect to B" bug.
                    {
                        let mut m = running.connected.lock().unwrap();
                        if let Some(n) = m.get_mut(&peer_id) {
                            *n = n.saturating_sub(1);
                            if *n == 0 {
                                m.remove(&peer_id);
                            }
                        }
                    }
                    let _ = ev.send(ChatEvent::Disconnected(peer_id));
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    if let Some(peer_id) = peer_id {
                        // Same peer can be re-dialed repeatedly (backoff, group
                        // ticks). Don't spam the same error line on every retry —
                        // log at most once per peer per FAIL_LOG_REPEAT_MS.
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let key = peer_id.to_base58();
                        let suppress = {
                            let m = running.dial_fail_log.lock().unwrap();
                            m.get(&key)
                                .map(|last| now.saturating_sub(*last) < FAIL_LOG_REPEAT_MS)
                                .unwrap_or(false)
                        };
                        if !suppress {
                            running
                                .dial_fail_log
                                .lock()
                                .unwrap()
                                .insert(key.clone(), now);
                            eprintln!("[swarm] DIAL FAILED peer={key} err={error:?}");
                        }
                    }
                }
                SwarmEvent::Behaviour(inner) => {
                    let me = *swarm.local_peer_id();
                    match inner {
                    // UPnP: a NAT gateway confirmed an external mapping for our
                    // listener. This surfaces as `ExternalAddrConfirmed` above
                    // (libp2p-swarm aggregates it), so nothing extra to do here.
                    NetEvent::Upnp(_) => {}
                    // Ping success/failure only drives the ping handler's
                    // own connection-drop logic (keep_alive + max_failures);
                    // we don't inspect it here.
                    NetEvent::Ping(_) => {}
                    // Relay client: reservation accepted / circuit established.
                    // The resulting `/p2p-circuit/{me}` endpoint likewise reaches
                    // us via `ExternalAddrConfirmed`, so we only log here.
                    NetEvent::Relay(r) => {
                        if let libp2p::relay::client::Event::ReservationReqAccepted {
                            relay_peer_id, ..
                        } = r
                        {
                            eprintln!("[swarm] RELAY reservation accepted from {relay_peer_id}");
                        }
                    }
                    NetEvent::Mdns(m) => {
                        if let MdnsEvent::Discovered(ips) = m {
                            for (p, addr) in ips {
                                swarm.add_peer_address(p, addr.clone());
                                let _ = ev.send(ChatEvent::Discovered {
                                    peer: p,
                                    addr,
                                });
                            }
                        }
                    }
                    NetEvent::Chat(c) => match c {
                        RrEvent::Message { peer, message, .. } => match message {
                            RrMessage::Request {
                                request, channel, ..
                            } => {
                                let resp = ChatResponse {
                                    id: request.id,
                                    e2e: String::new(),
                                };
                                let _ =
                                    swarm.behaviour_mut().chat.send_response(channel, resp);
                                if request.kind == crate::message::MsgKind::Webrtc {
                                    if let Some(json) = request.signal {
                                        let _ = ev.send(ChatEvent::Webrtc { peer, json });
                                    }
                                } else if request.kind == crate::message::MsgKind::Audio {
                                    if let Some(ap) = request.audio {
                                        if let Some(data) = crate::account::from_b64(&ap.data) {
                                            let _ = ev.send(ChatEvent::Audio {
                                                peer,
                                                rate: ap.rate,
                                                ch: ap.ch,
                                                data,
                                            });
                                        }
                                    }
                                } else {
                                    let _ = ev.send(ChatEvent::Text {
                                        peer,
                                        req: request,
                                    });
                                }
                            }
                            RrMessage::Response { response, .. } => {
                                // Peer acked this request — resolve its waiter (if any).
                                if let Some((_peer, tx)) =
                                    pending_acks.lock().unwrap().remove(&response.id)
                                {
                                    eprintln!("[swarm] ACKED id={}", response.id);
                                    let _ = tx.send(Ok(()));
                                }
                                let _ = ev.send(ChatEvent::Acked {
                                    id: response.id,
                                    e2e: response.e2e,
                                });
                            }
                        },
                        RrEvent::OutboundFailure { peer, error, .. } => {
                            eprintln!("[swarm] SEND FAILED peer={peer} err={error:?}");
                            // Fail every in-flight waiter addressed to this peer.
                            let mut m = pending_acks.lock().unwrap();
                            let ids: Vec<u64> = m
                                .iter()
                                .filter(|(_, (p, _))| *p == peer)
                                .map(|(id, _)| *id)
                                .collect();
                            for id in ids {
                                if let Some((_p, tx)) = m.remove(&id) {
                                    let _ = tx.send(Err(anyhow::anyhow!(
                                        "send to {peer} failed: {error:?}"
                                    )));
                                }
                            }
                            let _ = ev.send(ChatEvent::SendFailed {
                                peer,
                                reason: format!("{error:?}"),
                            });
                        }
                        _ => {}
                    },
                    NetEvent::Group(g) => match g {
                        libp2p::gossipsub::Event::Message { message, .. } => {
                            let topic = message.topic.into_string();
                            let from = message
                                .source
                                .unwrap_or(me);
                            let _ = ev.send(ChatEvent::GroupPacket {
                                topic,
                                from,
                                data: message.data,
                            });
                        }
                        libp2p::gossipsub::Event::Subscribed { peer_id, topic } => {
                            let _ = ev.send(ChatEvent::GroupPeerSubscribed {
                                peer: peer_id,
                                topic: topic.into_string(),
                            });
                        }
                        libp2p::gossipsub::Event::Unsubscribed { peer_id, topic } => {
                            let _ = ev.send(ChatEvent::GroupPeerUnsubscribed {
                                peer: peer_id,
                                topic: topic.into_string(),
                            });
                        }
                        _ => {}
                    },
                    };
                },
                _ => {}
            }
        }
    }
}
