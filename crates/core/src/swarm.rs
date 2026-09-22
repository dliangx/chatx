use crate::identity::DeviceIdentity;
use crate::message::{ChatRequest, ChatResponse};
use anyhow::Result;
pub use libp2p::gossipsub::IdentTopic;
use libp2p::futures::StreamExt as _;
use libp2p::mdns::tokio::Behaviour as Mdns;
use libp2p::mdns::{Config as MdnsConfig, Event as MdnsEvent};
use libp2p::request_response::json::Behaviour as Chat;
use libp2p::request_response::{
    Config as RrConfig, Event as RrEvent, Message as RrMessage, ProtocolSupport,
};
use libp2p::swarm::NetworkBehaviour;
use libp2p::swarm::{StreamProtocol, SwarmEvent};
use libp2p::{Multiaddr, PeerId, Swarm, SwarmBuilder};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub const TEXT_PROTOCOL: StreamProtocol = StreamProtocol::new("/p2pchat/text/1");

pub fn group_topic_name(group_id: &str) -> String {
    format!("/p2pchat/group/{}", group_id)
}

pub fn topic_to_group_id(topic: &str) -> Option<String> {
    let prefix = "/p2pchat/group/";
    topic.strip_prefix(prefix).map(|s| s.to_string()).filter(|s| !s.is_empty())
}

#[derive(NetworkBehaviour)]
#[behaviour(prelude = "libp2p::swarm::derive_prelude")]
pub struct Net {
    pub mdns: Mdns,
    pub chat: Chat<ChatRequest, ChatResponse>,
    pub group: libp2p::gossipsub::Behaviour,
}

#[derive(Debug, Clone)]
pub enum ChatEvent {
    Text {
        peer: PeerId,
        req: ChatRequest,
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
}

#[derive(Clone)]
pub struct Running {
    pub cmd_tx: mpsc::UnboundedSender<Cmd>,
    pub listen_addrs: Arc<Mutex<Vec<Multiaddr>>>,
    pub my_peer_id: PeerId,
}

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
    let chat: Chat<ChatRequest, ChatResponse> =
        Chat::new([(TEXT_PROTOCOL, ProtocolSupport::Full)], RrConfig::default());
    let net = Net {
        mdns,
        chat,
        group: libp2p::gossipsub::Behaviour::new(
            libp2p::gossipsub::MessageAuthenticity::Signed(device.keypair()),
            gossip_cfg,
        )
        .map_err(|e| anyhow::anyhow!("gossipsub init: {e}"))?,
    };

    let mut swarm = SwarmBuilder::with_existing_identity(device.keypair())
        .with_tokio()
        .with_tcp(
            Default::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )
        .expect("tcp+noise+yamux")
        .with_behaviour(|_k| net)?
        .build();

    let tcp0: Multiaddr = "/ip4/0.0.0.0/tcp/0".parse().unwrap();
    swarm
        .listen_on(tcp0)
        .map_err(|e| anyhow::anyhow!("listen tcp: {e}"))?;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    let listen_addrs: Arc<Mutex<Vec<Multiaddr>>> = Arc::new(Mutex::new(Vec::new()));

    tokio::spawn(swarm_loop(
        swarm,
        cmd_rx,
        evt_tx,
        Arc::clone(&listen_addrs),
    ));

    Ok((
        Arc::new(Running {
            cmd_tx,
            listen_addrs,
            my_peer_id: my_peer,
        }),
        evt_rx,
    ))
}

async fn swarm_loop(
    mut swarm: Swarm<Net>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    ev: mpsc::UnboundedSender<ChatEvent>,
    listen_addrs: Arc<Mutex<Vec<Multiaddr>>>,
) {
    loop {
        tokio::select! {
            maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                Some(Cmd::Connect { peer, addr }) => {
                    swarm.add_peer_address(peer, addr.clone());
                    let _ = swarm.dial(peer);
                }
                Some(Cmd::SendText { peer, from, e2e, text }) => {
                    let id: u64 = rand::random();
                    let req = ChatRequest {
                        id,
                        from,
                        e2e,
                        text: Some(text),
                        sealed: None,
                        kind: crate::message::MsgKind::Dm,
                        group_id: None,
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
                None => return,
            },
            Some(item) = swarm.next() => match item {
                SwarmEvent::NewListenAddr { address, .. } => {
                    listen_addrs.lock().unwrap().push(address.clone());
                    let _ = ev.send(ChatEvent::Listening(address));
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    let _ = ev.send(ChatEvent::Connected(peer_id));
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    let _ = ev.send(ChatEvent::Disconnected(peer_id));
                }
                SwarmEvent::Behaviour(inner) => {
                    let me = *swarm.local_peer_id();
                    match inner {
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
                                let _ = ev.send(ChatEvent::Text {
                                    peer,
                                    req: request,
                                });
                            }
                            RrMessage::Response { response, .. } => {
                                let _ = ev.send(ChatEvent::Acked {
                                    id: response.id,
                                    e2e: response.e2e,
                                });
                            }
                        },
                        RrEvent::OutboundFailure { peer, error, .. } => {
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
