//! digiKam database import source (stub).
//!
//! digiKam uses a SQLite database (`digikam4.db`) with tables like `Images`,
//! `Albums`, `Tags`, `ImageTags`. This will be fully implemented in Phase 2c.

use super::ImportSource;
use std::path::PathBuf;

#[allow(dead_code)]
pub struct DigikamSource {
    db_path: PathBuf,
    db_path_str: String,
}

impl DigikamSource {
    pub fn new(db_path: PathBuf) -> Self {
        let db_path_str = db_path.to_string_lossy().to_string();
        Self { db_path, db_path_str }
    }
}

impl ImportSource for DigikamSource {
    fn scan(&self) -> anyhow::Result<Vec<PathBuf>> {
        // digiKam stores relative paths in `Images.name` under `Albums.relativePath`
        // combined with the album root from `AlbumRoots.specificPath`.
        //
        // Full implementation coming in Phase 2c.
        anyhow::bail!(
            "digiKam import is not yet implemented. \
             Use Folder Import to import your digiKam library by pointing \
             at your photo directories directly."
        )
    }

    fn source_type(&self) -> &str {
        "digikam"
    }

    fn source_description(&self) -> &str {
        &self.db_path_str
    }
}

/// Try to find digiKam's default database location.
pub fn find_default_db() -> Option<PathBuf> {
    let data_dir = dirs::data_dir()?;
    let dk_db = data_dir.join("digikam").join("digikam4.db");
    if dk_db.exists() {
        Some(dk_db)
    } else {
        None
    }
}
