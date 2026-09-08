use std::path::PathBuf;

use thiserror::Error;

/// Stable failures exposed by the core processing pipeline.
#[derive(Debug, Error)]
pub enum FukidashiError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("missing model or runtime asset: {path}")]
    MissingAsset { path: PathBuf },
    #[error("runtime is unavailable: {0}")]
    RuntimeUnavailable(String),
    #[error("invalid tensor contract: {0}")]
    TensorContract(String),
    #[error("inference failed: {0}")]
    Inference(String),
    #[error("image or archive exceeds configured limits: {0}")]
    ResourceLimit(String),
    #[error("text does not fit its bubble: {0}")]
    TextOverflow(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("image error: {0}")]
    Image(#[from] image::ImageError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, FukidashiError>;
