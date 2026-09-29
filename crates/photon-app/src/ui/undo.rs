//! In-memory undo/redo stack for culling, tagging, rotation, and trash.
//!
//! Tracks reversible user operations, persists them in memory, and on undo/redo
//! re-syncs SQLite records and XMP sidecars.

use gtk4::gio;
use gtk4::gio::prelude::FileExt;
use photon_core::db::queries;
use photon_core::db::Database;
use photon_core::error::PhotonError;
use photon_core::models::Image;
use photon_import::{write_image_xmp, Keywords, XmpUpdate};
use std::path::{Path, PathBuf};

const MAX_UNDO_STACK: usize = 50;

#[derive(Debug, Clone)]
pub enum UndoAction {
    Rating {
        /// (image_id, old_rating)
        previous: Vec<(i64, i32)>,
        new_rating: i32,
    },
    Flag {
        /// (image_id, old_flag)
        previous: Vec<(i64, i32)>,
        new_flag: i32,
    },
    Orientation {
        /// (image_id, old_orientation, image_hash)
        previous: Vec<(i64, Option<u16>, String)>,
        cw: bool,
    },
    TagAdd {
        image_ids: Vec<i64>,
        tag_id: i64,
        tag_name: String,
    },
    TagRemove {
        image_ids: Vec<i64>,
        tag_id: i64,
        tag_name: String,
    },
    Trash {
        images: Vec<Image>,
        /// (image_id, tag_id)
        tags: Vec<(i64, i64)>,
        /// (image_path, xmp_path)
        trashed_paths: Vec<(PathBuf, Option<PathBuf>)>,
    },
}

#[derive(Default)]
pub struct UndoManager {
    undo_stack: Vec<UndoAction>,
    redo_stack: Vec<UndoAction>,
}

impl UndoManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, action: UndoAction) {
        self.redo_stack.clear();
        self.undo_stack.push(action);
        if self.undo_stack.len() > MAX_UNDO_STACK {
            self.undo_stack.remove(0);
        }
    }

    #[allow(dead_code)]
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    #[allow(dead_code)]
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self, db: &Database, cache_dir: &Path) -> Result<Option<String>, PhotonError> {
        let Some(action) = self.undo_stack.pop() else {
            return Ok(None);
        };

        let result = (|| -> Result<String, PhotonError> {
            match &action {
                UndoAction::Rating { previous, new_rating: _ } => {
                    let mut conn = db.conn()?;
                    for &(id, old_r) in previous {
                        queries::set_rating(&mut conn, id, old_r)?;
                        sync_cull_xmp(&conn, id);
                    }
                    let n = previous.len();
                    Ok(format!("Undid: Rating change ({} photo{})", n, if n == 1 { "" } else { "s" }))
                }
                UndoAction::Flag { previous, new_flag: _ } => {
                    let mut conn = db.conn()?;
                    for &(id, old_f) in previous {
                        queries::set_flag(&mut conn, id, old_f)?;
                        sync_cull_xmp(&conn, id);
                    }
                    let n = previous.len();
                    Ok(format!("Undid: Flag change ({} photo{})", n, if n == 1 { "" } else { "s" }))
                }
                UndoAction::Orientation { previous, cw: _ } => {
                    let mut conn = db.conn()?;
                    for &(id, old_o, ref hash) in previous {
                        let orient = old_o.unwrap_or(1);
                        queries::set_orientation(&mut conn, id, orient)?;
                        sync_orientation_xmp(&conn, id, orient);
                        photon_import::thumbnails::invalidate_cache(cache_dir, hash);
                        crate::ui::detail::invalidate_full_res(hash);
                    }
                    let n = previous.len();
                    Ok(format!("Undid: Rotation ({} photo{})", n, if n == 1 { "" } else { "s" }))
                }
                UndoAction::TagAdd { image_ids, tag_id, tag_name } => {
                    let conn = db.conn()?;
                    for &id in image_ids {
                        queries::untag_image(&conn, id, *tag_id)?;
                        sync_tags_xmp(&conn, id);
                    }
                    let n = image_ids.len();
                    Ok(format!("Undid: Added tag #{} ({} photo{})", tag_name, n, if n == 1 { "" } else { "s" }))
                }
                UndoAction::TagRemove { image_ids, tag_id, tag_name } => {
                    let conn = db.conn()?;
                    for &id in image_ids {
                        queries::tag_image(&conn, id, *tag_id)?;
                        sync_tags_xmp(&conn, id);
                    }
                    let n = image_ids.len();
                    Ok(format!("Undid: Removed tag #{} ({} photo{})", tag_name, n, if n == 1 { "" } else { "s" }))
                }
                UndoAction::Trash { images, tags, trashed_paths } => {
                    // Restore files from system trash. A row is only re-inserted
                    // when its file is back, so the library never points at a
                    // photo that is still in the trash.
                    let mut failed = Vec::new();
                    let mut restored_ids = Vec::new();
                    for (img, (p, xmp)) in images.iter().zip(trashed_paths.iter()) {
                        if let Err(e) = restore_trashed_file(p) {
                            log::warn!("Could not restore {}: {e}", p.display());
                            failed.push(p.display().to_string());
                            continue;
                        }
                        if let Some(xmp_path) = xmp {
                            if let Err(e) = restore_trashed_file(xmp_path) {
                                log::warn!("Could not restore XMP {}: {e}", xmp_path.display());
                            }
                        }
                        if let Some(id) = img.id {
                            restored_ids.push(id);
                        }
                    }

                    // Re-insert into database preserving original IDs
                    let mut conn = db.conn()?;
                    let tx = conn.transaction()?;
                    for img in images.iter().filter(|i| i.id.is_some_and(|id| restored_ids.contains(&id))) {
                        queries::restore_image(&tx, img)?;
                    }
                    for &(img_id, tag_id) in tags.iter().filter(|(img_id, _)| restored_ids.contains(img_id)) {
                        tx.execute(
                            "INSERT OR IGNORE INTO image_tags (image_id, tag_id) VALUES (?1, ?2)",
                            [img_id, tag_id],
                        )?;
                    }
                    tx.commit()?;

                    // Restoring is idempotent (files already back are skipped,
                    // rows use INSERT OR IGNORE), so keeping the action on the
                    // undo stack lets the user retry once the trash is fixed.
                    if !failed.is_empty() {
                        return Err(PhotonError::Other(format!(
                            "Could not restore {} of {} photos from the trash: {}",
                            failed.len(),
                            images.len(),
                            failed.join(", ")
                        )));
                    }
                    let n = images.len();
                    Ok(format!("Undid: Move to Trash ({} photo{})", n, if n == 1 { "" } else { "s" }))
                }
            }
        })();

        match result {
            Ok(description) => {
                self.redo_stack.push(action);
                Ok(Some(description))
            }
            Err(e) => {
                self.undo_stack.push(action);
                Err(e)
            }
        }
    }

    pub fn redo(&mut self, db: &Database, cache_dir: &Path) -> Result<Option<String>, PhotonError> {
        let Some(action) = self.redo_stack.pop() else {
            return Ok(None);
        };

        let result = (|| -> Result<String, PhotonError> {
            match &action {
                UndoAction::Rating { previous, new_rating } => {
                    let ids: Vec<i64> = previous.iter().map(|&(id, _)| id).collect();
                    let mut conn = db.conn()?;
                    queries::batch_set_rating(&mut conn, &ids, *new_rating)?;
                    for &id in &ids {
                        sync_cull_xmp(&conn, id);
                    }
                    let n = ids.len();
                    Ok(format!("Redid: Rating {}★ ({} photo{})", new_rating, n, if n == 1 { "" } else { "s" }))
                }
                UndoAction::Flag { previous, new_flag } => {
                    let ids: Vec<i64> = previous.iter().map(|&(id, _)| id).collect();
                    let mut conn = db.conn()?;
                    queries::batch_set_flag(&mut conn, &ids, *new_flag)?;
                    for &id in &ids {
                        sync_cull_xmp(&conn, id);
                    }
                    let n = ids.len();
                    let flag_name = match *new_flag {
                        1 => "Pick",
                        -1 => "Reject",
                        _ => "Unflag",
                    };
                    Ok(format!("Redid: Flag {} ({} photo{})", flag_name, n, if n == 1 { "" } else { "s" }))
                }
                UndoAction::Orientation { previous, cw } => {
                    let mut conn = db.conn()?;
                    for &(id, old_o, ref hash) in previous {
                        let next_o = photon_core::models::rotate_orientation(old_o, *cw);
                        queries::set_orientation(&mut conn, id, next_o)?;
                        sync_orientation_xmp(&conn, id, next_o);
                        photon_import::thumbnails::invalidate_cache(cache_dir, hash);
                        crate::ui::detail::invalidate_full_res(hash);
                    }
                    let n = previous.len();
                    Ok(format!("Redid: Rotation ({} photo{})", n, if n == 1 { "" } else { "s" }))
                }
                UndoAction::TagAdd { image_ids, tag_id, tag_name } => {
                    let conn = db.conn()?;
                    for &id in image_ids {
                        queries::tag_image(&conn, id, *tag_id)?;
                        sync_tags_xmp(&conn, id);
                    }
                    let n = image_ids.len();
                    Ok(format!("Redid: Added tag #{} ({} photo{})", tag_name, n, if n == 1 { "" } else { "s" }))
                }
                UndoAction::TagRemove { image_ids, tag_id, tag_name } => {
                    let conn = db.conn()?;
                    for &id in image_ids {
                        queries::untag_image(&conn, id, *tag_id)?;
                        sync_tags_xmp(&conn, id);
                    }
                    let n = image_ids.len();
                    Ok(format!("Redid: Removed tag #{} ({} photo{})", tag_name, n, if n == 1 { "" } else { "s" }))
                }
                UndoAction::Trash { images, tags: _, trashed_paths } => {
                    // Re-trash files. A row is only deleted when its file went
                    // to the trash, so a failure never hides a photo that is
                    // still on disk.
                    let mut ids = Vec::new();
                    let mut failed = Vec::new();
                    for (img, (p, xmp)) in images.iter().zip(trashed_paths.iter()) {
                        // Already gone: an earlier, partly failed redo trashed it.
                        if !p.exists() {
                            if let Some(id) = img.id {
                                ids.push(id);
                            }
                            continue;
                        }
                        if let Err(e) = trash_file(p) {
                            log::warn!("Could not move {} to the trash: {e}", p.display());
                            failed.push(p.display().to_string());
                            continue;
                        }
                        if let Some(xmp_path) = xmp {
                            if xmp_path.exists() {
                                if let Err(e) = trash_file(xmp_path) {
                                    log::warn!("Could not move XMP {} to the trash: {e}", xmp_path.display());
                                }
                            }
                        }
                        if let Some(id) = img.id {
                            ids.push(id);
                        }
                    }
                    let mut conn = db.conn()?;
                    queries::delete_images(&mut conn, &ids)?;
                    if !failed.is_empty() {
                        return Err(PhotonError::Other(format!(
                            "Could not move {} of {} photos to the trash: {}",
                            failed.len(),
                            images.len(),
                            failed.join(", ")
                        )));
                    }
                    let n = images.len();
                    Ok(format!("Redid: Move to Trash ({} photo{})", n, if n == 1 { "" } else { "s" }))
                }
            }
        })();

        match result {
            Ok(description) => {
                self.undo_stack.push(action);
                Ok(Some(description))
            }
            Err(e) => {
                self.redo_stack.push(action);
                Err(e)
            }
        }
    }

}

fn sync_cull_xmp(conn: &rusqlite::Connection, id: i64) {
    if let Ok(Some(img)) = queries::get_image(conn, id) {
        let update = XmpUpdate {
            rating: Some(img.rating),
            rejected: Some(img.flagged == -1),
            ..Default::default()
        };
        log_xmp_error(&img.path, write_image_xmp(conn, id, &img.path, &update));
    }
}

fn log_xmp_error<T, E: std::fmt::Display>(path: &Path, result: Result<T, E>) {
    if let Err(e) = result {
        log::warn!("Failed to update XMP sidecar for {}: {e}", path.display());
    }
}

fn sync_orientation_xmp(conn: &rusqlite::Connection, id: i64, orient: u16) {
    if let Ok(Some(img)) = queries::get_image(conn, id) {
        let update = XmpUpdate {
            orientation: Some(orient),
            ..Default::default()
        };
        log_xmp_error(&img.path, write_image_xmp(conn, id, &img.path, &update));
    }
}

fn sync_tags_xmp(conn: &rusqlite::Connection, id: i64) {
    if let Ok(Some(img)) = queries::get_image(conn, id) {
        if let (Ok(img_tags), Ok(all_tags)) = (
            queries::get_tags_for_image(conn, id),
            queries::get_all_tags(conn),
        ) {
            let names: Vec<String> = img_tags.into_iter().map(|t| t.name).collect();
            let all_names: Vec<String> = all_tags.into_iter().map(|t| t.name).collect();
            let update = XmpUpdate {
                keywords: Some(Keywords {
                    tags: &names,
                    known: &all_names,
                }),
                ..Default::default()
            };
            log_xmp_error(&img.path, write_image_xmp(conn, id, &img.path, &update));
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_TRASH_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn get_trash_dirs() -> Vec<PathBuf> {
    let mut trash_dirs = Vec::new();
    #[cfg(test)]
    TEST_TRASH_DIR.with(|t| {
        if let Some(ref p) = *t.borrow() {
            trash_dirs.push(p.clone());
        }
    });
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg).join("Trash");
        if !trash_dirs.contains(&p) {
            trash_dirs.push(p);
        }
    }
    if let Some(data) = dirs::data_dir() {
        let p = data.join("Trash");
        if !trash_dirs.contains(&p) {
            trash_dirs.push(p);
        }
    }
    if let Some(home) = dirs::home_dir() {
        let p = home.join(".local/share/Trash");
        if !trash_dirs.contains(&p) {
            trash_dirs.push(p);
        }
    }
    trash_dirs
}

/// Move a file to the FreeDesktop trash. Tests redirect this to
/// `TEST_TRASH_DIR` so they never touch the user's real trash.
fn trash_file(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if let Some(trash) = TEST_TRASH_DIR.with(|t| t.borrow().clone()) {
        let name = path.file_name().ok_or("no file name")?;
        std::fs::rename(path, trash.join("files").join(name)).map_err(|e| e.to_string())?;
        let info = trash.join("info").join(format!("{}.trashinfo", name.to_string_lossy()));
        return std::fs::write(info, format!("[Trash Info]\nPath={}\n", path.display())).map_err(|e| e.to_string());
    }
    gio::File::for_path(path).trash(gio::Cancellable::NONE).map_err(|e| e.to_string())
}

fn url_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.as_bytes().iter().copied();
    while let Some(b) = chars.next() {
        if b == b'%' {
            if let (Some(h1), Some(h2)) = (chars.next(), chars.next()) {
                if let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&[h1, h2]).unwrap_or(""), 16) {
                    bytes.push(byte);
                    continue;
                }
                bytes.push(b'%');
                bytes.push(h1);
                bytes.push(h2);
            } else {
                bytes.push(b'%');
            }
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Find and restore a file that was sent to the FreeDesktop trash.
pub(crate) fn restore_trashed_file(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }

    let orig_str = path.to_string_lossy();
    let trash_dirs = get_trash_dirs();

    for trash in trash_dirs {
        let info_dir = trash.join("info");
        let files_dir = trash.join("files");
        if !info_dir.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&info_dir) {
            for entry in entries.flatten() {
                let info_path = entry.path();
                if info_path.extension().and_then(|e| e.to_str()) == Some("trashinfo") {
                    if let Ok(content) = std::fs::read_to_string(&info_path) {
                        for line in content.lines() {
                            let line = line.trim();
                            if let Some(target) = line.strip_prefix("Path=") {
                                let decoded = url_decode(target);
                                if decoded == orig_str || target == orig_str {
                                    if let Some(stem) = info_path.file_stem() {
                                        let trashed_file = files_dir.join(stem);
                                        if trashed_file.exists() {
                                            if let Some(parent) = path.parent() {
                                                std::fs::create_dir_all(parent).map_err(|e| {
                                                    format!("Failed to recreate {}: {e}", parent.display())
                                                })?;
                                            }
                                            std::fs::rename(&trashed_file, path).map_err(|e| {
                                                format!("Failed to move {}: {e}", path.display())
                                            })?;
                                            // The file is back; a stale .trashinfo only
                                            // leaves an empty entry in the trash view.
                                            if let Err(e) = std::fs::remove_file(&info_path) {
                                                log::warn!("Could not remove {}: {e}", info_path.display());
                                            }
                                            return Ok(());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Err(format!("Could not find {} in system trash", path.display()))
}


#[cfg(test)]
mod tests {
    use super::*;
    use photon_core::models::Image;
    use std::path::PathBuf;

    fn make_test_db() -> (Database, PathBuf) {
        let db = Database::open_in_memory().unwrap();
        let cache_dir = std::env::temp_dir().join("photon_undo_test_cache");
        let _ = std::fs::create_dir_all(&cache_dir);
        (db, cache_dir)
    }

    fn insert_sample_image(conn: &mut rusqlite::Connection) -> i64 {
        let tx = conn.transaction().unwrap();
        let img = Image {
            filename: "photo.jpg".into(),
            path: std::path::PathBuf::from("/photos/photo.jpg"),
            hash: "hash123".into(),
            rating: 0,
            flagged: 0,
            orientation: Some(1),
            ..Default::default()
        };
        let id = queries::insert_image(&tx, &img).unwrap().unwrap();
        tx.commit().unwrap();
        id
    }

    #[test]
    fn undo_manager_stack_cap() {
        let mut um = UndoManager::new();
        assert!(!um.can_undo());
        assert!(!um.can_redo());

        for i in 0..60 {
            um.push(UndoAction::Rating {
                previous: vec![(1, i)],
                new_rating: i + 1,
            });
        }
        assert!(um.can_undo());
        assert_eq!(um.undo_stack.len(), MAX_UNDO_STACK);
    }

    #[test]
    fn undo_and_redo_rating() {
        let (db, cache_dir) = make_test_db();
        let id = {
            let mut conn = db.conn().unwrap();
            insert_sample_image(&mut conn)
        };

        let mut um = UndoManager::new();
        {
            let conn = db.conn().unwrap();
            queries::set_rating(&conn, id, 4).unwrap();
        }
        um.push(UndoAction::Rating {
            previous: vec![(id, 0)],
            new_rating: 4,
        });

        {
            let conn = db.conn().unwrap();
            assert_eq!(queries::get_image(&conn, id).unwrap().unwrap().rating, 4);
        }

        // Undo -> back to 0
        let desc = um.undo(&db, &cache_dir).unwrap().unwrap();
        assert!(desc.contains("Undid"));
        {
            let conn = db.conn().unwrap();
            assert_eq!(queries::get_image(&conn, id).unwrap().unwrap().rating, 0);
        }

        // Redo -> forward to 4
        let desc = um.redo(&db, &cache_dir).unwrap().unwrap();
        assert!(desc.contains("Redid"));
        {
            let conn = db.conn().unwrap();
            assert_eq!(queries::get_image(&conn, id).unwrap().unwrap().rating, 4);
        }
    }

    #[test]
    fn undo_and_redo_flag() {
        let (db, cache_dir) = make_test_db();
        let id = {
            let mut conn = db.conn().unwrap();
            insert_sample_image(&mut conn)
        };

        let mut um = UndoManager::new();
        {
            let conn = db.conn().unwrap();
            queries::set_flag(&conn, id, 1).unwrap();
        }
        um.push(UndoAction::Flag {
            previous: vec![(id, 0)],
            new_flag: 1,
        });

        {
            let conn = db.conn().unwrap();
            assert_eq!(queries::get_image(&conn, id).unwrap().unwrap().flagged, 1);
        }

        um.undo(&db, &cache_dir).unwrap();
        {
            let conn = db.conn().unwrap();
            assert_eq!(queries::get_image(&conn, id).unwrap().unwrap().flagged, 0);
        }

        um.redo(&db, &cache_dir).unwrap();
        {
            let conn = db.conn().unwrap();
            assert_eq!(queries::get_image(&conn, id).unwrap().unwrap().flagged, 1);
        }
    }

    #[test]
    fn undo_and_redo_orientation() {
        let (db, cache_dir) = make_test_db();
        let id = {
            let mut conn = db.conn().unwrap();
            insert_sample_image(&mut conn)
        };

        let mut um = UndoManager::new();
        {
            let conn = db.conn().unwrap();
            queries::set_orientation(&conn, id, 6).unwrap();
        }
        um.push(UndoAction::Orientation {
            previous: vec![(id, Some(1), "hash123".into())],
            cw: true,
        });

        {
            let conn = db.conn().unwrap();
            assert_eq!(queries::get_image(&conn, id).unwrap().unwrap().orientation, Some(6));
        }

        um.undo(&db, &cache_dir).unwrap();
        {
            let conn = db.conn().unwrap();
            assert_eq!(queries::get_image(&conn, id).unwrap().unwrap().orientation, Some(1));
        }

        um.redo(&db, &cache_dir).unwrap();
        {
            let conn = db.conn().unwrap();
            assert_eq!(queries::get_image(&conn, id).unwrap().unwrap().orientation, Some(6));
        }
    }

    #[test]
    fn undo_and_redo_tags() {
        let (db, cache_dir) = make_test_db();
        let (id, tag_id) = {
            let mut conn = db.conn().unwrap();
            let id = insert_sample_image(&mut conn);
            let tag_id = queries::ensure_tag(&conn, "vacation").unwrap();
            (id, tag_id)
        };

        let mut um = UndoManager::new();
        {
            let conn = db.conn().unwrap();
            queries::tag_image(&conn, id, tag_id).unwrap();
        }
        um.push(UndoAction::TagAdd {
            image_ids: vec![id],
            tag_id,
            tag_name: "vacation".into(),
        });

        {
            let conn = db.conn().unwrap();
            let tags = queries::get_tags_for_image(&conn, id).unwrap();
            assert_eq!(tags.len(), 1);
        }

        um.undo(&db, &cache_dir).unwrap();
        {
            let conn = db.conn().unwrap();
            let tags = queries::get_tags_for_image(&conn, id).unwrap();
            assert_eq!(tags.len(), 0);
        }

        um.redo(&db, &cache_dir).unwrap();
        {
            let conn = db.conn().unwrap();
            let tags = queries::get_tags_for_image(&conn, id).unwrap();
            assert_eq!(tags.len(), 1);
        }
    }

    #[test]
    fn undo_and_redo_trash() {
        let (db, cache_dir) = make_test_db();
        let temp_dir_path = std::env::temp_dir().join(format!("photon_trash_test_{}", uuid::Uuid::new_v4().simple()));
        let photos_dir = temp_dir_path.join("photos");
        let trash_dir = temp_dir_path.join("Trash");
        let trash_info = trash_dir.join("info");
        let trash_files = trash_dir.join("files");
        std::fs::create_dir_all(&photos_dir).unwrap();
        std::fs::create_dir_all(&trash_info).unwrap();
        std::fs::create_dir_all(&trash_files).unwrap();


        // Set thread-local test trash dir
        TEST_TRASH_DIR.with(|t| *t.borrow_mut() = Some(trash_dir.clone()));

        let photo_path = photos_dir.join("test_shot.jpg");
        let xmp_path = photos_dir.join("test_shot.jpg.xmp");
        std::fs::write(&photo_path, b"test image content").unwrap();
        std::fs::write(&xmp_path, b"<xmp>test sidecar</xmp>").unwrap();

        let tag_id = {
            let conn = db.conn().unwrap();
            queries::ensure_tag(&conn, "landscape").unwrap()
        };

        let mut img = Image {
            filename: "test_shot.jpg".into(),
            path: photo_path.clone(),
            hash: "trash_hash_456".into(),
            rating: 3,
            flagged: 1,
            orientation: Some(1),
            ..Default::default()
        };
        let id = {
            let mut conn = db.conn().unwrap();
            let tx = conn.transaction().unwrap();
            let id = queries::insert_image(&tx, &img).unwrap().unwrap();
            tx.execute(
                "INSERT INTO image_tags (image_id, tag_id) VALUES (?1, ?2)",
                [id, tag_id],
            )
            .unwrap();
            tx.commit().unwrap();
            id
        };
        img.id = Some(id);

        // Simulate moving files to FreeDesktop trash
        let trashed_file = trash_files.join("test_shot.jpg");
        let trashed_xmp = trash_files.join("test_shot.jpg.xmp");
        std::fs::rename(&photo_path, &trashed_file).unwrap();
        std::fs::rename(&xmp_path, &trashed_xmp).unwrap();

        let info_content = format!(
            "[Trash Info]\nPath={}\nDeletionDate=2026-09-28T12:00:00\n",
            photo_path.display()
        );
        std::fs::write(trash_info.join("test_shot.jpg.trashinfo"), info_content).unwrap();

        let xmp_info_content = format!(
            "[Trash Info]\nPath={}\nDeletionDate=2026-09-28T12:00:00\n",
            xmp_path.display()
        );
        std::fs::write(trash_info.join("test_shot.jpg.xmp.trashinfo"), xmp_info_content).unwrap();

        // Delete from DB (simulating trash action)
        {
            let mut conn = db.conn().unwrap();
            queries::delete_images(&mut conn, &[id]).unwrap();
        }

        assert!(!photo_path.exists());
        assert!(!xmp_path.exists());
        {
            let conn = db.conn().unwrap();
            assert!(queries::get_image(&conn, id).unwrap().is_none());
            assert!(queries::get_tags_for_image(&conn, id).unwrap().is_empty());
        }

        // Push trash action to UndoManager
        let mut um = UndoManager::new();
        um.push(UndoAction::Trash {
            images: vec![img.clone()],
            tags: vec![(id, tag_id)],
            trashed_paths: vec![(photo_path.clone(), Some(xmp_path.clone()))],
        });

        // 1. Undo -> files restored, row and tags re-inserted with original id
        let desc = um.undo(&db, &cache_dir).unwrap().unwrap();
        assert!(desc.contains("Undid: Move to Trash"));

        assert!(photo_path.exists());
        assert!(xmp_path.exists());
        assert_eq!(std::fs::read(&photo_path).unwrap(), b"test image content");
        assert_eq!(std::fs::read(&xmp_path).unwrap(), b"<xmp>test sidecar</xmp>");

        // Verify .trashinfo cleaned up
        assert!(!trash_info.join("test_shot.jpg.trashinfo").exists());
        assert!(!trash_info.join("test_shot.jpg.xmp.trashinfo").exists());

        {
            let conn = db.conn().unwrap();
            let restored = queries::get_image(&conn, id).unwrap().expect("image must be restored");
            assert_eq!(restored.id, Some(id));
            assert_eq!(restored.rating, 3);
            assert_eq!(restored.flagged, 1);
            let tags = queries::get_tags_for_image(&conn, id).unwrap();
            assert_eq!(tags.len(), 1);
            assert_eq!(tags[0].name, "landscape");
        }

        // 2. Redo -> moves to trash again and deletes from DB
        let desc = um.redo(&db, &cache_dir).unwrap().unwrap();
        assert!(desc.contains("Redid: Move to Trash"));
        assert!(!photo_path.exists() && trashed_file.exists());
        assert!(!xmp_path.exists() && trashed_xmp.exists());
        {
            let conn = db.conn().unwrap();
            assert!(queries::get_image(&conn, id).unwrap().is_none());
        }

        // Clear thread-local test trash dir
        TEST_TRASH_DIR.with(|t| *t.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(&temp_dir_path);
    }

    #[test]
    fn url_decode_handles_trashinfo_escapes() {
        assert_eq!(url_decode("/home/u/My%20Photos/a%23b.jpg"), "/home/u/My Photos/a#b.jpg");
        assert_eq!(url_decode("/caf%C3%A9.jpg"), "/café.jpg");
        assert_eq!(url_decode("/plain.jpg"), "/plain.jpg");
        // Malformed escapes are kept as-is rather than dropped.
        assert_eq!(url_decode("/100%zz.jpg"), "/100%zz.jpg");
        assert_eq!(url_decode("/end%"), "/end%");
    }

    /// Writes `name` into a fake trash with a `.trashinfo` whose Path is
    /// percent-encoded the way `gio trash` writes it.
    fn put_in_trash(trash_dir: &Path, original: &Path, name: &str, contents: &[u8]) {
        std::fs::write(trash_dir.join("files").join(name), contents).unwrap();
        let encoded = original.to_string_lossy().replace('%', "%25").replace(' ', "%20").replace('#', "%23");
        std::fs::write(
            trash_dir.join("info").join(format!("{name}.trashinfo")),
            format!("[Trash Info]\nPath={encoded}\nDeletionDate=2026-09-28T12:00:00\n"),
        )
        .unwrap();
    }

    #[test]
    fn undo_trash_skips_rows_whose_file_is_missing_and_can_be_retried() {
        let (db, cache_dir) = make_test_db();
        let root = std::env::temp_dir().join(format!("photon_trash_retry_{}", uuid::Uuid::new_v4().simple()));
        // Spaces and '#' force percent-encoding in the .trashinfo.
        let photos_dir = root.join("My Photos #2");
        let trash_dir = root.join("Trash");
        std::fs::create_dir_all(&photos_dir).unwrap();
        std::fs::create_dir_all(trash_dir.join("info")).unwrap();
        std::fs::create_dir_all(trash_dir.join("files")).unwrap();
        TEST_TRASH_DIR.with(|t| *t.borrow_mut() = Some(trash_dir.clone()));

        let path_a = photos_dir.join("shot a.jpg");
        let path_b = photos_dir.join("shot b.jpg");
        let tag_id = queries::ensure_tag(&db.conn().unwrap(), "keep").unwrap();

        let mut images = Vec::new();
        for (path, hash) in [(&path_a, "hash_a"), (&path_b, "hash_b")] {
            let mut img = Image {
                filename: path.file_name().unwrap().to_string_lossy().into_owned(),
                path: path.clone(),
                hash: hash.into(),
                ..Default::default()
            };
            let mut conn = db.conn().unwrap();
            let tx = conn.transaction().unwrap();
            img.id = queries::insert_image(&tx, &img).unwrap();
            tx.commit().unwrap();
            images.push(img);
        }
        let (id_a, id_b) = (images[0].id.unwrap(), images[1].id.unwrap());
        queries::delete_images(&mut db.conn().unwrap(), &[id_a, id_b]).unwrap();

        // Only A is in the trash; B has been emptied from it.
        put_in_trash(&trash_dir, &path_a, "shot a.jpg", b"A");

        let mut um = UndoManager::new();
        um.push(UndoAction::Trash {
            images: images.clone(),
            tags: vec![(id_a, tag_id), (id_b, tag_id)],
            trashed_paths: vec![(path_a.clone(), None), (path_b.clone(), None)],
        });

        let err = um.undo(&db, &cache_dir).unwrap_err().to_string();
        assert!(err.contains("1 of 2"), "unexpected error: {err}");
        assert!(err.contains("shot b.jpg"), "error should name the missing file: {err}");
        assert_eq!(std::fs::read(&path_a).unwrap(), b"A", "A restored through the encoded path");
        {
            let conn = db.conn().unwrap();
            assert!(queries::get_image(&conn, id_a).unwrap().is_some());
            assert_eq!(queries::get_tags_for_image(&conn, id_a).unwrap().len(), 1);
            assert!(queries::get_image(&conn, id_b).unwrap().is_none(), "no row for a file still missing");
            assert!(queries::get_tags_for_image(&conn, id_b).unwrap().is_empty());
        }
        assert!(um.can_undo(), "a failed undo stays on the stack");
        assert!(!um.can_redo());

        // Once B turns up in the trash, retrying finishes the job without
        // duplicating A.
        put_in_trash(&trash_dir, &path_b, "shot b.jpg", b"B");
        um.undo(&db, &cache_dir).unwrap().unwrap();
        assert_eq!(std::fs::read(&path_b).unwrap(), b"B");
        {
            let conn = db.conn().unwrap();
            assert!(queries::get_image(&conn, id_a).unwrap().is_some());
            assert!(queries::get_image(&conn, id_b).unwrap().is_some());
            assert_eq!(queries::get_tags_for_image(&conn, id_b).unwrap().len(), 1);
        }
        assert!(!um.can_undo());

        TEST_TRASH_DIR.with(|t| *t.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(&root);
    }

}

