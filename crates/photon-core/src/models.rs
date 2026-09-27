//! Core domain models for Photon.
//!
//! These types are the shared vocabulary across all crates.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Image
// ---------------------------------------------------------------------------

/// A photo/image in the library.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Image {
    pub id: Option<i64>,
    pub hash: String,  // Blake3 hash (deduplication key)
    pub path: PathBuf, // Absolute path or relative to library root
    pub filename: String,
    /// Name of the file as it came off the camera/source. Differs from
    /// `filename` when the library copy was renamed to avoid a collision.
    pub original_filename: Option<String>,
    pub size_bytes: i64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub created_at: Option<i64>, // EXIF DateTimeOriginal as unix timestamp
    pub imported_at: i64,
    pub format: Option<ImageFormat>,
    pub has_sidecar: bool,
    pub metadata_json: Option<String>,
    pub thumbnail_hash: Option<String>,

    // Denormalized EXIF fields for fast display (also in metadata_json)
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens_model: Option<String>,
    pub focal_length: Option<f64>,
    pub aperture: Option<f64>,
    pub shutter_speed: Option<String>,
    pub iso: Option<i32>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub location_name: Option<String>,
    /// EXIF orientation tag (1–8); `None` means unknown / upright.
    pub orientation: Option<u16>,

    // Organization
    pub rating: i32,
    pub flagged: i32, // -1 = rejected, 0 = unflagged, 1 = pick
    pub hidden: bool,
    pub title: Option<String>,
    pub description: Option<String>,

    // Sidecar grouping: images with the same group_hash are variants of
    // the same shot (RAW+JPG pair, _modified edits, XMP sidecars).
    // Computed from blake3(parent_dir + "/" + base_stem).
    pub group_hash: Option<String>,
    /// Micro-preview representation (~25-30 bytes) for instant placeholder rendering.
    pub thumbhash: Option<Vec<u8>>,
}

impl Image {
    pub fn new(path: PathBuf, hash: String, size_bytes: i64) -> Self {
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        Self {
            original_filename: Some(filename.clone()),
            filename,
            path,
            hash,
            size_bytes,
            imported_at: Utc::now().timestamp(),
            ..Default::default()
        }
    }

    /// Returns the base stem, stripping known edit suffixes like "_modified".
    pub fn base_stem(&self) -> String {
        let stem = self.path.file_stem().unwrap_or_default().to_string_lossy();
        strip_edit_suffix(&stem).to_string()
    }

    /// Returns true if this image is an edited variant (has _modified etc. suffix).
    pub fn is_edited_variant(&self) -> bool {
        let stem = self.path.file_stem().unwrap_or_default().to_string_lossy();
        strip_edit_suffix(&stem).len() != stem.len()
    }
}

/// Suffixes editors append to derived files ("IMG_001_modified.jpg").
const EDIT_SUFFIXES: &[&str] = &["_modified", "_edit", "_edited", "-edit"];

/// Strip a known edit suffix from a file stem: `"IMG_001_edit"` → `"IMG_001"`.
pub fn strip_edit_suffix(stem: &str) -> &str {
    EDIT_SUFFIXES
        .iter()
        .find_map(|suffix| stem.strip_suffix(suffix))
        .unwrap_or(stem)
}

// ---------------------------------------------------------------------------
// ImageFormat
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Webp,
    Gif,
    Heif,
    Avif,
    Tiff,
    // RAW variants
    RawOrf, // Olympus
    RawCr2, // Canon CR2
    RawCr3, // Canon CR3
    RawNef, // Nikon
    RawArw, // Sony
    RawDng, // Adobe DNG
    RawRaf, // Fujifilm
    RawRw2, // Panasonic
    Unknown,
}

impl ImageFormat {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "jpg" | "jpeg" => Self::Jpeg,
            "png" => Self::Png,
            "webp" => Self::Webp,
            "gif" => Self::Gif,
            "heif" | "heic" => Self::Heif,
            "avif" => Self::Avif,
            "tiff" | "tif" => Self::Tiff,
            "orf" => Self::RawOrf,
            "cr2" => Self::RawCr2,
            "cr3" => Self::RawCr3,
            "nef" => Self::RawNef,
            "arw" => Self::RawArw,
            "dng" => Self::RawDng,
            "raf" => Self::RawRaf,
            "rw2" => Self::RawRw2,
            _ => Self::Unknown,
        }
    }

    /// Parse from DB-stored format string (e.g. "JPEG", "RAW_ORF") or file extension.
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "JPEG" => Self::Jpeg,
            "PNG" => Self::Png,
            "WEBP" => Self::Webp,
            "GIF" => Self::Gif,
            "HEIF" => Self::Heif,
            "AVIF" => Self::Avif,
            "TIFF" => Self::Tiff,
            "RAW_ORF" => Self::RawOrf,
            "RAW_CR2" => Self::RawCr2,
            "RAW_CR3" => Self::RawCr3,
            "RAW_NEF" => Self::RawNef,
            "RAW_ARW" => Self::RawArw,
            "RAW_DNG" => Self::RawDng,
            "RAW_RAF" => Self::RawRaf,
            "RAW_RW2" => Self::RawRw2,
            "UNKNOWN" => Self::Unknown,
            // Fallback: try as extension
            other => Self::from_extension(other),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::Webp => "WEBP",
            Self::Gif => "GIF",
            Self::Heif => "HEIF",
            Self::Avif => "AVIF",
            Self::Tiff => "TIFF",
            Self::RawOrf => "RAW_ORF",
            Self::RawCr2 => "RAW_CR2",
            Self::RawCr3 => "RAW_CR3",
            Self::RawNef => "RAW_NEF",
            Self::RawArw => "RAW_ARW",
            Self::RawDng => "RAW_DNG",
            Self::RawRaf => "RAW_RAF",
            Self::RawRw2 => "RAW_RW2",
            Self::Unknown => "UNKNOWN",
        }
    }

    pub fn is_raw(&self) -> bool {
        matches!(
            self,
            Self::RawOrf
                | Self::RawCr2
                | Self::RawCr3
                | Self::RawNef
                | Self::RawArw
                | Self::RawDng
                | Self::RawRaf
                | Self::RawRw2
        )
    }

    /// All extensions that Photon recognises as importable images.
    pub fn all_extensions() -> &'static [&'static str] {
        &[
            "jpg", "jpeg", "png", "webp", "gif", "heif", "heic", "avif", "tiff", "tif", "orf",
            "cr2", "cr3", "nef", "arw", "dng", "raf", "rw2",
        ]
    }
}

// ---------------------------------------------------------------------------
// Tag
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: Option<i64>,
    pub name: String,
    pub color: Option<String>, // Hex #RRGGBB
    pub is_category: bool,
}

// ---------------------------------------------------------------------------
// ImportBatch
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ImportBatch {
    pub id: Option<i64>,
    pub source_type: String,
    pub source_path: String,
    pub total_files: i32,
    pub imported_count: i32,
    pub duplicate_count: i32,
    pub error_count: i32,
    pub status: String,
    pub started_at: i64,
    pub completed_at: Option<i64>,
}

impl ImportBatch {
    pub fn new(source_type: String, source_path: String, total_files: i32) -> Self {
        Self {
            source_type,
            source_path,
            total_files,
            started_at: Utc::now().timestamp(),
            status: "running".to_string(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// ImportProgress — sent from import engine → UI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum ImportProgress {
    /// Scanning finished; `total` files will be examined.
    Started { total: usize },
    /// A file finished (imported, skipped as duplicate, or failed).
    Processing {
        processed: usize,
        total: usize,
        current_file: String,
    },
    /// New photos were committed to the library and have thumbnails, so the
    /// UI can show them now. `imported` is the running total.
    Committed { imported: usize },
    Completed {
        imported: usize,
        duplicates: usize,
        errors: Vec<String>,
        cancelled: bool,
    },
}

// ---------------------------------------------------------------------------
// TimelineItem — the minimum needed to lay out and draw one grid tile
// ---------------------------------------------------------------------------

/// Lightweight projection of an image for the virtualized timeline. Loading
/// 100k of these is cheap; full `Image` rows are fetched only when opened.
#[derive(Debug, Clone)]
pub struct TimelineItem {
    pub id: i64,
    pub hash: String,
    pub created_at: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub orientation: Option<u16>,
    pub thumbhash: Option<Vec<u8>>,
    pub rating: i32,
    pub flagged: i32,
}

impl TimelineItem {
    /// Width / height as displayed (EXIF orientations 5–8 swap the axes).
    /// Unknown dimensions default to 3:2.
    pub fn display_aspect(&self) -> f64 {
        let (w, h) = match (self.width, self.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (w as f64, h as f64),
            _ => (3.0, 2.0),
        };
        if matches!(self.orientation, Some(5..=8)) {
            h / w
        } else {
            w / h
        }
    }
}

// ---------------------------------------------------------------------------
// LibraryQuery — flexible query builder input
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct LibraryQuery {
    pub year: Option<i32>,
    pub month: Option<u32>,
    pub day: Option<u32>,
    pub tags: Vec<String>,
    pub search_text: Option<String>, // FTS5
    pub format_filter: Option<Vec<ImageFormat>>,
    pub date_range: Option<(i64, i64)>, // unix timestamps
    pub limit: Option<i32>,
    pub offset: Option<i32>,
}

// ---------------------------------------------------------------------------
// UIAction — sidebar/menu → main window communication
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum UIAction {
    ShowAll,
    FilterByYear(i32),
    FilterByDate(i32, u32),
    FilterByDay(i32, u32, u32),
    Search(String),
    FilterByTag(String),
    /// View a single photo by index into the current photo list.
    ViewPhoto(usize),
}

// ---------------------------------------------------------------------------
// FolderImportMode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderImportMode {
    /// Register files where they are (no copy/move).
    InPlace,
    /// Copy files into the Photon library directory.
    Copy,
    /// Move files into the Photon library directory.
    Move,
}

// ---------------------------------------------------------------------------
// Preferences — persisted in library_meta table
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    /// Command to open raster images (JPEG, PNG, WEBP, etc.)
    pub raster_editor: String,
    /// Command to open RAW images (ORF, CR2, NEF, DNG, etc.)
    pub raw_editor: String,
    /// Default image viewer command
    pub viewer: String,
    /// Thumbnail grid size in pixels (width of each card)
    pub thumbnail_size: u32,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            raster_editor: "gimp".to_string(),
            raw_editor: "darktable".to_string(),
            viewer: "xdg-open".to_string(),
            thumbnail_size: 200,
        }
    }
}

// ---------------------------------------------------------------------------
// EditRecord — tracks external edits to an image
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditRecord {
    pub id: Option<i64>,
    pub image_id: i64,
    pub tool_name: String,
    pub operation_type: Option<String>,
    pub sidecar_path: Option<String>,
    pub edited_at: i64,
}

// ---------------------------------------------------------------------------
// DesktopApp — discovered from .desktop files
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopApp {
    pub name: String,
    pub exec: String, // e.g. "gimp" (stripped of %f %U etc.)
    pub handles_raw: bool,
    pub handles_raster: bool,
    pub handles_viewer: bool,
}
