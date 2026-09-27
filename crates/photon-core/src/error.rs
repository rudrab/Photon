//! Unified error types for photon-core.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PhotonError {
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("Database pool error: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Metadata parse error: {0}")]
    MetadataParse(String),

    #[error("Corrupt file at {path}: {reason}")]
    CorruptFile { path: String, reason: String },

    #[error("Disk full")]
    DiskFull,

    #[error("External tool not found: {0}")]
    ToolNotFound(String),

    #[error("Migration error: {0}")]
    Migration(String),

    #[error("{0}")]
    Other(String),
}

impl PhotonError {
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::CorruptFile { .. } | Self::ToolNotFound(_) | Self::MetadataParse(_)
        )
    }
}

impl From<anyhow::Error> for PhotonError {
    fn from(e: anyhow::Error) -> Self {
        PhotonError::Other(e.to_string())
    }
}
