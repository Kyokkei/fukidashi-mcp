use std::path::{Path, PathBuf};

use crate::{
    config::ensure_file,
    error::{FukidashiError, Result},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Cpu,
    Cuda,
    DirectMl,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub dylib: Option<PathBuf>,
    pub provider: Provider,
}

/// Validate the explicitly provisioned runtime without loading native code.
pub fn validate_runtime(path: Option<&Path>) -> Result<()> {
    if let Some(path) = path {
        ensure_file(path)?;
    }
    Ok(())
}

pub fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Cpu => "cpu",
        Provider::Cuda => "cuda",
        Provider::DirectMl => "directml",
    }
}

pub fn unavailable(message: impl Into<String>) -> FukidashiError {
    FukidashiError::RuntimeUnavailable(message.into())
}
