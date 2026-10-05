use webrtc_rs::ice_transport::ice_server::RTCIceServer;

/// TURN / STUN servers. Empty `turn_url` disables TURN. Three-row env:
/// `P2PCHAT_TURN_URL`, `P2PCHAT_TURN_USER`, `P2PCHAT_TURN_PASS`.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub turn_url: Option<String>,
    pub turn_user: Option<String>,
    pub turn_pass: Option<String>,
    /// Default `stun:stun.l.google.com:19302` if absent.
    pub stun_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let turn_url = std::env::var("P2PCHAT_TURN_URL").ok().filter(|s| !s.trim().is_empty());
        let turn_user = std::env::var("P2PCHAT_TURN_USER").ok().filter(|s| !s.trim().is_empty());
        let turn_pass = std::env::var("P2PCHAT_TURN_PASS").ok().filter(|s| !s.trim().is_empty());
        let stun_url = std::env::var("P2PCHAT_STUN").ok().filter(|s| !s.trim().is_empty());
        Self { turn_url, turn_user, turn_pass, stun_url }
    }

    /// Whether TURN is configured and usable.
    pub fn has_turn(&self) -> bool {
        self.turn_url.is_some() && self.turn_user.is_some() && self.turn_pass.is_some()
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

        // STUN (default provided when unset).
        let stun = self
            .stun_url
            .clone()
            .unwrap_or_else(|| "stun:stun.l.google.com:19302".into());
        v.push(RTCIceServer {
            urls: vec![stun],
            username: String::new(),
            credential: String::new(),
        });

        v
    }
}
