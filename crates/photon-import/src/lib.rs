//! photon-import: parallel import engine for Photon.
//!
//! See [`engine`] for the pipeline. Progress is reported via
//! `crossbeam_channel::Sender<ImportProgress>`.

pub mod dedup;
pub mod device;
pub mod engine;
pub mod library;
pub mod metadata;
pub mod raw;
pub mod export;
pub mod sidecar;
pub mod sources;
pub mod thumbnails;
pub mod quality;

#[cfg(test)]
mod testutil;

pub use engine::{ImportConfig, ImportEngine};
pub use export::{batch_export, export_single, ExportConfig, ExportFormat, ExportReport, ExportResize};
pub use quality::{
    compute_quality, find_bursts, suggest_rejects, Burst, BurstItem, QualityScore, RejectSuggestion,
    BLUR_SHARPNESS_FLOOR, BURST_SHARPNESS_REJECT_RATIO, HIGHLIGHT_CLIP_REJECT_THRESHOLD,
    QUALITY_LONG_EDGE, QUALITY_VERSION,
};
pub use sidecar::{
    read_image_xmp, reconcile_shots, repair_darktable_keyword_tags, sync_xmp_metadata, write_image_xmp, Keywords,
    XmpUpdate,
};
pub use sources::{DiskSource, ImportSource};
pub use thumbnails::{compute_histogram, HistogramData};
pub mod icc;
