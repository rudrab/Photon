//! Disk (folder) import source.
//!
//! Recursively walks a directory tree and discovers all image files.
//! Both RAW and JPEG variants of the same shot are imported —
//! grouping into sidecar groups is handled by the engine, not here.

use super::ImportSource;
use photon_core::models::ImageFormat;
use std::path::PathBuf;
use walkdir::WalkDir;

pub struct DiskSource {
    root: PathBuf,
    recursive: bool,
    root_str: String,
}

impl DiskSource {
    pub fn new(root: PathBuf, recursive: bool) -> Self {
        let root_str = root.to_string_lossy().to_string();
        Self {
            root,
            recursive,
            root_str,
        }
    }
}

impl ImportSource for DiskSource {
    fn scan(&self) -> anyhow::Result<Vec<PathBuf>> {
        let extensions = ImageFormat::all_extensions();
        let walker = if self.recursive {
            WalkDir::new(&self.root)
        } else {
            WalkDir::new(&self.root).max_depth(1)
        };

        let mut all_files: Vec<PathBuf> = Vec::new();

        for entry in walker.into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = match path.extension().and_then(|e| e.to_str()) {
                Some(e) => e.to_lowercase(),
                None => continue,
            };
            if extensions.contains(&ext.as_str()) {
                all_files.push(path.to_path_buf());
            }
        }

        // Import everything — RAW files, JPEGs, _modified edits, all of them.
        // The engine will group related files by stem into sidecar groups.
        // Deduplication by blake3 hash prevents true duplicates.

        log::info!(
            "DiskSource scanned {} image files from {}",
            all_files.len(),
            self.root.display()
        );

        Ok(all_files)
    }

    fn source_type(&self) -> &str {
        "folder"
    }

    fn source_description(&self) -> &str {
        &self.root_str
    }
}
