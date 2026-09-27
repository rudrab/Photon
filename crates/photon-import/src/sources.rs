//! Import source adapters.
//!
//! Each source knows how to scan for importable files and provide optional
//! metadata hints (e.g. Shotwell's DB already has dates/tags).

pub mod digikam;
pub mod disk;
pub mod shotwell;

pub use disk::DiskSource;
pub use shotwell::ShotwellSource;

use std::path::PathBuf;

/// Trait implemented by all import sources.
pub trait ImportSource: Send + Sync {
    /// Scan and return all importable file paths.
    fn scan(&self) -> anyhow::Result<Vec<PathBuf>>;

    /// Human-readable source type (e.g. "folder", "shotwell", "digikam").
    fn source_type(&self) -> &str;

    /// Human-readable description (e.g. the root path or DB path).
    fn source_description(&self) -> &str;
}
