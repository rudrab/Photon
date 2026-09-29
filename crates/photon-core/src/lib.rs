//! photon-core: Domain logic, database layer, and models for Photon.
//!
//! This crate is library-grade — usable by CLI tools, scripts, and GUIs.
//! It owns the database schema, all query logic, and core domain types.

pub mod db;
pub mod error;
pub mod models;

pub use db::Database;
pub use error::PhotonError;
pub use models::{DesktopApp, EditRecord, Image, ImageFormat, LibraryQuery, Preferences, Tag};

/// Check if a path appears to be on a mount that is currently disconnected/offline
/// (e.g. under /run/media, /media, /mnt whose parent mountpoint does not exist).
pub fn is_path_offline(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    if s.starts_with("/run/media/") || s.starts_with("/media/") || s.starts_with("/mnt/") {
        let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
        if s.starts_with("/run/media/") && parts.len() >= 3 {
            let mount = std::path::PathBuf::from(format!("/{}/{}/{}", parts[0], parts[1], parts[2]));
            return !mount.exists();
        } else if (s.starts_with("/media/") || s.starts_with("/mnt/")) && parts.len() >= 2 {
            let mount = std::path::PathBuf::from(format!("/{}/{}", parts[0], parts[1]));
            return !mount.exists();
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detects_offline_mount_paths() {
        assert!(is_path_offline(Path::new("/run/media/user/nonexistent_drive/DCIM/pic.jpg")));
        assert!(!is_path_offline(Path::new("/home/user/Pictures/pic.jpg")));
    }
}

