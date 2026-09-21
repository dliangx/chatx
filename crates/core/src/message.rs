
use serde::{Deserialize, Serialize};

const _PROTOCOL: &str = "/p2pchat/text/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MsgKind {
    #[default]
    Dm,
    GroupKey,
    GroupMsg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub id: u64,
    pub from: String,
    pub e2e: String,
    pub text: Option<String>,
    pub sealed: Option<String>,
    #[serde(default)]
    pub kind: MsgKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: u64,
    pub e2e: String,
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
