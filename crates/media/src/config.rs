use webrtc_rs::ice_transport::ice_server::RTCIceServer;

/// TURN / STUN servers. Empty `turn_url` disables TURN. Three-row env:
/// `CHATX_TURN_URL`, `CHATX_TURN_USER`, `CHATX_TURN_PASS`.
///
/// STUN precedence (highest first):
///   1. `stun` — explicitly supplied by the caller (the directory's
///      operator-hosted STUN from `GET /v1/stun`, and/or `CHATX_STUN`).
///   2. built-in public default `stun:stun.l.google.com:19302`.
///
/// The caller is responsible for populating `stun`; it is empty by default so
/// that a host without a directory falls back to the public STUN.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub turn_url: Option<String>,
    pub turn_user: Option<String>,
    pub turn_pass: Option<String>,
    /// Operator-provided STUN endpoints (as `host:port`), typically discovered
    /// from the directory at login. Highest precedence; empty → public default.
    pub stun: Vec<String>,
}

/// Normalize a STUN reference into a `stun:` URL. Accepts either a bare
/// `host:port` (the directory's `StunInfo.addr` format) or a full
/// `stun:…` / `stuns:…` URL. Empty / whitespace input is rejected (None).
fn stun_url(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if t.starts_with("stun:") || t.starts_with("stuns:") {
        Some(t.to_string())
    } else {
        Some(format!("stun:{t}"))
    }
}

impl Config {
    pub fn from_env() -> Self {
        let turn_url = std::env::var("CHATX_TURN_URL").ok().filter(|s| !s.trim().is_empty());
        let turn_user = std::env::var("CHATX_TURN_USER").ok().filter(|s| !s.trim().is_empty());
        let turn_pass = std::env::var("CHATX_TURN_PASS").ok().filter(|s| !s.trim().is_empty());
        let stun: Vec<String> = std::env::var("CHATX_STUN")
            .ok()
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Self { turn_url, turn_user, turn_pass, stun }
    }

    /// Whether TURN is configured and usable.
    pub fn has_turn(&self) -> bool {
        self.turn_url.is_some() && self.turn_user.is_some() && self.turn_pass.is_some()
    }

    /// Set the operator-provided STUN endpoints (highest precedence). The
    /// app calls this from the directory's login-time discovery so WebRTC ICE
    /// prefers the operator's own STUN over the public default.
    pub fn with_stun(mut self, servers: Vec<String>) -> Self {
        self.stun = servers
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        self
    }

    pub fn ice_servers(&self) -> Vec<RTCIceServer> {
        let mut v = Vec::new();

        // TURN (URL / username / credential all required; webrtc-rs refuses
        // TURN without credentials).
        if let (Some(url), Some(user), Some(pass)) =
            (self.turn_url.clone(), self.turn_user.clone(), self.turn_pass.clone())
        {
            v.push(RTCIceServer {
                urls: vec![url],
                username: user,
                credential: pass,
            });
        }

        // STUN: prefer the operator-provided endpoints (each becomes its own
        // RTCIceServer so ICE can use them all); fall back to the built-in
        // public STUN only when none were supplied.
        let mut urls: Vec<String> = self
            .stun
            .iter()
            .filter_map(|s| stun_url(s))
            .collect();
        if urls.is_empty() {
            urls.push("stun:stun.l.google.com:19302".into());
        }
        v.push(RTCIceServer {
            urls,
            username: String::new(),
            credential: String::new(),
        });

        v
    }
}
