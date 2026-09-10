//! 聊天消息模型（request-response JSON 编解码）。
//!
//! M1 握手策略：
//! - 每条 `ChatRequest` 携带发送方 E2E 公钥（`e2e`）+ 文本；
//! - 每条 `ChatResponse` 携带响应方 E2E 公钥（`e2e`）；
//! - 双方各自 `ecdh(my_static, other_e2e)` 派生**同一把** AES-256 会话密钥；
//! - 第一条消息即完成密钥协商与文本投递，无需独立 Hello 往返。

use serde::{Deserialize, Serialize};

const _PROTOCOL: &str = "/p2pchat/text/1";

/// 消息类型：决定 `text` / `sealed` / `group_id` 字段如何解释。
/// - `dm`        — 1:1；`text`=明文 / `sealed`=1:1 密文（M1 演示）；`group_id`=None。
/// - `group-key` — 群密钥 E2E 私发；`sealed`=base64(serde_json<SecretEnvelope>)；`group_id` 快速路由。
/// - `group-msg` — 群消息；`sealed`=base64(serde_json<SealedGroupMsg>)；`group_id` 快速路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MsgKind {
    #[default]
    Dm,
    GroupKey,
    GroupMsg,
}

/// 发送一条消息（1:1 / 群密钥 / 群消息三合一）。
///
/// 兼容性：老客户端不识别 `kind` / `group_id`，反序列化时按 `Dm` 处理；
/// `sealed` 字段原本就预留密文用途，此处复用为群相关的不透明 blob。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    /// 消息唯一 id（客户端自增 / 时间戳混种）。
    pub id: u64,
    /// 发送方 peer（可打印 base58）。
    pub from: String,
    /// 发送方 E2E 公钥（base64，X25519）。
    pub e2e: String,
    /// 1:1 明文文本（`kind=Dm` 时使用）。
    pub text: Option<String>,
    /// 密文/不透明 blob：1:1 密文（M1）`nonce12||ct+tag`、群密钥 `base64(SecretEnvelope)`、群消息 `base64(SealedGroupMsg)`。
    pub sealed: Option<String>,
    /// 消息类型（缺省 `Dm`）。
    #[serde(default)]
    pub kind: MsgKind,
    /// 群组 id（`kind=GroupKey` / `GroupMsg` 时必填；`Dm` 时忽略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
}

/// 响应：确认收到 + 回带我方 E2E 公钥。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    /// 对应 request 的 id。
    pub id: u64,
    /// 响应方 E2E 公钥（base64，X25519）。
    pub e2e: String,
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
