use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("webrtc: {0}")]
    Webrtc(#[from] webrtc_rs::error::Error),
    #[error("openh264: {0}")]
    OpenH264(#[from] openh264::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}
