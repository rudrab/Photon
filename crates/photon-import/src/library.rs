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
///
/// The copy is always flushed to disk. With `verify`, it is also read back
/// from the disk (not the page cache) and must match what was read from `src`.
pub fn stage_copy(src: &Path, dest: &Path, verify: bool) -> io::Result<Staged> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let part = partial_path(dest);
    let result = copy_hashing(src, &part).and_then(|hash| {
        if verify {
            verify_copy(&part, &hash)?;
        }
        Ok(hash)
    });
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

fn copy_hashing(src: &Path, dest: &Path) -> io::Result<String> {
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
    // Keep the camera's timestamps on the copy.
    if let Ok(mtime) = fs::metadata(src).and_then(|m| m.modified()) {
        let _ = writer.set_modified(mtime);
    }
    writer.sync_all()?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Check that `path`, as stored on disk, has content hash `expected`.
///
/// A freshly written file would be read back from the page cache, which
/// proves nothing about the disk; its (flushed, so clean) cached pages are
/// dropped first so the read comes from the device.
pub fn verify_copy(path: &Path, expected: &str) -> io::Result<()> {
    let file = File::open(path)?;
    drop_cached_pages(&file);
    let actual = hash_reader(file)?;
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: copy differs from the original (verification failed)", path.display()),
        ))
    }
}

fn hash_reader(mut reader: impl Read) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(target_os = "linux")]
fn drop_cached_pages(file: &File) {
    use std::os::fd::AsRawFd;
    // Advisory: if it is ignored, verification still catches bad reads and
    // writes that reached the cache, just not bad media.
    unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
}

#[cfg(not(target_os = "linux"))]
fn drop_cached_pages(_file: &File) {}

/// How [`move_into_place`] got the file there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    /// Same filesystem: renamed; there is no source any more.
    Renamed,
    /// Copied (and flushed, and verified if asked) across filesystems; the
    /// source is still there, for the caller to remove once it is safe to.
    Copied,
}

/// Put `src` at `dest` for a move. Same filesystem: an atomic rename. Across
/// filesystems (card → disk): a copy, which must have content hash `expected`
/// (when given: what the source read as before) — the source is left for the
/// caller to delete, so a crash, yanked card or bad read can never lose the
/// only copy.
pub fn move_into_place(src: &Path, dest: &Path, expected: Option<&str>, verify: bool) -> io::Result<Moved> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(src, dest) {
        Ok(()) => Ok(Moved::Renamed),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            let staged = stage_copy(src, dest, verify)?;
            if expected.is_some_and(|h| h != staged.hash) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: read differently twice; the card may be failing", src.display()),
                ));
            }
            staged.commit()?;
            Ok(Moved::Copied)
        }
        Err(e) => Err(e),
    }
}

/// Move `src` to `dest`: [`move_into_place`], then delete the source.
pub fn move_file(src: &Path, dest: &Path, verify: bool) -> io::Result<()> {
    if move_into_place(src, dest, None, verify)? == Moved::Copied {
        fs::remove_file(src)?;
    }
    Ok(())
}

/// Copy or move a small companion file (XMP sidecar) next to its photo.
pub fn transfer_sidecar(mode: FolderImportMode, src: &Path, dest: &Path) -> io::Result<()> {
    match mode {
        FolderImportMode::InPlace => Ok(()),
        FolderImportMode::Copy => stage_copy(src, dest, true)?.commit().map(drop),
        FolderImportMode::Move => move_file(src, dest, true),
    }
}

/// Copy library file `file` (content hash `hash`) to the same place under
/// `backup_root` as it has under `library_root`: the second copy that makes
/// it safe to format the card. A backup already there with the same content
/// counts; one with different content is an error, never overwritten.
pub fn back_up(file: &Path, hash: &str, library_root: &Path, backup_root: &Path) -> io::Result<PathBuf> {
    let relative = file.strip_prefix(library_root).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{} is outside the library", file.display()))
    })?;
    let dest = backup_root.join(relative);
    if dest.exists() {
        return match verify_copy(&dest, hash) {
            Ok(()) => Ok(dest),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{}: a different file is already there", dest.display()),
            )),
        };
    }
    let staged = stage_copy(file, &dest, true)?;
    if staged.hash != hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: library copy no longer matches its hash", file.display()),
        ));
    }
    staged.commit()
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut partial = dest.as_os_str().to_owned();
    partial.push(".photon-part");
    PathBuf::from(partial)
}

/// Scans `search_dir` recursively to relink any images in `missing`.
/// A candidate matches if:
/// 1. Its filename matches (case-insensitively).
/// 2. Its file size matches `image.size_bytes`.
/// 3. Its BLAKE3 content hash matches `image.hash`.
/// Returns the number of successfully relinked images.
pub fn relink_missing_folder(
    conn: &rusqlite::Connection,
    missing: &[photon_core::models::Image],
    search_dir: &Path,
) -> io::Result<usize> {
    if !search_dir.is_dir() {
        return Ok(0);
    }

    use std::collections::HashMap;
    let mut by_name: HashMap<String, Vec<&photon_core::models::Image>> = HashMap::new();
    for img in missing {
        if let Some(file_name) = img.path.file_name().and_then(|n| n.to_str()) {
            by_name.entry(file_name.to_lowercase()).or_default().push(img);
        }
    }

    let mut relinked = 0;
    for entry in walkdir::WalkDir::new(search_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().to_lowercase();
        if let Some(candidates) = by_name.get_mut(&file_name) {
            let Ok(meta) = entry.metadata() else { continue };
            let size = meta.len() as i64;
            let path = entry.path();

            let mut matched_idx = None;
            for (idx, img) in candidates.iter().enumerate() {
                if img.size_bytes == size {
                    if let Ok(hash) = crate::dedup::blake3_hash_file(path) {
                        if hash == img.hash {
                            if let Some(id) = img.id {
                                if photon_core::db::queries::relink_image(conn, id, &path.to_string_lossy()).is_ok() {
                                    relinked += 1;
                                    matched_idx = Some(idx);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            if let Some(idx) = matched_idx {
                candidates.remove(idx);
            }
        }
    }

    Ok(relinked)
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
        let staged = stage_copy(&src, &dest, true).unwrap();
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
        move_file(&src, &moved, true).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read(&moved).unwrap(), b"data");
    }

    #[test]
    fn verification_catches_a_bad_copy() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.jpg");
        fs::write(&file, b"data").unwrap();
        let good = blake3::hash(b"data").to_hex().to_string();
        verify_copy(&file, &good).unwrap();

        fs::write(&file, b"dat4").unwrap();
        let err = verify_copy(&file, &good).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn backup_mirrors_the_library_layout_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let (lib, backup) = (dir.path().join("lib"), dir.path().join("backup"));
        let file = lib.join("2024/05/01/IMG_1.ORF");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"raw").unwrap();
        let hash = blake3::hash(b"raw").to_hex().to_string();

        let copy = back_up(&file, &hash, &lib, &backup).unwrap();
        assert_eq!(copy, backup.join("2024/05/01/IMG_1.ORF"));
        assert_eq!(fs::read(&copy).unwrap(), b"raw");
        // Again: already backed up.
        assert_eq!(back_up(&file, &hash, &lib, &backup).unwrap(), copy);

        // A different file in the way is left alone.
        fs::write(&copy, b"other").unwrap();
        assert!(back_up(&file, &hash, &lib, &backup).is_err());
        assert_eq!(fs::read(&copy).unwrap(), b"other");

        // A library file that changed since import is not backed up as if it hadn't.
        fs::remove_file(&copy).unwrap();
        fs::write(&file, b"changed").unwrap();
        assert!(back_up(&file, &hash, &lib, &backup).is_err());
        assert!(!copy.exists());
    }

    #[test]
    fn relink_missing_folder_matches_by_name_size_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("external_drive/photos");
        fs::create_dir_all(&target_dir).unwrap();

        let real_file = target_dir.join("vacation_001.jpg");
        fs::write(&real_file, b"photo content data").unwrap();
        let hash = blake3::hash(b"photo content data").to_hex().to_string();
        let size = b"photo content data".len() as i64;

        let db = photon_core::db::Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        let mut img = photon_core::models::Image::new(PathBuf::from("/lost/path/vacation_001.jpg"), hash.clone(), size);
        let id = {
            let tx = conn.transaction().unwrap();
            let id = photon_core::db::queries::insert_image(&tx, &img).unwrap().unwrap();
            tx.commit().unwrap();
            id
        };
        img.id = Some(id);
        img.missing = true;
        photon_core::db::queries::mark_missing(&conn, &[id], true).unwrap();

        // Relink scanning target_dir
        let count = relink_missing_folder(&conn, &[img], dir.path()).unwrap();
        assert_eq!(count, 1);

        let refreshed = photon_core::db::queries::get_image(&conn, id).unwrap().unwrap();
        assert_eq!(refreshed.path, real_file);
        assert_eq!(refreshed.missing, false);
    }
}
