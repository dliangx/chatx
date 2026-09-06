//! 聊天消息模型（request-response JSON 编解码）。
//!
//! M1 握手策略：
//! - 每条 `ChatRequest` 携带发送方 E2E 公钥（`e2e`）+ 文本；
//! - 每条 `ChatResponse` 携带响应方 E2E 公钥（`e2e`）；
//! - 双方各自 `ecdh(my_static, other_e2e)` 派生**同一把** AES-256 会话密钥；
//! - 第一条消息即完成密钥协商与文本投递，无需独立 Hello 往返。

use serde::{Deserialize, Serialize};

const _PROTOCOL: &str = "/p2pchat/text/1";

/// 发送一条文本消息（含可选 E2E 密文）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    /// 消息唯一 id（客户端自增 / 时间戳混种）。
    pub id: u64,
    /// 发送方 peer（可打印 base58）。
    pub from: String,
    /// 发送方 E2E 公钥（base64，X25519）。
    pub e2e: String,
    /// 明文文本（M1 演示用）。
    pub text: Option<String>,
    /// 密文（nonce12 || ciphertext+tag），base64。M1 演示留空。
    pub sealed: Option<String>,
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
