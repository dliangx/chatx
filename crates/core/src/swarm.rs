use crate::identity::DeviceIdentity;
use crate::message::{
    AudioPayload, ChatRequest, ChatResponse, FileAck, FileRequest, ImageAck, ImageRequest,
    VoiceAck, VoiceRequest,
};
use crate::moment::{PostRequest, PostResponse};
use anyhow::Result;
use libp2p::futures::StreamExt as _;
pub use libp2p::gossipsub::IdentTopic;
use libp2p::mdns::tokio::Behaviour as Mdns;
use libp2p::mdns::{Config as MdnsConfig, Event as MdnsEvent};
use libp2p::request_response::cbor::{self, Behaviour as Blob};
use libp2p::request_response::json::Behaviour as Chat;
use libp2p::request_response::{
    Config as RrConfig, Event as RrEvent, Message as RrMessage, ProtocolSupport, ResponseChannel,
};
use libp2p::swarm::NetworkBehaviour;
use libp2p::swarm::{StreamProtocol, SwarmEvent};
use libp2p::upnp::tokio::Behaviour as Upnp;
use libp2p::{Multiaddr, PeerId, Swarm, SwarmBuilder};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

pub const TEXT_PROTOCOL: StreamProtocol = StreamProtocol::new("/chatx/text/1");
pub const MOMENT_PROTOCOL: StreamProtocol = StreamProtocol::new("/chatx/moment/1");
/// Binary (CBOR) voice-message transport. The CBOR codec serializes the
/// raw PCM payload as a compact byte-string — no base64 expansion, no JSON
/// envelope — so the on-wire size equals the clip size.
pub const VOICE_PROTOCOL: StreamProtocol = StreamProtocol::new("/chatx/voice/1");

/// Binary (CBOR) photo-message transport — raw image bytes as a byte-string.
pub const IMAGE_PROTOCOL: StreamProtocol = StreamProtocol::new("/chatx/image/1");

/// Binary (CBOR) document-file transport — raw file bytes (pdf/docx/xlsx/pptx) + sender basename as a byte-string.
pub const FILE_PROTOCOL: StreamProtocol = StreamProtocol::new("/chatx/file/1");

/// Hard ceiling for a single inbound (or outbound) voice request. ~64 MiB
/// covers the 5-minute capture cap (`MAX_FRAMES`, ~4.9 MB raw) with wide
/// margin while still bounding the receiver's read buffer.
const VOICE_REQ_MAX: u64 = 64 * 1024 * 1024;

/// Ceiling for a single inbound photo request (photos are capped server-side
/// before sending, but keep a generous margin for large originals).
const IMAGE_REQ_MAX: u64 = 64 * 1024 * 1024;

/// Ceiling for one inbound document-file request (app-side caps picked files well below this; mirrors the wasm sandbox's `MAX_FILE_BYTES`).
const FILE_REQ_MAX: u64 = 64 * 1024 * 1024;

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
    /// DCUtR — coordinates relay→direct hole-punching. Once the relay
    /// circuit is up, `/libp2p/dcutr` runs over it: peers exchange observed
    /// public addresses (CONNECT) and clock offsets (SYNC), then dial each
    /// other's public UDP simultaneously (QUIC simultaneous-open does the
    /// actual punching). On success the connection is demoted from
    /// `/p2p-circuit` to a direct QUIC address automatically.
    pub dcutr: libp2p::dcutr::Behaviour,
    /// AutoNAT — dial-back probes to peer nodes that answer "can you see me?"
    /// with the address they actually connected on. Ground-truth reachability
    /// verdict: `Public(addr)` means an outside node dialed our advertised
    /// candidate successfully (EIM-friendly NAT / UPnP / public), `Private`
    /// means every probe was refused (symmetric NAT, UDP blocked, CGNAT).
    /// Combined with the dual-socket STUN fingerprint in [`crate::stun`] this
    /// tells the caller which hole-punching matrix cell it sits in — see
    /// `endpoints_with_egress` for how the two signals are fused.
    pub autonat: libp2p::autonat::Behaviour,
    pub chat: Chat<ChatRequest, ChatResponse>,
    pub group: libp2p::gossipsub::Behaviour,
    // libp2p ping keeps the muxed TCP connection alive across idle periods:
    // a ping sub-stream is exchanged every 15s on every established connection,
    // so NAT / host firewalls don't time out a dormant yamux session. Without
    // it, an otherwise-idle P2P session gets reaped by the peer's NAT after a
    // few seconds — the exact "CONNECTED then DISCONNECTED" symptom we fixed.
    pub ping: libp2p::ping::Behaviour,
    /// Binary (CBOR) post-sync channel — carries raw image bytes and small
    /// request/response frames (see `moment.rs`). Uses libp2p's built-in CBOR
    /// codec so `Vec<u8>` serializes as a compact byte-string (no base64).
    pub blob: Blob<PostRequest, PostResponse>,
    /// Binary (CBOR) voice-message channel — carries the raw s16le PCM clip as
    /// a compact byte-string. Kept separate from `blob` (moment sync) so the
    /// codec type parameters and per-protocol size limits stay distinct.
    pub voice: Blob<VoiceRequest, VoiceAck>,
    /// Binary (CBOR) photo-message channel — carries raw image bytes.
    pub image: Blob<ImageRequest, ImageAck>,
    /// Binary (CBOR) document-file channel — carries raw file bytes + name.
    pub file: Blob<FileRequest, FileAck>,
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

    /// Persistent voice message received from a peer. The application layer is
    /// expected to write `data` (s16le PCM) to disk and then call
    /// [`crate::Client::persist_voice`] to record it into the chat store.
    Voice {
        peer: PeerId,
        rate: u32,
        ch: u32,
        data: Vec<u8>,
        duration_ms: u64,
    },

    /// Persistent photo message received from a peer. The application layer is
    /// expected to write `data` (image bytes) to disk and call
    /// [`crate::Client::persist_image`] to record it into the chat store, and
    /// only **then** call [`crate::Client::ack_image`] so the sender's "sent"
    /// badge is confirmed after the photo is durably persisted. `id` tags this
    /// inbound message — pair it with the sender to send the acknowledgement.
    Image {
        peer: PeerId,
        data: Vec<u8>,
        id: u64,
    },

    /// Persistent document file (pdf/docx/xlsx/pptx) received from a peer; `name` = sender's basename — the app writes `data` to disk and records the row via [`crate::Client::persist_file`].
    File {
        peer: PeerId,
        name: String,
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
    /// A dial for `peer` was just issued. The app can flip the chat status
    /// dot to "dialing" (grey + pulsing) immediately, instead of waiting for
    /// the eventual `Connected` / `DialFailed` (which may take seconds).
    Dialing(PeerId),
    /// A previously-issued dial for `peer` definitively failed (all
    /// candidate addrs exhausted, transport error, handshake timeout…). The
    /// dot should drop out of "dialing" back to `""` (red) / `relay` /
    /// `direct` as appropriate.
    DialFailed(PeerId),
    Listening(Multiaddr),
    /// An external (NAT-traversable) address has been confirmed for this
    /// device — e.g. a UPnP-mapped public IP, or a reserved relay-circuit
    /// address. The application should re-announce endpoints to the directory
    /// (heartbeat) now so remote peers can dial it.
    ExternalAddrConfirmed {
        address: Multiaddr,
    },
    /// AutoNAT flipped its reachability verdict. `Public(addr)` means an
    /// outside node dialed us back successfully; `Private` means every probe
    /// was refused. The app can flip the NAT badge on settings / log an
    /// informational line. `addr` is `None` for Private.
    NatStatus {
        public: Option<Multiaddr>,
    },
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
    /// Send a moment/post frame over the binary CBOR channel and wait for
    /// the matching `PostResponse` (resolved by `id`). The serving side is
    /// handled directly by this crate's `swarm_loop` (serving its own
    /// `social_posts` + local media files), so the app layer never touches
    /// the wire.
    SendBlob {
        peer: PeerId,
        req: crate::moment::PostRequest,
        ack_tx: oneshot::Sender<anyhow::Result<crate::moment::PostResponse>>,
    },
    Connect {
        peer: PeerId,
        addr: Multiaddr,
    },
    /// Register every candidate address for a peer and issue a single
    /// `swarm.dial(peer)`; libp2p then dials the candidates **in parallel**
    /// (multi-addr failover) and reports `ConnectionEstablished` for whichever
    /// one wins. Used by the DM send path so the worst case is one dial+send
    /// window (~6 + 8 s) instead of N×(dial+send) windows as when callers
    /// used to issue `Connect` + `SendText` per candidate address.
    Dial {
        peer: PeerId,
        addrs: Vec<Multiaddr>,
    },
    SendText {
        peer: PeerId,
        from: String,
        e2e: String,
        text: String,
        ack_tx: Option<oneshot::Sender<anyhow::Result<()>>>,
        /// Stable sender-local message id, carried on the wire for receiver-side
        /// dedup of redeliveries. `None` = not dedup-eligible (legacy / fire-and-forget).
        client_id: Option<u64>,
    },
    SendAudio {
        peer: PeerId,
        from: String,
        e2e: String,
        rate: u32,
        ch: u32,
        data: Vec<u8>,
    },
    /// Send a persistent voice *message*. The peer responds with the standard
    /// chat ack (resolved via `ack_tx`). The sender is expected to have
    /// already written the clip to local disk before calling this — the
    /// network only carries the PCM bytes.
    SendVoice {
        peer: PeerId,
        from: String,
        e2e: String,
        rate: u32,
        ch: u32,
        duration_ms: u64,
        data: Vec<u8>,
        ack_tx: Option<oneshot::Sender<anyhow::Result<()>>>,
    },
    /// Send a persistent photo *message*. The peer responds with the standard
    /// ack (resolved via `ack_tx`). The sender writes the image to local disk
    /// before calling this — the network carries the raw image bytes.
    SendImage {
        peer: PeerId,
        from: String,
        data: Vec<u8>,
        ack_tx: Option<oneshot::Sender<anyhow::Result<()>>>,
    },
    /// Acknowledge an inbound photo: the app has durably written the image and
    /// persisted its row, so we may now tell the sender it is "sent". Replies
    /// the `ImageAck` on the retained inbound channel (see
    /// [`Running::pending_inbound_image`]).
    AckImage {
        peer: PeerId,
        id: u64,
    },
    /// Send a document file (pdf/docx/xlsx/pptx); `name` = sender's basename (persisted/shown at the receiver); same peer-ack lifecycle as [`Cmd::SendImage`].
    SendFile {
        peer: PeerId,
        from: String,
        name: String,
        data: Vec<u8>,
        ack_tx: Option<oneshot::Sender<anyhow::Result<()>>>,
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
    /// Inbound photo channels awaiting the app's "ack after persist" reply.
    /// Keyed by inbound `ImageRequest::id` -> `(sender, channel, received_at)`.
    /// The channel is kept (not acked on receipt) so the app can send the
    /// `ImageAck` only once the image + its row are durably persisted. Entries
    /// expire so a crashed app can't leak channels — the sender then times out.
    pub pending_inbound_image:
        Arc<Mutex<HashMap<u64, (PeerId, ResponseChannel<ImageAck>, std::time::Instant)>>>,
    /// In-flight binary (moment) request waiters, keyed by `PostRequest::id`.
    pub pending_blobs: Arc<Mutex<HashMap<u64, (PeerId, oneshot::Sender<anyhow::Result<crate::moment::PostResponse>>)>>>,
    /// Shared pool over the local SQLite db, used by the serving side of the
    /// moment channel to read our own `social_posts` on inbound requests.
    pub pool: Option<sqlx::SqlitePool>,
    pub connected: Arc<Mutex<HashMap<PeerId, u32>>>,
    pub dial_backoff: Arc<Mutex<HashMap<String, (u64, u32)>>>,
    pub dial_fail_log: Arc<Mutex<HashMap<String, u64>>>,
    /// Last relay->direct escalation attempt per peer (ms since epoch). Cooldown
    /// guard so a flapping peer isn't re-dialed on every connection event.
    pub escalation_last: Arc<Mutex<HashMap<PeerId, u64>>>,
    pub relays: Arc<Mutex<Vec<(PeerId, Multiaddr)>>>,
    pub injected: mpsc::UnboundedSender<ChatEvent>,
    /// Live direct-transport connections (QUIC/TCP LAN/internet) per peer,
    /// counted from `ConnectionEstablished`/`ConnectionClosed` events — the
    /// `endpoint` field on each event is the actual path that connection runs
    /// over, which is the only unambiguous classifier (libp2p dials many
    /// addresses in parallel for one peer, so "the address we tried to dial"
    /// does not say which one carried the traffic). A non-zero count means at
    /// least one direct link is up → the chat dot is green.
    pub link_direct: Arc<Mutex<HashMap<PeerId, u32>>>,
    /// Live relay-circuit connections per peer — same classification source as
    /// `link_direct`. A non-zero count (with zero direct) means the peer is
    /// reachable but only via the relay → the chat dot is yellow.
    pub link_relay: Arc<Mutex<HashMap<PeerId, u32>>>,
    /// Live AutoNAT reachability verdict (`true` = outside node dialed us,
    /// `false` = private/blocked/unknown). Written from `swarm_loop` on
    /// `Autonat(StatusChanged)` events, read synchronously by the directory
    /// announce path (`endpoints_with_egress`) to decide whether our STUN
    /// public-IP candidate is *trustworthy* — a symmetric NAT also produces a
    /// public IP via STUN, but peers can never dial it.
    pub nat_public: Arc<std::sync::atomic::AtomicBool>,
    /// Peers with a dial in flight (a `swarm.dial` issued but not yet
    /// established or failed). Presence == "connecting / hole-punching in
    /// progress" (grey + pulsing dot). Added in `Cmd::Connect`, cleared once the
    /// connection is established or the dial errors out.
    pub dialing: Arc<Mutex<std::collections::HashSet<PeerId>>>,
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

/// True when `addr` carries a `/p2p-circuit` segment — i.e. it is a
/// relay-tunneled route rather than a direct transport path.
fn is_relayed(addr: &Multiaddr) -> bool {
    addr.iter()
        .any(|c| matches!(c, libp2p::multiaddr::Protocol::P2pCircuit))
}

/// Classify a [`libp2p::core::connection::ConnectedPoint`] from a swarm event:
/// `true` means the live transport is a relay circuit, `false` means a direct
/// transport.
fn is_endpoint_relayed(endpoint: &libp2p::core::connection::ConnectedPoint) -> bool {
    use libp2p::core::connection::ConnectedPoint;
    match endpoint {
        ConnectedPoint::Dialer { address, .. } => is_relayed(address),
        ConnectedPoint::Listener {
            local_addr,
            send_back_addr,
        } => is_relayed(local_addr) || is_relayed(send_back_addr),
    }
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

/// Parse a list of full relay multiaddr strings (as returned by the
/// directory's `GET /v1/relays`, or the `CHATX_RELAYS` env var) into
/// `(relay_peer_id, dialable_base)` pairs. Malformed or non-relay entries
/// are skipped with a debug log.
pub fn parse_relay_strings(specs: &[String]) -> Vec<(PeerId, Multiaddr)> {
    let mut out = Vec::new();
    for s in specs {
        match s.parse::<Multiaddr>() {
            Ok(addr) => match split_relay_addr(&addr) {
                Some(pair) => out.push(pair),
                None => tracing::debug!("[relay] {:?} has no /p2p/... component; ignored", s),
            },
            Err(e) => tracing::debug!("[relay] {s:?}: {e}"),
        }
    }
    out
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
                None => tracing::debug!("[relay] {s} has no /p2p/.../ component; ignored"),
            },
            Err(e) => tracing::debug!("[relay] {s}: {e}"),
        }
    }
    out
}

pub const MAX_DIALS_PER_TICK: usize = 64;
pub const DIAL_BACKOFF_BASE_MS: u64 = 10_000;
pub const DIAL_BACKOFF_MAX_MS: u64 = 300_000;
pub const FAIL_LOG_REPEAT_MS: u64 = 60_000;
// DM-specific reconnection backoff. The group-dial backoff above (base 10s) is
// deliberate for chatty fan-out, but for a pair of actively-interacting users a
// 10s no-retry window is user-visible, so reconnection of a dropped DM peer
// uses a smaller base / cap (exponential: 2s, 4s, 8s, ... capped at 60s).
// `DIAL_BACKOFF_DUR` is the per-attempt window a single dial must outlive: it
// covers libp2p's internal multi-addr failover (~5s) so an in-flight dial isn't
// abandoned before its handshake lands (was 6000ms inline, now a shared const).
pub const DM_RECONNECT_BASE_MS: u64 = 2_000;
pub const DM_RECONNECT_MAX_MS: u64 = 60_000;
pub const DIAL_BACKOFF_DUR: std::time::Duration = std::time::Duration::from_secs(6);

pub type EventRx = mpsc::UnboundedReceiver<ChatEvent>;

pub async fn boot(
    device: &DeviceIdentity,
    pool: Option<sqlx::SqlitePool>,
) -> Result<(Arc<Running>, mpsc::UnboundedReceiver<ChatEvent>)> {
    let my_peer = device.peer_id();
    let gossip_cfg = libp2p::gossipsub::ConfigBuilder::default()
        .allow_self_origin(true)
        // Group voice clips / photos travel on the gossipsub group topic; the
        // stock 1 MiB cap would reject them. Match the DM voice/photo channel
        // budget so a clip/image that fits a DM also fits a group broadcast.
        .max_transmit_size(IMAGE_REQ_MAX as usize)
        .build()
        .map_err(|e| anyhow::anyhow!("gossipsub config: {e}"))?;
    let mdns = Mdns::new(MdnsConfig::default(), my_peer)?;
    // JSON channel is for small control frames (text / webrtc / realtime audio)
    // — its stock 1 MiB request cap is plenty, so we keep the default codec.
    // Large voice clips travel on the dedicated binary (CBOR) `voice` channel
    // below, whose request cap is raised to fit a full clip.
    let chat: Chat<ChatRequest, ChatResponse> = Chat::new(
        [(TEXT_PROTOCOL, ProtocolSupport::Full)],
        RrConfig::default(),
    );
    let blob: Blob<PostRequest, PostResponse> = Blob::new(
        [(MOMENT_PROTOCOL, ProtocolSupport::Full)],
        RrConfig::default(),
    );
    // Binary (CBOR) voice channel. `Vec<u8>` serializes as a compact byte-string
    // (no base64 / JSON), and we raise the request cap to fit a full clip — the
    // stock 1 MiB default would truncate longer recordings and drop the stream.
    let voice: Blob<VoiceRequest, VoiceAck> = {
        let codec = cbor::codec::Codec::<VoiceRequest, VoiceAck>::default()
            .set_request_size_maximum(VOICE_REQ_MAX)
            .set_response_size_maximum(10 * 1024 * 1024);
        Blob::with_codec(codec, [(VOICE_PROTOCOL, ProtocolSupport::Full)], RrConfig::default())
    };
    // Binary (CBOR) photo channel — raw image bytes, capped like voice.
    let image: Blob<ImageRequest, ImageAck> = {
        let codec = cbor::codec::Codec::<ImageRequest, ImageAck>::default()
            .set_request_size_maximum(IMAGE_REQ_MAX)
            .set_response_size_maximum(10 * 1024 * 1024);
        Blob::with_codec(codec, [(IMAGE_PROTOCOL, ProtocolSupport::Full)], RrConfig::default())
    };
    // Binary (CBOR) document-file channel — raw file bytes + name.
    let file: Blob<FileRequest, FileAck> = {
        let codec = cbor::codec::Codec::<FileRequest, FileAck>::default()
            .set_request_size_maximum(FILE_REQ_MAX)
            .set_response_size_maximum(10 * 1024 * 1024);
        Blob::with_codec(codec, [(FILE_PROTOCOL, ProtocolSupport::Full)], RrConfig::default())
    };
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

    // AutoNAT probe config — defaults are tuned for "eventually figure out
    // whether I'm reachable"; we want a verdict *fast* enough (within one or
    // two boot delay windows) that the first directory heartbeat already
    // reflects it. `use_connected: false` keeps the probe scope tight: we
    // explicitly add the relay as a fixed server (it's the one peer we're
    // guaranteed to reach through every network), and don't want any
    // connected peer silently becoming a probe server (that would turn every
    // DM into an autonat dial-back request).
    let autonat_cfg = libp2p::autonat::Config {
        boot_delay: std::time::Duration::from_secs(6),
        retry_interval: std::time::Duration::from_secs(30),
        refresh_interval: std::time::Duration::from_secs(15 * 60),
        // We explicitly add the relay as a fixed server (`add_server` bypasses
        // this check) — disable the connected-peer path so we don't turn
        // every DM into an auto-nat dial-back request.
        use_connected: false,
        ..libp2p::autonat::Config::default()
    };
    let mut swarm = SwarmBuilder::with_existing_identity(device.keypair())
        .with_tokio()
        .with_tcp(
            Default::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )
        .expect("tcp+noise+yamux")
        .with_quic()
        .with_relay_client(libp2p::noise::Config::new, libp2p::yamux::Config::default)?
        .with_behaviour(move |k, relay| {
            let my_peer = libp2p::PeerId::from_public_key(&k.public());
            let autonat = libp2p::autonat::Behaviour::new(my_peer, autonat_cfg);
            Net {
            mdns,
            upnp: Upnp::default(),
            relay,
            dcutr: libp2p::dcutr::Behaviour::new(my_peer),
            autonat,
            chat,
            blob,
            voice,
            image,
            file,
            group,
            ping,
            }
        })?
        .build();

    let tcp0: Multiaddr = "/ip4/0.0.0.0/tcp/0".parse().unwrap();
    swarm
        .listen_on(tcp0)
        .map_err(|e| anyhow::anyhow!("listen tcp: {e}"))?;
    let udp0: Multiaddr = "/ip4/0.0.0.0/udp/0/quic-v1".parse().unwrap();
    swarm
        .listen_on(udp0)
        .map_err(|e| anyhow::anyhow!("listen quic: {e}"))?;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    let (injected_tx, injected_rx) = mpsc::unbounded_channel();
    let listen_addrs: Arc<Mutex<Vec<Multiaddr>>> = Arc::new(Mutex::new(Vec::new()));
    let pending_acks = Arc::new(Mutex::new(HashMap::new()));
    let pending_inbound_image:
        Arc<Mutex<HashMap<u64, (PeerId, ResponseChannel<ImageAck>, std::time::Instant)>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pending_blobs = Arc::new(Mutex::new(HashMap::new()));
    let connected = Arc::new(Mutex::new(HashMap::new()));
    let dial_backoff = Arc::new(Mutex::new(HashMap::new()));
    let dial_fail_log = Arc::new(Mutex::new(HashMap::new()));
    let escalation_last = Arc::new(Mutex::new(HashMap::new()));
    let relays = Arc::new(Mutex::new(parse_relay_env()));
    let link_direct: Arc<Mutex<HashMap<PeerId, u32>>> = Arc::new(Mutex::new(HashMap::new()));
    let link_relay: Arc<Mutex<HashMap<PeerId, u32>>> = Arc::new(Mutex::new(HashMap::new()));
    let dialing: Arc<Mutex<std::collections::HashSet<PeerId>>> = Arc::new(Mutex::new(std::collections::HashSet::new()));
    // Start UNKNOWN (false) — the heartbeat NAT diagnostic treats `false` as
    // the conservative "can't prove reachability yet" state. Flipped by the
    // first `StatusChanged` event once the relay probe has an answer.
    let nat_public: Arc<std::sync::atomic::AtomicBool> = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let me = *swarm.local_peer_id();
    for (peer, base) in relays.lock().unwrap().iter().cloned() {
        let circuit = base
            .clone()
            .with(libp2p::multiaddr::Protocol::P2p(peer))
            .with(libp2p::multiaddr::Protocol::P2pCircuit)
            .with(libp2p::multiaddr::Protocol::P2p(me));
        match swarm.listen_on(circuit.clone()) {
            Ok(_) => tracing::debug!("[swarm] reserving relay slot on {circuit}"),
            Err(e) => tracing::warn!("reserve relay {peer}: {e}"),
        }
        // Register the relay as a fixed AutoNAT probe server. The relay is
        // the one peer every client is *guaranteed* to be able to reach
        // (outbound TCP/QUIC, or the very circuit we just reserved over), so
        // its dial-back answer is the most trustworthy reachability verdict we
        // can get behind any NAT — no third-party node needed. Passing the
        // base address lets autonat dial the relay directly on the first probe
        // without waiting on the (separate) relay-client address book; the
        // relay must *answer* autonat dial-back requests (see
        // `signaling/src/relay.rs` — `with_autonat_server`).
        let has_ip = base.iter().any(|c| matches!(
            c,
            libp2p::multiaddr::Protocol::Ip4(_) | libp2p::multiaddr::Protocol::Ip6(_)
        ));
        if has_ip {
            swarm.behaviour_mut().autonat.add_server(peer, Some(base.clone()));
        } else {
            tracing::debug!(%base, "autonat server {peer}: no IP in base; relying on address book only");
            swarm.behaviour_mut().autonat.add_server(peer, None);
        }
    }

    let running = Arc::new(Running {
        cmd_tx: cmd_tx.clone(),
        listen_addrs: Arc::clone(&listen_addrs),
        my_peer_id: my_peer,
        pending_acks: Arc::clone(&pending_acks),
        pending_inbound_image: Arc::clone(&pending_inbound_image),
        pending_blobs: Arc::clone(&pending_blobs),
        pool: pool.clone(),
        connected: Arc::clone(&connected),
        dial_backoff: Arc::clone(&dial_backoff),
        dial_fail_log: Arc::clone(&dial_fail_log),
        escalation_last: Arc::clone(&escalation_last),
        relays: Arc::clone(&relays),
        injected: injected_tx.clone(),
        link_direct: Arc::clone(&link_direct),
        link_relay: Arc::clone(&link_relay),
        dialing: Arc::clone(&dialing),
        nat_public: Arc::clone(&nat_public),
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
    let pending_blobs = running.pending_blobs.clone();
    loop {
        tokio::select! {
            maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                Some(Cmd::SendBlob { peer, req, ack_tx }) => {
                    let id = req.id;
                    running
                        .pending_blobs
                        .lock()
                        .unwrap()
                        .insert(id, (peer.clone(), ack_tx));
                    let _rid = swarm.behaviour_mut().blob.send_request(&peer, req);
                    tracing::debug!("[swarm] SEND BLOB id={id} peer={peer}");
                }
                Some(Cmd::Connect { peer, addr }) => {
                    let own = is_own_address(&addr, &running);
                    if peer == *swarm.local_peer_id() {
                        tracing::debug!("[swarm] DIAL SKIPPED self peer={peer} addr={addr}");
                    } else if own {
                        // Dialing one of our own listen / confirmed-external
                        // addresses (e.g. a sibling device on this host, or a
                        // stale self endpoint in the directory) — skip.
                        tracing::debug!("[swarm] DIAL SKIPPED own addr={addr} peer={peer}");
                    } else {
                        tracing::debug!("[swarm] DIAL peer={peer} addr={addr}");
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
                        running.dialing.lock().unwrap().insert(peer);
                        let _ = swarm.dial(peer);
                        let _ = ev.send(ChatEvent::Dialing(peer));
                    }
                }
                Some(Cmd::Dial { peer, addrs }) => {
                        if peer == *swarm.local_peer_id() {
                            tracing::debug!("[swarm] DIAL SKIPPED self peer={peer} ({} addrs)", addrs.len());
                    } else {
                        // Filter out our own listen / sibling addresses so a
                        // stale directory entry never produces a self-dial.
                        let foreign: Vec<Multiaddr> = addrs
                            .into_iter()
                            .filter(|a| !is_own_address(a, &running))
                            .collect();
                        if foreign.is_empty() {
                            tracing::warn!(
                                %peer,
                                "[swarm] DIAL all candidates self/loopback; skipping"
                            );
                            continue;
                        }
                        tracing::debug!(
                            %peer,
                            n = foreign.len(),
                            "[swarm] DIAL fan-out (parallel)"
                        );
                        running
                            .dial_backoff
                            .lock()
                            .unwrap()
                            .remove(&peer.to_base58());
                        for a in &foreign {
                            swarm.add_peer_address(peer, a.clone());
                        }
                        running.dialing.lock().unwrap().insert(peer);
                        let _ = swarm.dial(peer);
                        let _ = ev.send(ChatEvent::Dialing(peer));
                    }
                }
                Some(Cmd::SendText {
                    peer,
                    from,
                    e2e,
                    text,
                    ack_tx,
                    client_id,
                }) => {
                    // `id` is minted fresh PER SEND so concurrent redeliveries of
                    // the same logical message each carry a distinct request/id,
                    // and their ACKs don't cross-match. `client_id` is the
                    // stable sender-local id used for receiver-side dedup.
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
                        client_id,
                        kind: crate::message::MsgKind::Dm,
                        group_id: None,
                        audio: None,
                        signal: None,
                        voice: None,
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
                        client_id: None,
                        kind: crate::message::MsgKind::Audio,
                        group_id: None,
                        audio: Some(AudioPayload {
                            rate,
                            ch,
                            data: crate::account::b64(&data),
                        }),
                        signal: None,
                        voice: None,
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
                        client_id: None,
                        kind: crate::message::MsgKind::Webrtc,
                        group_id: None,
                        audio: None,
                        signal: Some(payload),
                        voice: None,
                    };
                    let _rid = swarm.behaviour_mut().chat.send_request(&peer, req);
                }
                Some(Cmd::SendVoice {
                    peer,
                    from,
                    e2e: _,
                    rate,
                    ch,
                    duration_ms,
                    data,
                    ack_tx,
                }) => {
                    let id: u64 = rand::random();
                    if let Some(tx) = ack_tx {
                        pending_acks
                            .lock()
                            .unwrap()
                            .insert(id, (peer, tx));
                    }
                    // Binary (CBOR) channel: `data` is the raw s16le PCM and
                    // `cbor4ii` serializes it as a compact byte-string — no
                    // base64, no JSON. Wire size ≈ clip size.
                    let req = VoiceRequest {
                        id,
                        from,
                        rate,
                        ch,
                        duration_ms,
                        data,
                    };
                    let _rid = swarm.behaviour_mut().voice.send_request(&peer, req);
                }
                Some(Cmd::SendImage {
                    peer,
                    from,
                    data,
                    ack_tx,
                }) => {
                    let id: u64 = rand::random();
                    if let Some(tx) = ack_tx {
                        pending_acks
                            .lock()
                            .unwrap()
                            .insert(id, (peer, tx));
                    }
                    tracing::debug!(
                        "[swarm] IMAGE OUTBOUND peer={peer} id={} {} bytes",
                        id,
                        data.len()
                    );
                    let req = ImageRequest { id, from, data };
                    let _rid = swarm.behaviour_mut().image.send_request(&peer, req);
                }
                Some(Cmd::AckImage { peer, id }) => {
                    match running.pending_inbound_image.lock().unwrap().remove(&id) {
                        Some((_sender, channel, _at)) => {
                            let _ = swarm
                                .behaviour_mut()
                                .image
                                .send_response(channel, ImageAck { id });
                            tracing::debug!(
                                "[swarm] IMAGE ACK SENT to={peer} id={} (after app persist)",
                                id
                            );
                        }
                        // Not in the table (already acked, expired, or never held) —
                        // nothing to send; the sender will resolve on timeout.
                        None => tracing::debug!(
                            "[swarm] IMAGE ACK DROPPED to={peer} id={} (no channel)",
                            id
                        ),
                    }
                }
                Some(Cmd::SendFile {
                    peer,
                    from,
                    name,
                    data,
                    ack_tx,
                }) => {
                    let id: u64 = rand::random();
                    if let Some(tx) = ack_tx {
                        pending_acks
                            .lock()
                            .unwrap()
                            .insert(id, (peer, tx));
                    }
                    tracing::debug!(
                        "[swarm] FILE OUTBOUND peer={peer} id={} {} bytes name={name}",
                        id,
                        data.len()
                    );
                    let req = FileRequest {
                        id,
                        from,
                        name,
                        data,
                    };
                    let _rid = swarm.behaviour_mut().file.send_request(&peer, req);
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
                        client_id: None,
                        kind: crate::message::MsgKind::GroupKey,
                        group_id: Some(group_id),
                        audio: None,
                        signal: None,
                        voice: None,
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
                    // Give the relay client its dial target. Don't `dial` here —
                    // the client reserves over the connection it dials itself in
                    // `listen_on`; a pre-dial breaks that handshake.
                    swarm.add_peer_address(relay_peer, relay_addr.clone());
                    if let Err(e) = swarm.listen_on(circuit.clone()) {
                        tracing::warn!(%relay_peer, %circuit, "reserve relay: {e}");
                    }
                }
                Some(Cmd::Disconnect { peer }) => {
                    // `disconnect_peer_id` returns `Ok(())` if a live conn was
                    // closed, `Err(())` if the peer was not connected (no-op).
                    // Both outcomes are fine — we intentionally do not remove
                    // the peer from the address book, so mDNS / identify can
                    // rediscover them if the user re-enters the chat.
                    match swarm.disconnect_peer_id(peer) {
                        Ok(()) => tracing::debug!("[swarm] DISCONNECTED {peer} (via cmd)"),
                        Err(()) => tracing::debug!("[swarm] DISCONNECT {peer}: was not connected"),
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
                    tracing::debug!("[swarm] INCOMING conn on {local_addr:?}");
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
                    let new = !addrs.iter().any(|a| a == &address);
                    if new {
                        addrs.push(address.clone());
                    }
                    tracing::info!("[swarm] EXTERNAL ADDR CONFIRMED {address}");
                    if new {
                        let _ = ev.send(ChatEvent::ExternalAddrConfirmed { address });
                    }
                }
                SwarmEvent::ExternalAddrExpired { address } => {
                    running
                        .listen_addrs
                        .lock()
                        .unwrap()
                        .retain(|a| a != &address);
                    tracing::debug!("[swarm] EXTERNAL ADDR EXPIRED {address}");
                }
                SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                    tracing::debug!(
                        "[swarm] CONNECTED to {peer_id} endpoint={endpoint:?}"
                    );
                    // Decrement counters on a best-effort basis; classify via
                    // the live `endpoint` (the only unambiguous source — the
                    // address we *tried* to dial is not the address we
                    // *used*).
                    {
                        let is_relay = is_endpoint_relayed(&endpoint);
                        let bucket = if is_relay {
                            &running.link_relay
                        } else {
                            &running.link_direct
                        };
                        *running.connected.lock().unwrap().entry(peer_id).or_insert(0) += 1;
                        *bucket.lock().unwrap().entry(peer_id).or_insert(0) += 1;
                    tracing::debug!(
                        "[swarm] {peer_id} link_{kind}={count}",
                        kind = if is_relay { "relay" } else { "direct" },
                        count = bucket.lock().unwrap().get(&peer_id).copied().unwrap_or(0)
                    );
                    }
                    running.dial_backoff.lock().unwrap().remove(&peer_id.to_base58());
                    running.dialing.lock().unwrap().remove(&peer_id);
                    let _ = ev.send(ChatEvent::Connected(peer_id));
                }
                SwarmEvent::NewExternalAddrOfPeer { peer_id, address } => {
                    // §A escalation: the peer just advertised a *new* address.
                    // If that's a direct (non-relay) address and we're currently
                    // connected to this peer, re-dial so libp2p re-contends and
                    // prefers direct (bandwidth/latency saving). Bounded by a
                    // per-peer cooldown.
                    if peer_id != running.my_peer_id
                        && !is_relayed(&address)
                        && running.connected.lock().unwrap().contains_key(&peer_id)
                    {
                        const COOLDOWN_MS: u64 = 30_000;
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let allow = {
                            let m = running.escalation_last.lock().unwrap();
                            m.get(&peer_id)
                                .map(|last| now.saturating_sub(*last) >= COOLDOWN_MS)
                                .unwrap_or(true)
                        };
                        if allow {
                            {
                                let mut m = running.escalation_last.lock().unwrap();
                                m.insert(peer_id, now);
                            }
                            running
                                .dial_backoff
                                .lock()
                                .unwrap()
                                .remove(&peer_id.to_base58());
                            tracing::info!(%peer_id, %address, "new direct addr from peer — re-dial to upgrade from relay");
                            if let Err(e) = swarm.dial(peer_id) {
                                tracing::debug!(%peer_id, "escalate re-dial: {e}");
                            }
                        }
                    }
                }
                SwarmEvent::ConnectionClosed { peer_id, endpoint, .. } => {
                    tracing::debug!(
                        "[swarm] DISCONNECTED from {peer_id} endpoint={endpoint:?}"
                    );
                    {
                        let mut m = running.connected.lock().unwrap();
                        let dropped_all = match m.get_mut(&peer_id) {
                            Some(n) => {
                                *n = n.saturating_sub(1);
                                if *n == 0 {
                                    m.remove(&peer_id);
                                    true
                                } else {
                                    false
                                }
                            }
                            None => false,
                        };
                        drop(m);
                        if dropped_all {
                            running.link_direct.lock().unwrap().remove(&peer_id);
                            running.link_relay.lock().unwrap().remove(&peer_id);
                            running.dialing.lock().unwrap().remove(&peer_id);
                        } else {
                            let is_relay = is_endpoint_relayed(&endpoint);
                            let bucket = if is_relay {
                                &running.link_relay
                            } else {
                                &running.link_direct
                            };
                            let mut b = bucket.lock().unwrap();
                            if let Some(n) = b.get_mut(&peer_id) {
                                *n = n.saturating_sub(1);
                                if *n == 0 {
                                    b.remove(&peer_id);
                                }
                            }
                        }
                        // NOTE: `b` guard is released before `ev.send` to avoid
                        // holding the lock across a potential blocking send.
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
                            tracing::warn!("[swarm] DIAL FAILED peer={key} err={error:?}");
                        }
                        // A dial attempt failed. Clear the "connecting" marker
                        // ONLY if no established connection to this peer exists
                        // (otherwise the dot still shows green/yellow from the
                        // successful path, and a concurrent in-flight dial may
                        // still be resolving for the same peer).
                        let cleared = !running
                            .connected
                            .lock()
                            .unwrap()
                            .contains_key(&peer_id);
                        if cleared {
                            running.dialing.lock().unwrap().remove(&peer_id);
                        }
                        // Tell the app the dial for this peer is over; the dot
                        // should drop back out of "dialing" (grey + pulsing).
                        if cleared {
                            let _ = ev.send(ChatEvent::DialFailed(peer_id));
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
                            tracing::debug!("[swarm] RELAY reservation accepted from {relay_peer_id}");
                        }
                    }
                     // DCUtR: relay→direct hole-punch completed (Ok) or failed
                     // (Err — connection stays on the relay; the §A escalation
                     // path remains a backstop once we learn the peer's real
                     // address).
                      NetEvent::Dcutr(libp2p::dcutr::Event { remote_peer_id, result }) => {
                          match result {
                              Ok(_) => tracing::info!(
                                  %remote_peer_id,
                                  "[dcutr] hole-punch SUCCEEDED — upgraded relay→direct"
                              ),
                              Err(e) => {
                                  // The connection stays on the relay; the §A
                                  // escalation path is the backstop.
                                  // Capture the full failure cause for the
                                  // log — most useful when a peer on a
                                  // symmetric NAT pair repeatedly fails and
                                  // the operator wants to correlate with
                                  // `scripts/nat-probe` output.
                                  tracing::warn!(
                                      %remote_peer_id,
                                      err = %e,
                                      reason = ?std::error::Error::source(&e).map(|s| s.to_string()),
                                      "[dcutr] hole-punch failed — staying on relay"
                                  );
                              }
                          }
                      }
                      NetEvent::Autonat(a) => match a {
                          libp2p::autonat::Event::StatusChanged { old, new } => {
                              tracing::info!(?old, ?new, "[autonat] NAT status changed");
                              let is_public = matches!(new, libp2p::autonat::NatStatus::Public(_));
                              running
                                  .nat_public
                                  .store(is_public, std::sync::atomic::Ordering::Relaxed);
                              let pub_addr = match &new {
                                  libp2p::autonat::NatStatus::Public(a) => Some(a.clone()),
                                  _ => None,
                              };
                              let _ = ev.send(ChatEvent::NatStatus { public: pub_addr });
                              // On a Private verdict, the STUN public-IP
                              // endpoint in the directory is known to be
                              // un-dialable by peers — log so the operator
                              // can see *why* a cross-NAT dial is falling
                              // back to the relay (symmetric NAT / UDP
                              // ingress blocked / CGNAT are the usual
                              // suspects; TURN is the recommended fix).
                              if matches!(new, libp2p::autonat::NatStatus::Private) {
                                  let stun_ip: Option<String> =
                                      crate::stun::discover_cached().map(|p| p.ip.to_string());
                                  tracing::warn!(?stun_ip,
                                      "[autonat] verdict: PRIVATE — STUN public IP, if any, \
                                       is likely an ephemeral mapping a peer cannot dial \
                                       (symmetric NAT / UDP ingress blocked / CGNAT). Relay \
                                       path is the only reliable route to remote peers; TURN \
                                       recommended.");
                              }
                          }
                          libp2p::autonat::Event::OutboundProbe(p) => {
                              tracing::debug!(?p, "[autonat] outbound probe");
                          }
                          libp2p::autonat::Event::InboundProbe(p) => {
                              tracing::debug!(?p, "[autonat] inbound probe (chatx is acting as probe server)");
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
                                    tracing::debug!("[swarm] ACKED id={}", response.id);
                                    let _ = tx.send(Ok(()));
                                }
                                let _ = ev.send(ChatEvent::Acked {
                                    id: response.id,
                                    e2e: response.e2e,
                                });
                            }
                        },
                            RrEvent::OutboundFailure { peer, error, .. } => {
                                tracing::warn!("[swarm] SEND FAILED peer={peer} err={error:?}");
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
                                // Also fail any in-flight blob waiters for this peer.
                                let mut mb = running.pending_blobs.lock().unwrap();
                                let bids: Vec<u64> = mb
                                    .iter()
                                    .filter(|(_, (p, _))| *p == peer)
                                    .map(|(id, _)| *id)
                                    .collect();
                                for id in bids {
                                    if let Some((_p, tx)) = mb.remove(&id) {
                                        let _ = tx.send(Err(anyhow::anyhow!(
                                            "send blob to {peer} failed: {error:?}"
                                        )));
                                    }
                                }
                                drop(mb);
                                let _ = ev.send(ChatEvent::SendFailed {
                                peer,
                                reason: format!("{error:?}"),
                            });
                        }
                        _ => {}
                    },
                    // Binary (CBOR) voice-message channel.
                    //  * Inbound request: we received a voice clip; notify the app.
                    //  * Inbound response: we are the sender; resolve the waiter.
                    NetEvent::Voice(v) => match v {
                        RrEvent::Message { peer, message, .. } => match message {
                            RrMessage::Request { request, channel, .. } => {
                                tracing::debug!(
                                    "[swarm] VOICE INBOUND from peer={peer} id={} {} bytes rate={} ch={} dur={}ms",
                                    request.id, request.data.len(),
                                    request.rate, request.ch, request.duration_ms
                                );
                                // Ack immediately (the app writes to disk + persists
                                // asynchronously); the sender unblocks.
                                let ack = VoiceAck { id: request.id };
                                let _ = swarm.behaviour_mut().voice.send_response(channel, ack);
                                let _ = ev.send(ChatEvent::Voice {
                                    peer,
                                    rate: request.rate,
                                    ch: request.ch,
                                    data: request.data,
                                    duration_ms: request.duration_ms,
                                });
                            }
                            RrMessage::Response { response, .. } => {
                                tracing::debug!("[swarm] VOICE ACKED id={}", response.id);
                                if let Some((_peer, tx)) =
                                    pending_acks.lock().unwrap().remove(&response.id)
                                {
                                    let _ = tx.send(Ok(()));
                                }
                            }
                        },
                        RrEvent::OutboundFailure { peer, error, .. } => {
                            tracing::warn!("[swarm] VOICE OUTBOUND FAILED peer={peer} err={error:?}");
                            let mut m = pending_acks.lock().unwrap();
                            let ids: Vec<u64> = m
                                .iter()
                                .filter(|(_, (p, _))| *p == peer)
                                .map(|(id, _)| *id)
                                .collect();
                            for id in ids {
                                if let Some((_p, tx)) = m.remove(&id) {
                                    let _ = tx.send(Err(anyhow::anyhow!(
                                        "send voice to {peer} failed: {error:?}"
                                    )));
                                }
                            }
                        }
                        _ => {}
                    },
                    // Binary (CBOR) photo-message channel.
                    //  * Inbound request: we received a photo; notify the app.
                    //  * Inbound response: we are the sender; resolve the waiter.
                    NetEvent::Image(img) => match img {
                        RrEvent::Message { peer, message, .. } => match message {
                            RrMessage::Request { request, channel, .. } => {
                                tracing::debug!(
                                    "[swarm] IMAGE INBOUND from peer={peer} id={} {} bytes",
                                    request.id,
                                    request.data.len()
                                );
                                // Hold the inbound channel and do NOT ack here: the
                                // app must acknowledge (via `Cmd::AckImage`) only
                                // after it has durably persisted the image + its
                                // row. Prune any expired unclaimed channels so a
                                // crashed app can't leak them.
                                {
                                    let now = std::time::Instant::now();
                                    let ttl = std::time::Duration::from_secs(60);
                                    let mut m = running.pending_inbound_image.lock().unwrap();
                                    m.retain(|_, (_, _, at)| now.duration_since(*at) < ttl);
                                }
                                running
                                    .pending_inbound_image
                                    .lock()
                                    .unwrap()
                                    .insert(
                                        request.id,
                                        (peer.clone(), channel, std::time::Instant::now()),
                                    );
                                let _ = ev.send(ChatEvent::Image {
                                    peer,
                                    data: request.data,
                                    id: request.id,
                                });
                            }
                            RrMessage::Response { response, .. } => {
                                tracing::debug!("[swarm] IMAGE ACKED id={}", response.id);
                                if let Some((_peer, tx)) =
                                    pending_acks.lock().unwrap().remove(&response.id)
                                {
                                    let _ = tx.send(Ok(()));
                                }
                            }
                        },
                        RrEvent::OutboundFailure { peer, error, .. } => {
                            tracing::warn!("[swarm] IMAGE OUTBOUND FAILED peer={peer} err={error:?}");
                            let mut m = pending_acks.lock().unwrap();
                            let ids: Vec<u64> = m
                                .iter()
                                .filter(|(_, (p, _))| *p == peer)
                                .map(|(id, _)| *id)
                                .collect();
                            for id in ids {
                                if let Some((_p, tx)) = m.remove(&id) {
                                    let _ = tx.send(Err(anyhow::anyhow!(
                                        "send image to {peer} failed: {error:?}"
                                    )));
                                }
                            }
                        }
                        _ => {}
                    },
                    // Binary (CBOR) document-file channel (pdf/docx/xlsx/pptx): inbound request = file received (notify the app); inbound response = we are the sender (resolve the waiter).
                    NetEvent::File(f) => match f {
                        RrEvent::Message { peer, message, .. } => match message {
                            RrMessage::Request { request, channel, .. } => {
                                tracing::debug!(
                                    "[swarm] FILE INBOUND from peer={peer} id={} {} bytes name={}",
                                    request.id,
                                    request.data.len(),
                                    request.name
                                );
                                let ack = FileAck { id: request.id };
                                let _ = swarm.behaviour_mut().file.send_response(channel, ack);
                                let _ = ev.send(ChatEvent::File {
                                    peer,
                                    name: request.name,
                                    data: request.data,
                                });
                            }
                            RrMessage::Response { response, .. } => {
                                tracing::debug!("[swarm] FILE ACKED id={}", response.id);
                                if let Some((_peer, tx)) =
                                    pending_acks.lock().unwrap().remove(&response.id)
                                {
                                    let _ = tx.send(Ok(()));
                                }
                            }
                        },
                        RrEvent::OutboundFailure { peer, error, .. } => {
                            tracing::warn!("[swarm] FILE OUTBOUND FAILED peer={peer} err={error:?}");
                            let mut m = pending_acks.lock().unwrap();
                            let ids: Vec<u64> = m
                                .iter()
                                .filter(|(_, (p, _))| *p == peer)
                                .map(|(id, _)| *id)
                                .collect();
                            for id in ids {
                                if let Some((_p, tx)) = m.remove(&id) {
                                    let _ = tx.send(Err(anyhow::anyhow!(
                                        "send file to {peer} failed: {error:?}"
                                    )));
                                }
                            }
                        }
                        _ => {}
                    },
                    // Moment/post sync — the binary CBOR channel.
                    //  * Inbound request: we are the friend's device; answer
                    //    from our own `social_posts` + local media files.
                    //  * Inbound response: we are the syncing client; resolve
                    //    the matching waiter in `pending_blobs`.
                    NetEvent::Blob(b) => match b {
                        RrEvent::Message { message, .. } => match message {
                            RrMessage::Request { request, channel, .. } => {
                                let req_id = request.id;
                                let pool = running.pool.clone();
                                let payload =
                                    match crate::moment::serve(request, pool).await {
                                        Ok(p) => p,
                                        Err(e) => {
                                            tracing::warn!("[swarm] SERVE FAIL id={req_id}: {e}");
                                            crate::moment::PostPayload::Err {
                                                msg: e.to_string(),
                                            }
                                        }
                                    };
                                let resp = crate::moment::PostResponse {
                                    id: req_id,
                                    payload,
                                };
                                let _ =
                                    swarm.behaviour_mut().blob.send_response(channel, resp);
                                tracing::debug!("[swarm] SERVED blob id={req_id}");
                            }
                            RrMessage::Response { response, .. } => {
                                let mut m = pending_blobs.lock().unwrap();
                                if let Some((_peer, tx)) = m.remove(&response.id) {
                                    tracing::debug!("[swarm] BLOB ACKED id={}", response.id);
                                    let _ = tx.send(Ok(response));
                                }
                            }
                        },
                        RrEvent::OutboundFailure { peer, error, .. } => {
                            tracing::warn!("[swarm] BLOB OUTBOUND FAILED peer={peer} err={error:?}");
                            let mut m = pending_blobs.lock().unwrap();
                            let ids: Vec<u64> = m
                                .iter()
                                .filter(|(_, (p, _))| *p == peer)
                                .map(|(id, _)| *id)
                                .collect();
                            for id in ids {
                                if let Some((_p, tx)) = m.remove(&id) {
                                    let _ = tx.send(Err(anyhow::anyhow!(
                                        "send blob to {peer} failed: {error:?}"
                                    )));
                                }
                            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_relayed_detects_circuit_only() {
        let peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        // A real circuit addr: /ip4/.../tcp/.../p2p/<relay>/p2p-circuit/p2p/<me>
        let relay: Multiaddr = format!("/ip4/1.2.3.4/tcp/1883/p2p/{peer}/p2p-circuit/p2p/{peer}")
            .parse()
            .unwrap();
        let direct: Multiaddr = "/ip4/1.2.3.4/tcp/1883".parse().unwrap();
        let direct_udp: Multiaddr = "/ip4/1.2.3.4/udp/1883/quic-v1".parse().unwrap();
        assert!(is_relayed(&relay), "circuit addr is relayed");
        assert!(!is_relayed(&direct), "tcp direct is not relayed");
        assert!(!is_relayed(&direct_udp), "udp direct is not relayed");
    }

    #[test]
    fn parse_relay_strings_accepts_full_multiaddr() {
        // Build a valid full relay multiaddr for an arbitrary real peer id.
        let peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let full = format!("/ip4/1.2.3.4/tcp/4039/p2p/{peer}");
        let specs = vec![full.clone(), "not a multiaddr".to_string()];
        let out = parse_relay_strings(&specs);
        assert_eq!(out.len(), 1, "one valid, one malformed");
        assert_eq!(out[0].0, peer);
        assert_eq!(
            out[0].1.to_string(),
            "/ip4/1.2.3.4/tcp/4039",
            "base should exclude the /p2p/ component"
        );
    }

    #[test]
    fn parse_relay_strings_ignores_missing_p2p() {
        let specs = vec!["/ip4/1.2.3.4/tcp/4039".to_string()];
        assert!(
            parse_relay_strings(&specs).is_empty(),
            "a relay addr without /p2p/ is unusable"
        );
    }

    /// Regression: the exact shape the built-in relay now advertises via
    /// `GET /v1/relays` — a *public* IP (not `0.0.0.0`), the default 1883
    /// port, and a full `/p2p/` peer suffix — must parse into a usable
    /// (relay_peer, dialable_base) pair. This is the address a remote client
    /// dials to reserve a circuit, so a parse failure here silently kills
    /// cross-NAT delivery.
    #[test]
    fn parses_real_relay_advertise_shape() {
        let peer = "12D3KooWL7v2eLzk86VrBvkt33GiXqBH9rkx5FZPnJJBLuW3tqLP"
            .parse::<PeerId>()
            .expect("valid base58 peer id from the live relay");
        let specs = vec![
            format!("/ip4/139.159.201.87/tcp/1883/p2p/{peer}")
        ];
        let out = parse_relay_strings(&specs);
        assert_eq!(out.len(), 1, "relay advertise addr must parse");
        assert_eq!(out[0].0, peer);
        assert_eq!(
            out[0].1.to_string(),
            "/ip4/139.159.201.87/tcp/1883",
            "dialable base keeps the public IP + port, drops /p2p/"
        );
    }
}
