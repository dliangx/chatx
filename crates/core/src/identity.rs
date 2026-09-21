
use libp2p::identity::Keypair;

#[derive(Clone)]
pub struct DeviceIdentity {
    keypair: Keypair,
    peer_id_base58: String,
}

impl DeviceIdentity {
    pub fn generate() -> Self {
        let keypair = Keypair::generate_ed25519();
        let peer_id_base58 = libp2p::PeerId::from(keypair.public()).to_base58();
        Self {
            keypair,
            peer_id_base58,
        }
    }

    pub fn public_base64(&self) -> String {
        let ed_pk = self
            .keypair
            .public()
            .try_into_ed25519()
            .expect("device uses Ed25519");
        crate::account::b64(&ed_pk.to_bytes())
    }

    pub fn peer_id(&self) -> libp2p::PeerId {
        libp2p::PeerId::from(&self.keypair.public())
    }

    pub fn peer_base58(&self) -> &str {
        &self.peer_id_base58
    }

    pub fn keypair(&self) -> Keypair {
        self.keypair.clone()
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let kp = self.keypair.to_protobuf_encoding().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e)
        })?;
        let bundle: Vec<u8> = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&kp).into_bytes()
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bundle)
    }

    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        use base64::Engine as _;
        let bundle = std::fs::read(path)?;
        let kp: Vec<u8> = base64::engine::general_purpose::STANDARD
            .decode(&bundle)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let keypair = Keypair::from_protobuf_encoding(&kp)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let peer_id_base58 = libp2p::PeerId::from(keypair.public()).to_base58();
        Ok(Self {
            keypair,
            peer_id_base58,
        })
    }

    pub fn home_dir() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("P2PCHAT_HOME") {
            return std::path::PathBuf::from(p);
        }
        #[cfg(windows)]
        let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
        #[cfg(not(windows))]
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(format!("{home}/.config/p2pchat"))
    }

    pub fn profile_dir(profile: &str) -> std::path::PathBuf {
        let p = profile.trim().to_lowercase();
        if p.is_empty() {
            Self::home_dir()
        } else {
            Self::home_dir().join(p)
        }
    }

    pub fn default_profile() -> String {
        std::env::var("P2PCHAT_PROFILE")
            .unwrap_or_else(|_| "default".into())
            .trim()
            .to_lowercase()
    }

    fn base_profile_dir(base: &std::path::Path, profile: &str) -> std::path::PathBuf {
        let p = profile.trim().to_lowercase();
        if p.is_empty() || p == "default" {
            base.to_path_buf()
        } else {
            base.join(p)
        }
    }

    pub fn base_device_path(base: &std::path::Path, profile: &str) -> std::path::PathBuf {
        Self::base_profile_dir(base, profile).join("device.json")
    }

    pub fn base_keystore_path(base: &std::path::Path, profile: &str) -> std::path::PathBuf {
        Self::base_profile_dir(base, profile).join("keystore.json")
    }

    pub fn base_groups_dir(base: &std::path::Path, profile: &str) -> std::path::PathBuf {
        Self::base_profile_dir(base, profile).join("groups")
    }

    pub fn groups_dir(profile: &str) -> std::path::PathBuf {
        Self::base_groups_dir(&Self::home_dir(), profile)
    }

    pub fn device_path(profile: &str) -> std::path::PathBuf {
        Self::base_device_path(&Self::home_dir(), profile)
    }

    pub fn keystore_path(profile: &str) -> std::path::PathBuf {
        Self::base_keystore_path(&Self::home_dir(), profile)
    }

    pub fn load_or_create_in(base: &std::path::Path, profile: &str) -> anyhow::Result<Self> {
        let path = Self::base_device_path(base, profile);
        if let Ok(d) = Self::load(&path) {
            return Ok(d);
        }
        let d = Self::generate();
        d.save(&path)?;
        Ok(d)
    }

    pub fn load_or_create(profile: &str) -> anyhow::Result<Self> {
        Self::load_or_create_in(&Self::home_dir(), profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_save_load_roundtrip() {
        let d = DeviceIdentity::generate();
        let path = std::env::temp_dir().join(format!("p2pchat-dev-{}.json", std::process::id()));
        d.save(&path).unwrap();
        let d2 = DeviceIdentity::load(&path).unwrap();
        assert_eq!(d2.peer_base58(), d.peer_base58());
        let _ = std::fs::remove_file(path);
    }
}
