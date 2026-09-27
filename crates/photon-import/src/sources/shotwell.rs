//! Shotwell database import source.
//!
//! Reads Shotwell's `photo.db` (SQLite) and extracts the original file paths.
//! Shotwell stores paths in the `PhotoTable` with columns like `filename` (full path).

use super::ImportSource;
use photon_core::models::ImageFormat;
use rusqlite::Connection;
use std::path::PathBuf;

pub struct ShotwellSource {
    db_path: PathBuf,
    db_path_str: String,
}

impl ShotwellSource {
    pub fn new(db_path: PathBuf) -> Self {
        let db_path_str = db_path.to_string_lossy().to_string();
        Self { db_path, db_path_str }
    }

    /// Also extract Shotwell's event/tag data as hints for Photon tags.
    pub fn extract_tags(&self) -> anyhow::Result<Vec<(PathBuf, Vec<String>)>> {
        let conn = Connection::open(&self.db_path)?;

        let mut stmt = conn.prepare(
            "SELECT p.filename, t.name
             FROM PhotoTable p
             JOIN TagTable t ON (',' || t.photo_id_list || ',') LIKE ('%,' || p.id || ',%')
             ORDER BY p.filename",
        )?;

        let rows = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let tag: String = row.get(1)?;
            Ok((PathBuf::from(path), tag))
        })?;

        // Group tags by path
        let mut map: std::collections::HashMap<PathBuf, Vec<String>> =
            std::collections::HashMap::new();
        for row in rows.flatten() {
            map.entry(row.0).or_default().push(row.1);
        }

        Ok(map.into_iter().collect())
    }
}

impl ImportSource for ShotwellSource {
    fn scan(&self) -> anyhow::Result<Vec<PathBuf>> {
        let conn = Connection::open(&self.db_path)?;

        // Shotwell's PhotoTable stores full paths in `filename`
        let mut stmt = conn.prepare(
            "SELECT filename FROM PhotoTable ORDER BY exposure_time DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            let path_str: String = row.get(0)?;
            Ok(PathBuf::from(path_str))
        })?;

        let supported = ImageFormat::all_extensions();
        let is_supported = |p: &PathBuf| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| supported.contains(&e.to_lowercase().as_str()))
        };

        let mut paths: Vec<PathBuf> = Vec::new();
        for row in rows.flatten() {
            // Only include supported files that still exist on disk
            if !is_supported(&row) {
                continue;
            }
            if row.exists() {
                paths.push(row);
            } else {
                log::warn!("Shotwell references missing file: {}", row.display());
            }
        }

        Ok(paths)
    }

    fn source_type(&self) -> &str {
        "shotwell"
    }

    fn source_description(&self) -> &str {
        &self.db_path_str
    }
}

/// Try to find Shotwell's default database location.
pub fn find_default_db() -> Option<PathBuf> {
    let data_dir = dirs::data_dir()?;
    let shotwell_db = data_dir.join("shotwell").join("data").join("photo.db");
    if shotwell_db.exists() {
        Some(shotwell_db)
    } else {
        None
    }
}
