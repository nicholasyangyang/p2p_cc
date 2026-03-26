use thiserror::Error;

#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum AppError {
    #[error("key error: {0}")]
    Keys(String),

    #[error("json error: {0}")]
    Json(String),

    #[error("nostr error: {0}")]
    Nostr(String),

    #[error("broker error: {0}")]
    Broker(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}
