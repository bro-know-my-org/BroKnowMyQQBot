//! Browser command execution boundary.

use std::collections::BTreeSet;

use async_trait::async_trait;
use plugin_api::{BrowserPermission, BrowserRun};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserArtifact {
    mime_type: String,
    data: Vec<u8>,
}

impl BrowserArtifact {
    pub fn new(mime_type: String, data: Vec<u8>) -> Result<Self, BrowserExecutionError> {
        let signature_matches = match mime_type.as_str() {
            "image/png" => data.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/jpeg" => data.starts_with(&[0xff, 0xd8, 0xff]),
            _ => false,
        };
        if data.is_empty() || !signature_matches {
            return Err(BrowserExecutionError::InvalidRequest(
                "browser artifact is not a supported image".to_owned(),
            ));
        }
        if data.len() > 8 * 1024 * 1024 {
            return Err(BrowserExecutionError::ResourceExhausted(
                "browser artifact exceeds the 8 MiB limit".to_owned(),
            ));
        }
        Ok(Self { mime_type, data })
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn into_parts(self) -> (String, Vec<u8>) {
        (self.mime_type, self.data)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserExecution {
    final_url: String,
    title: String,
    extracted_text: Vec<String>,
    artifacts: Vec<BrowserArtifact>,
}

impl BrowserExecution {
    pub fn new(
        final_url: String,
        title: String,
        extracted_text: Vec<String>,
        artifacts: Vec<BrowserArtifact>,
    ) -> Result<Self, BrowserExecutionError> {
        if final_url.len() > 2048
            || title.len() > 64 * 1024
            || extracted_text.len() > 32
            || artifacts.len() > 32
        {
            return Err(BrowserExecutionError::ResourceExhausted(
                "browser result exceeds Host count or field limits".to_owned(),
            ));
        }
        let text_bytes = extracted_text
            .iter()
            .map(String::len)
            .fold(0_usize, usize::saturating_add);
        let artifact_bytes = artifacts
            .iter()
            .map(|artifact| artifact.data.len())
            .fold(0_usize, usize::saturating_add);
        if text_bytes > 1024 * 1024 || artifact_bytes > 32 * 1024 * 1024 {
            return Err(BrowserExecutionError::ResourceExhausted(
                "browser result exceeds Host count or byte limits".to_owned(),
            ));
        }
        Ok(Self {
            final_url,
            title,
            extracted_text,
            artifacts,
        })
    }

    pub fn into_parts(self) -> (String, String, Vec<String>, Vec<BrowserArtifact>) {
        (
            self.final_url,
            self.title,
            self.extracted_text,
            self.artifacts,
        )
    }
}

#[derive(Debug, Error)]
pub enum BrowserExecutionError {
    #[error("browser capability is unavailable: {0}")]
    Unavailable(String),
    #[error("browser request is denied: {0}")]
    Denied(String),
    #[error("browser request is invalid: {0}")]
    InvalidRequest(String),
    #[error("browser task exceeded a resource limit: {0}")]
    ResourceExhausted(String),
    #[error("browser worker failed: {0}")]
    Worker(String),
}

#[async_trait]
pub trait BrowserExecutor: Send + Sync + 'static {
    /// Implementations must own and terminate task resources when this future
    /// is dropped, including subprocesses spawned for a cancelled invocation.
    async fn execute(
        &self,
        permissions: &[BrowserPermission],
        granted_capabilities: &BTreeSet<String>,
        request: &BrowserRun,
    ) -> Result<BrowserExecution, BrowserExecutionError>;
}

#[derive(Debug, Default)]
pub struct UnavailableBrowserExecutor;

#[async_trait]
impl BrowserExecutor for UnavailableBrowserExecutor {
    async fn execute(
        &self,
        _permissions: &[BrowserPermission],
        _granted_capabilities: &BTreeSet<String>,
        _request: &BrowserRun,
    ) -> Result<BrowserExecution, BrowserExecutionError> {
        Err(BrowserExecutionError::Unavailable(
            "install and configure the optional Browser Runtime".to_owned(),
        ))
    }
}
