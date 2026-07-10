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
    /// A gate verdict tool exceeded its wall-clock ceiling. Distinguished from `Driver` so a
    /// caller can tell "hung tool" from "tool absent/broken": a timeout must PROPAGATE (the edit
    /// is refused, disk untouched) and must never route into a weaker-fallback path — swapping
    /// verdict engines on a hang is the silent gate degrade CONTRIBUTING forbids.
    #[error("gate timeout: {0}")]
    GateTimeout(String),
    #[error("anchor not found: {0}")]
    Anchor(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
