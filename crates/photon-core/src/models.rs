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
    #[serde(default)]
    pub xmp_mtime: Option<i64>,
    #[serde(default)]
    pub missing: bool,
    pub is_video: bool,
    pub duration: Option<i32>,
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
    // Video variants
    VideoMp4,
    VideoMov,
    VideoM4v,
    VideoMts,
    VideoM2ts,
    VideoAvi,
    VideoMkv,
    Video3gp,
    VideoWebm,
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
            "mp4" => Self::VideoMp4,
            "mov" => Self::VideoMov,
            "m4v" => Self::VideoM4v,
            "mts" => Self::VideoMts,
            "m2ts" => Self::VideoM2ts,
            "avi" => Self::VideoAvi,
            "mkv" => Self::VideoMkv,
            "3gp" => Self::Video3gp,
            "webm" => Self::VideoWebm,
            _ => Self::Unknown,
        }
    }

    /// Parse from DB-stored format string (e.g. "JPEG", "RAW_ORF", "VIDEO_MP4") or file extension.
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
            "VIDEO_MP4" => Self::VideoMp4,
            "VIDEO_MOV" => Self::VideoMov,
            "VIDEO_M4V" => Self::VideoM4v,
            "VIDEO_MTS" => Self::VideoMts,
            "VIDEO_M2TS" => Self::VideoM2ts,
            "VIDEO_AVI" => Self::VideoAvi,
            "VIDEO_MKV" => Self::VideoMkv,
            "VIDEO_3GP" => Self::Video3gp,
            "VIDEO_WEBM" => Self::VideoWebm,
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
            Self::VideoMp4 => "VIDEO_MP4",
            Self::VideoMov => "VIDEO_MOV",
            Self::VideoM4v => "VIDEO_M4V",
            Self::VideoMts => "VIDEO_MTS",
            Self::VideoM2ts => "VIDEO_M2TS",
            Self::VideoAvi => "VIDEO_AVI",
            Self::VideoMkv => "VIDEO_MKV",
            Self::Video3gp => "VIDEO_3GP",
            Self::VideoWebm => "VIDEO_WEBM",
            Self::Unknown => "UNKNOWN",
        }
    }

    pub fn is_video(&self) -> bool {
        matches!(
            self,
            Self::VideoMp4
                | Self::VideoMov
                | Self::VideoM4v
                | Self::VideoMts
                | Self::VideoM2ts
                | Self::VideoAvi
                | Self::VideoMkv
                | Self::Video3gp
                | Self::VideoWebm
        )
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

    /// All extensions that Photon recognises as importable images and videos.
    pub fn all_extensions() -> &'static [&'static str] {
        &[
            "jpg", "jpeg", "png", "webp", "gif", "heif", "heic", "avif", "tiff", "tif", "orf",
            "cr2", "cr3", "nef", "arw", "dng", "raf", "rw2", "mp4", "mov", "m4v", "mts", "m2ts",
            "avi", "mkv", "3gp", "webm",
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
#[derive(Debug, Clone, Default)]
pub struct TimelineItem {
    pub is_video: bool,
    pub duration: Option<String>,
    pub id: i64,
    pub hash: String,
    pub created_at: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub orientation: Option<u16>,
    pub thumbhash: Option<Vec<u8>>,
    pub rating: i32,
    pub flagged: i32,
    pub missing: bool,
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
    FilterByAlbum(i64),
    FilterByEvent(i64),
    FilterMissing,
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
    #[serde(default)]
    pub delete_mode: Versions,
    #[serde(default = "Versions::share_default")]
    pub share_versions: Versions,
    #[serde(default = "default_true")]
    pub verify_imports: bool,
    #[serde(default)]
    pub import_backup_dir: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Versions {
    /// The RAW, the raster copies and the edits.
    #[default]
    AllVersions,
    /// Only the RAW files; JPGs and edits stay.
    RawOnly,
    /// Only the raster files (JPGs, edits); the RAW stays.
    RasterOnly,
}

impl Versions {
    pub const ALL: [Versions; 3] = [Self::AllVersions, Self::RawOnly, Self::RasterOnly];

    /// Chats and photo sites can't show RAW files.
    pub fn share_default() -> Self {
        Self::RasterOnly
    }

    fn matches(self, img: &Image) -> bool {
        let raw = img.format.is_some_and(|f| f.is_raw());
        match self {
            Self::AllVersions => true,
            Self::RawOnly => raw,
            Self::RasterOnly => !raw,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AllVersions => "All versions",
            Self::RawOnly => "RAW only",
            Self::RasterOnly => "Raster only (JPG, edits)",
        }
    }

    /// Which of a shot's `versions` to delete, given the ones the user
    /// `selected` among them. Falls back to the selection when the mode
    /// matches none of the versions (e.g. "RAW only" on a JPG + edit pair),
    /// so Delete never silently does nothing.
    pub fn pick<'a>(self, versions: &'a [Image], selected: &[&'a Image]) -> Vec<&'a Image> {
        let picked: Vec<&Image> = versions.iter().filter(|v| self.matches(v)).collect();
        if picked.is_empty() {
            selected.to_vec()
        } else {
            picked
        }
    }

    /// Like [`pick`](Self::pick), but only swaps versions when needed:
    /// selected versions of the right kind are taken as they are (sharing a
    /// JPG doesn't also send its edits); a shot selected only by its other
    /// kind (the RAW, when sharing raster) sends its versions of the right
    /// kind instead.
    pub fn pick_selected<'a>(self, versions: &'a [Image], selected: &[&'a Image]) -> Vec<&'a Image> {
        let chosen: Vec<&Image> = selected.iter().copied().filter(|v| self.matches(v)).collect();
        if chosen.is_empty() {
            self.pick(versions, selected)
        } else {
            chosen
        }
    }
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            raster_editor: "gimp".to_string(),
            raw_editor: "darktable".to_string(),
            viewer: "xdg-open".to_string(),
            thumbnail_size: 200,
            delete_mode: Versions::default(),
            share_versions: Versions::share_default(),
            verify_imports: true,
            import_backup_dir: None,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Album {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub cover_image_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub id: i64,
    pub name: String,
    pub start_date: i64,
    pub end_date: i64,
    pub comment: Option<String>,
}

/// Compose an existing EXIF orientation (1–8) with a 90° clockwise (`cw == true`)
/// or 90° counter-clockwise (`cw == false`) rotation.
///
/// Unset, zero or out-of-range orientations are treated as 1 (normal).
pub fn rotate_orientation(orientation: Option<u16>, cw: bool) -> u16 {
    let o = orientation.filter(|&v| (1..=8).contains(&v)).unwrap_or(1);
    if cw {
        match o {
            1 => 6,
            2 => 7,
            3 => 8,
            4 => 5,
            5 => 2,
            6 => 3,
            7 => 4,
            8 => 1,
            _ => 6,
        }
    } else {
        match o {
            1 => 8,
            2 => 5,
            3 => 6,
            4 => 7,
            5 => 4,
            6 => 1,
            7 => 2,
            8 => 3,
            _ => 8,
        }
    }
}

/// What an XMP sidecar says about a photo, as read back from darktable,
/// Lightroom or digiKam. `None` means the file doesn't say, and the library's
/// value is kept.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct XmpReadResult {
    /// Stars, 0–5. A reject (`xmp:Rating="-1"`) is reported in `rejected`,
    /// never here.
    pub rating: Option<i8>,
    /// `Some(true)` for `xmp:Rating="-1"`, `Some(false)` for any other rating.
    /// (Picks aren't in XMP: there is no standard field.)
    pub rejected: Option<bool>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub orientation: Option<u8>,
    /// Every keyword in `dc:subject` but darktable's internal ones: the
    /// photo's complete tag list (Photon writes all of a photo's tags).
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(name: &str, format: ImageFormat) -> Image {
        Image {
            format: Some(format),
            ..Image::new(PathBuf::from(name), name.to_string(), 0)
        }
    }

    fn names(picked: Vec<&Image>) -> Vec<&str> {
        picked.iter().map(|i| i.filename.as_str()).collect()
    }

    #[test]
    fn delete_mode_picks_versions_of_a_shot() {
        let shot = [
            version("IMG_1.ORF", ImageFormat::RawOrf),
            version("IMG_1.JPG", ImageFormat::Jpeg),
            version("IMG_1_edit.jpg", ImageFormat::Jpeg),
        ];
        let selected = [&shot[1]];
        assert_eq!(names(Versions::AllVersions.pick(&shot, &selected)), ["IMG_1.ORF", "IMG_1.JPG", "IMG_1_edit.jpg"]);
        assert_eq!(names(Versions::RawOnly.pick(&shot, &selected)), ["IMG_1.ORF"]);
        assert_eq!(names(Versions::RasterOnly.pick(&shot, &selected)), ["IMG_1.JPG", "IMG_1_edit.jpg"]);
    }

    #[test]
    fn delete_mode_falls_back_to_the_selection() {
        let shot = [version("IMG_2.JPG", ImageFormat::Jpeg), version("IMG_2_edit.jpg", ImageFormat::Jpeg)];
        assert_eq!(names(Versions::RawOnly.pick(&shot, &[&shot[1]])), ["IMG_2_edit.jpg"]);
    }

    #[test]
    fn sharing_swaps_versions_only_when_needed() {
        let shot = [
            version("IMG_1.ORF", ImageFormat::RawOrf),
            version("IMG_1.JPG", ImageFormat::Jpeg),
            version("IMG_1_edit.jpg", ImageFormat::Jpeg),
        ];
        let raster = Versions::RasterOnly;
        // The JPG alone: not its edit, not the RAW.
        assert_eq!(names(raster.pick_selected(&shot, &[&shot[1]])), ["IMG_1.JPG"]);
        // RAW and JPG both selected (e.g. a whole day): just the JPG.
        assert_eq!(names(raster.pick_selected(&shot, &[&shot[0], &shot[1]])), ["IMG_1.JPG"]);
        // Only the RAW: its raster versions instead.
        assert_eq!(names(raster.pick_selected(&shot, &[&shot[0]])), ["IMG_1.JPG", "IMG_1_edit.jpg"]);
        // A lone RAW is still shared.
        let lone = [version("IMG_3.ORF", ImageFormat::RawOrf)];
        assert_eq!(names(raster.pick_selected(&lone, &[&lone[0]])), ["IMG_3.ORF"]);
    }

    #[test]
    fn old_preferences_without_delete_mode_still_load() {
        let json = r#"{"raster_editor":"gimp","raw_editor":"darktable","viewer":"eog","thumbnail_size":220}"#;
        let prefs: Preferences = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.delete_mode, Versions::AllVersions);
        assert_eq!(prefs.share_versions, Versions::RasterOnly);
    }

    #[test]
    fn orientation_composition_table_covers_all_8x2_cases() {
        // Table of expected results for all 8 EXIF values x rotate +/- 90 (CW and CCW)
        let expected_cw = [
            (1, 6),
            (2, 7),
            (3, 8),
            (4, 5),
            (5, 2),
            (6, 3),
            (7, 4),
            (8, 1),
        ];
        let expected_ccw = [
            (1, 8),
            (2, 5),
            (3, 6),
            (4, 7),
            (5, 4),
            (6, 1),
            (7, 2),
            (8, 3),
        ];

        for (from, to) in expected_cw {
            assert_eq!(rotate_orientation(Some(from), true), to, "CW from {from}");
        }
        for (from, to) in expected_ccw {
            assert_eq!(rotate_orientation(Some(from), false), to, "CCW from {from}");
        }

        // Test four consecutive 90-degree rotations in either direction return to start
        for o in 1..=8 {
            let mut curr_cw = o;
            let mut curr_ccw = o;
            for _ in 0..4 {
                curr_cw = rotate_orientation(Some(curr_cw), true);
                curr_ccw = rotate_orientation(Some(curr_ccw), false);
            }
            assert_eq!(curr_cw, o, "4x CW rotation did not return to {o}");
            assert_eq!(curr_ccw, o, "4x CCW rotation did not return to {o}");
        }

        // Unset / 0 / invalid defaults to 1
        assert_eq!(rotate_orientation(None, true), 6);
        assert_eq!(rotate_orientation(Some(0), true), 6);
        assert_eq!(rotate_orientation(Some(99), true), 6);
        assert_eq!(rotate_orientation(None, false), 8);
        assert_eq!(rotate_orientation(Some(0), false), 8);
        assert_eq!(rotate_orientation(Some(99), false), 8);
    }
}
