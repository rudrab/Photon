//! photon-import: parallel import engine for Photon.
//!
//! See [`engine`] for the pipeline. Progress is reported via
//! `crossbeam_channel::Sender<ImportProgress>`.

pub mod dedup;
pub mod engine;
pub mod library;
pub mod metadata;
pub mod raw;
pub mod export;
pub mod sidecar;
pub mod sources;
pub mod thumbnails;

#[cfg(test)]
mod testutil;

pub use engine::{ImportConfig, ImportEngine};
pub use export::{batch_export, export_single, ExportConfig, ExportFormat, ExportReport, ExportResize};
pub use sidecar::{read_image_xmp, sync_xmp_metadata, write_image_xmp, Keywords, XmpUpdate};
pub use sources::{DiskSource, ImportSource};
pub use thumbnails::{compute_histogram, HistogramData};
pub mod icc;
