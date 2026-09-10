//! 设备身份：每台设备一把 libp2p Ed25519（`peer_id`），与账户身份解耦。
//! - 设备私钥以 libp2p keystore protobuf 编码明文存于 `device.json`（M2 接 OS keychain）。
//! - 账户级 E2E / 审批密钥在 [`account`](crate::account) 模块，口令加密存于 `keystore.json`。
//!
//! 一台机器可持有多个账户（多 profile），每个账户在每个 profile 下有一台"本地设备"。

use libp2p::identity::Keypair;

/// 设备身份：libp2p Keypair（Ed25519）→ `peer_id`。
#[derive(Clone)]
pub struct DeviceIdentity {
    keypair: Keypair,
    peer_id_base58: String,
}

impl DeviceIdentity {
    /// 生成一台新设备（随机 Ed25519）。
    pub fn generate() -> Self {
        let keypair = Keypair::generate_ed25519();
        let peer_id_base58 = libp2p::PeerId::from(keypair.public()).to_base58();
        Self {
            keypair,
            peer_id_base58,
        }
    }

    /// 设备 Ed25519 公钥（base64，32B）。
    /// 用于目录登记与审批证明（与 libp2p `peer_id` 是同一对密钥的不同投影）。
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

    /// 落盘设备私钥（`device.json`）。
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

    // ── profile 目录约定 ──
    /// 基础目录：`$P2PCHAT_HOME` 或 `~/.config/p2pchat`。
    pub fn home_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(
            std::env::var("P2PCHAT_HOME").unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                format!("{home}/.config/p2pchat")
            }),
        )
    }

    /// 某 profile 的目录：`<home>/<profile>`。
    pub fn profile_dir(profile: &str) -> std::path::PathBuf {
        let p = profile.trim().to_lowercase();
        if p.is_empty() {
            Self::home_dir()
        } else {
            Self::home_dir().join(p)
        }
    }

    /// 默认 profile：`$P2PCHAT_PROFILE` 或 `default`。
    pub fn default_profile() -> String {
        std::env::var("P2PCHAT_PROFILE")
            .unwrap_or_else(|_| "default".into())
            .trim()
            .to_lowercase()
    }

    /// 某 profile 的完整目录：`<base>/<profile>`（profile 为 default/空 时即 base）。
    fn base_profile_dir(base: &std::path::Path, profile: &str) -> std::path::PathBuf {
        let p = profile.trim().to_lowercase();
        if p.is_empty() || p == "default" {
            base.to_path_buf()
        } else {
            base.join(p)
        }
    }

    /// 指定 base 目录下的设备文件路径 `<base>/<profile>/device.json`。
    pub fn base_device_path(base: &std::path::Path, profile: &str) -> std::path::PathBuf {
        Self::base_profile_dir(base, profile).join("device.json")
    }

    /// 指定 base 目录下的账户 keystore 路径 `<base>/<profile>/keystore.json`。
    pub fn base_keystore_path(base: &std::path::Path, profile: &str) -> std::path::PathBuf {
        Self::base_profile_dir(base, profile).join("keystore.json")
    }

    /// 指定 base 目录下的群定义目录 `<base>/<profile>/groups/`（每个群一个 `.json`）。
    pub fn base_groups_dir(base: &std::path::Path, profile: &str) -> std::path::PathBuf {
        Self::base_profile_dir(base, profile).join("groups")
    }

    /// 某 profile 下的群定义目录。
    pub fn groups_dir(profile: &str) -> std::path::PathBuf {
        Self::base_groups_dir(&Self::home_dir(), profile)
    }

    /// 当前 profile 的设备文件路径 `<profile>/device.json`。
    pub fn device_path(profile: &str) -> std::path::PathBuf {
        Self::base_device_path(&Self::home_dir(), profile)
    }

    /// 当前 profile 的账户 keystore 文件路径 `<profile>/keystore.json`。
    pub fn keystore_path(profile: &str) -> std::path::PathBuf {
        Self::base_keystore_path(&Self::home_dir(), profile)
    }

    /// 在**指定 base 目录**下取既有设备，没有则生成并落盘（保证同 base+profile 的 peer_id 稳定）。
    pub fn load_or_create_in(base: &std::path::Path, profile: &str) -> anyhow::Result<Self> {
        let path = Self::base_device_path(base, profile);
        if let Ok(d) = Self::load(&path) {
            return Ok(d);
        }
        let d = Self::generate();
        d.save(&path)?;
        Ok(d)
    }

    /// 取当前 profile 的既有设备，没有则生成并落盘（保证同 profile 的 peer_id 稳定）。
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
