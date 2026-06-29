use thiserror::Error;

/// One error type for the whole core. Drivers map their failures into `Driver`.
#[derive(Error, Debug)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("config: {0}")]
    Config(String),
    #[error("driver: {0}")]
    Driver(String),
    #[error("anchor not found: {0}")]
    Anchor(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
