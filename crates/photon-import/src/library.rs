//! Placing files into the Photon library (Copy / Move import modes).
//!
//! Files are filed by capture date as `YYYY/MM/DD/`, the same drill-down
//! layout Shotwell uses, so the folder tree mirrors the app's sidebar.

use chrono::DateTime;
use photon_core::models::FolderImportMode;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Decides destination paths sequentially, so that parallel transfers never
/// race for the same name.
pub struct DestinationPlanner {
    root: PathBuf,
    reserved: HashSet<PathBuf>,
}

impl DestinationPlanner {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            reserved: HashSet::new(),
        }
    }

    /// Pick a free path for `src`, filed under its capture date.
    /// Name collisions get a numeric suffix: `IMG_001.jpg`, `IMG_001_1.jpg`, …
    pub fn plan(&mut self, src: &Path, captured_at: Option<i64>) -> io::Result<PathBuf> {
        let folder = captured_at
            .and_then(|ts| DateTime::from_timestamp(ts, 0))
            .map(|dt| dt.format("%Y/%m/%d").to_string())
            .unwrap_or_else(|| "Unsorted".to_string());
        let dir = self.root.join(folder);

        let name = src
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
        let stem = src.file_stem().unwrap_or(name).to_os_string();
        let ext = src.extension().map(|e| e.to_os_string());

        let mut candidate = dir.join(name);
        let mut n = 0u32;
        while self.reserved.contains(&candidate) || candidate.exists() {
            n += 1;
            let mut numbered: OsString = stem.clone();
            numbered.push(format!("_{n}"));
            if let Some(ext) = &ext {
                numbered.push(".");
                numbered.push(ext);
            }
            candidate = dir.join(numbered);
        }

        self.reserved.insert(candidate.clone());
        Ok(candidate)
    }
}

/// The library root: explicit destination, else `~/Pictures/Photon Library`.
pub fn library_root(destination: Option<&Path>) -> PathBuf {
    destination
        .map(Path::to_path_buf)
        .or_else(|| dirs::picture_dir().map(|p| p.join("Photon Library")))
        .unwrap_or_else(|| PathBuf::from("Photon Library"))
}

/// Copy `src` to `dest` while hashing the bytes in the same pass, so a new
/// file is read from the card exactly once. The data lands in a `.photon-part`
/// file that is renamed into place only after the caller [`Staged::commit`]s,
/// letting it drop duplicates it discovers from the hash without ever
/// exposing a partial or unwanted file.
pub fn stage_copy(src: &Path, dest: &Path, durable: bool) -> io::Result<Staged> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let part = partial_path(dest);
    let result = copy_hashing(src, &part, durable);
    match result {
        Ok(hash) => Ok(Staged {
            part,
            dest: dest.to_path_buf(),
            hash,
        }),
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

/// A copied file waiting for a keep/discard decision. Dropping it discards.
#[must_use]
pub struct Staged {
    part: PathBuf,
    dest: PathBuf,
    pub hash: String,
}

impl Staged {
    /// Move the staged file to its final name.
    pub fn commit(self) -> io::Result<PathBuf> {
        fs::rename(&self.part, &self.dest)?;
        Ok(self.dest.clone())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        // No-op after a successful commit (the part file is gone).
        let _ = fs::remove_file(&self.part);
    }
}

fn copy_hashing(src: &Path, dest: &Path, durable: bool) -> io::Result<String> {
    let mut reader = File::open(src)?;
    let mut writer = File::create(dest)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer.write_all(&buf[..n])?;
    }
    if durable {
        writer.sync_all()?;
    }
    // Keep the camera's timestamps on the copy.
    if let Ok(mtime) = fs::metadata(src).and_then(|m| m.modified()) {
        let _ = writer.set_modified(mtime);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Move `src` to `dest`. Same filesystem: an atomic rename. Across
/// filesystems (card → disk): copy, fsync, and only then delete the source,
/// so a crash or yanked card can never lose the only copy.
pub fn move_file(src: &Path, dest: &Path) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            stage_copy(src, dest, true)?.commit()?;
            fs::remove_file(src)
        }
        Err(e) => Err(e),
    }
}

/// Copy or move a small companion file (XMP sidecar) next to its photo.
pub fn transfer_sidecar(mode: FolderImportMode, src: &Path, dest: &Path) -> io::Result<()> {
    match mode {
        FolderImportMode::InPlace => Ok(()),
        FolderImportMode::Copy => stage_copy(src, dest, false)?.commit().map(drop),
        FolderImportMode::Move => move_file(src, dest),
    }
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut partial = dest.as_os_str().to_owned();
    partial.push(".photon-part");
    PathBuf::from(partial)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_by_capture_date_and_avoids_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let mut planner = DestinationPlanner::new(dir.path().to_path_buf());
        let ts = 1_688_463_000; // 2023-07-04

        let a = planner.plan(Path::new("/card/IMG_1.jpg"), Some(ts)).unwrap();
        let b = planner.plan(Path::new("/other/IMG_1.jpg"), Some(ts)).unwrap();
        let c = planner.plan(Path::new("/card/IMG_2.jpg"), None).unwrap();

        assert_eq!(a, dir.path().join("2023/07/04/IMG_1.jpg"));
        assert_eq!(b, dir.path().join("2023/07/04/IMG_1_1.jpg"));
        assert_eq!(c, dir.path().join("Unsorted/IMG_2.jpg"));
    }

    #[test]
    fn avoids_files_already_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("Unsorted")).unwrap();
        fs::write(dir.path().join("Unsorted/A.jpg"), b"x").unwrap();

        let mut planner = DestinationPlanner::new(dir.path().to_path_buf());
        let p = planner.plan(Path::new("/s/A.jpg"), None).unwrap();
        assert_eq!(p, dir.path().join("Unsorted/A_1.jpg"));
    }

    #[test]
    fn staged_copy_hashes_and_only_appears_on_commit() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jpg");
        fs::write(&src, b"data").unwrap();

        let dest = dir.path().join("lib/a/copy.jpg");
        let staged = stage_copy(&src, &dest, false).unwrap();
        assert_eq!(staged.hash, blake3::hash(b"data").to_hex().to_string());
        assert!(!dest.exists(), "not visible before commit");
        staged.commit().unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"data");
        assert!(src.exists());

        // Dropping without commit leaves nothing behind.
        let other = dir.path().join("lib/a/dropped.jpg");
        drop(stage_copy(&src, &other, false).unwrap());
        assert_eq!(fs::read_dir(dir.path().join("lib/a")).unwrap().count(), 1);
    }

    #[test]
    fn move_removes_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jpg");
        fs::write(&src, b"data").unwrap();

        let moved = dir.path().join("lib/b/moved.jpg");
        move_file(&src, &moved).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read(&moved).unwrap(), b"data");
    }
}
