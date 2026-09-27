//! photon-import: parallel import engine for Photon.
//!
//! See [`engine`] for the pipeline. Progress is reported via
//! `crossbeam_channel::Sender<ImportProgress>`.

pub mod dedup;
pub mod engine;
pub mod library;
pub mod metadata;
pub mod sidecar;
pub mod sources;
pub mod thumbnails;

#[cfg(test)]
mod testutil;

pub use engine::{ImportConfig, ImportEngine};
pub use sources::{DiskSource, ImportSource};
