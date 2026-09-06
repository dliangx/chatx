//! libp2p 网络层（M2 v4）：TCP + Noise + Yamux + mDNS + request-response 文本聊天。
use crate::identity::DeviceIdentity;
use crate::message::{ChatRequest, ChatResponse};
use anyhow::Result;
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

#[derive(NetworkBehaviour)]
#[behaviour(prelude = "libp2p::swarm::derive_prelude")]
pub struct Net {
    pub mdns: Mdns,
    pub chat: Chat<ChatRequest, ChatResponse>,
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
}

/// 可跨线程共享的运行态句柄（只含 Send+Sync 组件）。
#[derive(Clone)]
pub struct Running {
    pub cmd_tx: mpsc::UnboundedSender<Cmd>,
    pub listen_addrs: Arc<Mutex<Vec<Multiaddr>>>,
    pub my_peer_id: PeerId,
}

/// 事件接收端（独占，交事件泵）。
pub type EventRx = mpsc::UnboundedReceiver<ChatEvent>;

/// 启动 swarm；返回共享句柄 + 事件接收端。
pub async fn boot(
    device: &DeviceIdentity,
) -> Result<(Arc<Running>, mpsc::UnboundedReceiver<ChatEvent>)> {
    let my_peer = device.peer_id();
    let mdns = Mdns::new(MdnsConfig::default(), my_peer)?;
    let chat: Chat<ChatRequest, ChatResponse> =
        Chat::new([(TEXT_PROTOCOL, ProtocolSupport::Full)], RrConfig::default());
    let net = Net { mdns, chat };

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
                    };
                    let _rid = swarm.behaviour_mut().chat.send_request(&peer, req);
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
                SwarmEvent::Behaviour(inner) => match inner {
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
                },
                _ => {}
            }
        }
    }
}
