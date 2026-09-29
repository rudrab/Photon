//! digiKam database import source.
//!
//! digiKam keeps its library in SQLite (`digikam4.db`, by default in the
//! collection folder). A photo's path is split over three tables:
//!
//!   AlbumRoots.identifier + specificPath   the collection folder, on a volume
//!   Albums.relativePath                    the album folder inside it ("/" = root)
//!   Images.name                            the file name
//!
//! `identifier` is `volumeid:?path=/abs/dir` or `volumeid:?uuid=<fs uuid>`; for
//! the latter `specificPath` is relative to where that volume is mounted.
//!
//! Besides the files, the import carries over digiKam's star ratings
//! (`ImageInformation.rating`, -1 = none), pick labels (internal tags
//! `_Pick_Label_Rejected_` / `_Pick_Label_Accepted_`) and the user's tags
//! (by their leaf name: "Places/Paris" becomes "Paris").

use super::ImportSource;
use photon_core::db::queries;
use photon_core::models::ImageFormat;
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Images.status for photos digiKam shows (not trashed or obsolete).
const STATUS_VISIBLE: i32 = 1;
const INTERNAL_TAGS_ROOT: &str = "_Digikam_Internal_Tags_";
const PICK_REJECTED: &str = "_Pick_Label_Rejected_";
const PICK_ACCEPTED: &str = "_Pick_Label_Accepted_";

pub struct DigikamSource {
    db_path: PathBuf,
    db_path_str: String,
}

/// What digiKam knows about one photo.
#[derive(Debug, Default, PartialEq)]
struct PhotoInfo {
    rating: i32,
    /// -1 rejected, 0 none, 1 accepted (Photon's flag values).
    flag: i32,
    tags: Vec<String>,
}

impl DigikamSource {
    pub fn new(db_path: PathBuf) -> Self {
        let db_path_str = db_path.to_string_lossy().to_string();
        Self { db_path, db_path_str }
    }

    fn open(&self) -> anyhow::Result<Connection> {
        // Read-only: digiKam may be running, and its database is not ours.
        Ok(Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?)
    }

    /// Every visible photo: (digiKam image id, path on disk).
    fn photos(&self, conn: &Connection) -> anyhow::Result<Vec<(i64, PathBuf)>> {
        let mut roots: HashMap<i64, PathBuf> = HashMap::new();
        let mut stmt = conn.prepare("SELECT id, identifier, specificPath FROM AlbumRoots")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?;
        for row in rows {
            let (id, identifier, specific) = row?;
            roots.insert(id, resolve_root(&identifier, &specific));
        }

        let mut stmt = conn.prepare(
            "SELECT i.id, a.albumRoot, a.relativePath, i.name
             FROM Images i JOIN Albums a ON i.album = a.id
             WHERE i.status = ?1",
        )?;
        let rows = stmt.query_map([STATUS_VISIBLE], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        })?;
        let mut photos = Vec::new();
        for row in rows {
            let (id, root, relative, name) = row?;
            let Some(root) = roots.get(&root) else { continue };
            photos.push((id, root.join(relative.trim_start_matches('/')).join(name)));
        }
        Ok(photos)
    }

    /// Ratings, pick labels and tags, by digiKam image id.
    fn infos(&self, conn: &Connection) -> anyhow::Result<HashMap<i64, PhotoInfo>> {
        let mut infos: HashMap<i64, PhotoInfo> = HashMap::new();

        let mut stmt = conn.prepare("SELECT imageid, rating FROM ImageInformation WHERE rating > 0")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i32>(1)?)))? {
            let (id, rating) = row?;
            infos.entry(id).or_default().rating = rating.min(5);
        }

        // Tag tree: id → (parent, name).
        let mut tags: HashMap<i64, (i64, String)> = HashMap::new();
        let mut stmt = conn.prepare("SELECT id, pid, name FROM Tags")?;
        for row in stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0), r.get::<_, String>(2)?))
        })? {
            let (id, pid, name) = row?;
            tags.insert(id, (pid, name));
        }
        // Face-recognition placeholders and the like (older databases lack the table).
        let flagged_internal: HashSet<i64> = conn
            .prepare("SELECT tagid FROM TagProperties WHERE property = 'internalTag'")
            .and_then(|mut stmt| stmt.query_map([], |r| r.get(0))?.collect())
            .unwrap_or_default();
        let is_internal = |mut id: i64| {
            for _ in 0..64 {
                if flagged_internal.contains(&id) {
                    return true;
                }
                let Some((pid, name)) = tags.get(&id) else { return false };
                if name == INTERNAL_TAGS_ROOT || name.starts_with('_') {
                    return true;
                }
                if *pid == 0 {
                    return false;
                }
                id = *pid;
            }
            true // a cycle: don't trust it
        };

        let mut stmt = conn.prepare("SELECT imageid, tagid FROM ImageTags")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (image, tag) = row?;
            let Some((_, name)) = tags.get(&tag) else { continue };
            match name.as_str() {
                PICK_REJECTED => infos.entry(image).or_default().flag = -1,
                PICK_ACCEPTED => infos.entry(image).or_default().flag = 1,
                _ if !is_internal(tag) => infos.entry(image).or_default().tags.push(name.clone()),
                _ => {}
            }
        }
        Ok(infos)
    }
}

impl ImportSource for DigikamSource {
    fn scan(&self) -> anyhow::Result<Vec<PathBuf>> {
        let conn = self.open()?;
        let supported = ImageFormat::all_extensions();
        let mut paths = Vec::new();
        for (_, path) in self.photos(&conn)? {
            let ok = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| supported.contains(&e.to_lowercase().as_str()));
            if !ok {
                continue; // videos, sidecars
            }
            if path.exists() {
                paths.push(path);
            } else {
                log::warn!("digiKam references missing file: {}", path.display());
            }
        }
        Ok(paths)
    }

    fn source_type(&self) -> &str {
        "digikam"
    }

    fn source_description(&self) -> &str {
        &self.db_path_str
    }

    fn apply_metadata(&self, photon: &Connection) -> anyhow::Result<usize> {
        let conn = self.open()?;
        let mut infos = self.infos(&conn)?;
        let tx = photon.unchecked_transaction()?;
        let mut tag_ids: HashMap<String, i64> = HashMap::new();
        let mut updated = Vec::new();
        for (dk_id, path) in self.photos(&conn)? {
            let Some(info) = infos.remove(&dk_id) else { continue };
            let Some(id) = queries::image_id_by_path(&tx, &path.to_string_lossy())? else {
                continue; // not imported (unsupported, missing, or a duplicate of another file)
            };
            if info.rating > 0 {
                queries::set_rating(&tx, id, info.rating)?;
            }
            if info.flag != 0 {
                queries::set_flag(&tx, id, info.flag)?;
            }
            for name in &info.tags {
                let tag = match tag_ids.get(name) {
                    Some(&tag) => tag,
                    None => {
                        let tag = queries::ensure_tag(&tx, name)?;
                        tag_ids.insert(name.clone(), tag);
                        tag
                    }
                };
                queries::tag_image(&tx, id, tag)?;
            }
            updated.push((id, path));
        }
        tx.commit()?;

        // Into the sidecars too, like any change in Photon: darktable sees
        // them, and a later read of the sidecar doesn't drop them.
        let known: Vec<String> = queries::get_all_tags(photon)?.into_iter().map(|t| t.name).collect();
        for (id, path) in &updated {
            let Some(image) = queries::get_image(photon, *id)? else { continue };
            let tags: Vec<String> = queries::get_tags_for_image(photon, *id)?.into_iter().map(|t| t.name).collect();
            let update = crate::sidecar::XmpUpdate {
                rating: Some(image.rating),
                rejected: Some(image.flagged == -1),
                keywords: Some(crate::sidecar::Keywords { tags: &tags, known: &known }),
                ..Default::default()
            };
            if let Err(e) = crate::sidecar::write_image_xmp(photon, *id, path, &update) {
                log::warn!("Writing XMP for {}: {e}", path.display());
            }
        }
        Ok(updated.len())
    }
}

/// The folder an album root points to. For a volume given by UUID, tries
/// each place that filesystem is mounted (btrfs mounts one filesystem at
/// several points, e.g. `/` and `/home`) and takes the first that exists.
fn resolve_root(identifier: &str, specific: &str) -> PathBuf {
    let query = identifier.split_once('?').map_or("", |(_, q)| q);
    let param = |key: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
            .map(percent_decode)
    };
    let relative = specific.trim_start_matches('/');

    if let Some(dir) = param("path") {
        return PathBuf::from(dir).join(relative);
    }
    if let Some(uuid) = param("uuid") {
        let candidates = mount_points_of_uuid(&uuid);
        if let Some(found) = candidates.iter().map(|m| m.join(relative)).find(|p| p.is_dir()) {
            return found;
        }
    }
    PathBuf::from("/").join(relative)
}

fn mount_points_of_uuid(uuid: &str) -> Vec<PathBuf> {
    let Ok(device) = std::fs::canonicalize(Path::new("/dev/disk/by-uuid").join(uuid)) else {
        return Vec::new();
    };
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else { return Vec::new() };
    let mut points: Vec<PathBuf> = mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (dev, point) = (fields.next()?, fields.next()?);
            let dev = std::fs::canonicalize(dev).ok()?;
            (dev == device).then(|| PathBuf::from(point.replace("\\040", " ")))
        })
        .collect();
    // Deepest first: /home before / when both hold the filesystem.
    points.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    points
}

/// Decode `%XX` escapes (digiKam stores identifiers as URLs).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where digiKam keeps its database: the "Database Name" folder from its
/// settings (native or Flatpak), else the default ~/Pictures/digikam4.db.
pub fn find_default_db() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let configs = [
        dirs::config_dir().map(|c| c.join("digikamrc")),
        Some(home.join(".var/app/org.kde.digikam/config/digikamrc")),
    ];
    for config in configs.into_iter().flatten() {
        let Ok(text) = std::fs::read_to_string(&config) else { continue };
        for line in text.lines() {
            if let Some(dir) = line.strip_prefix("Database Name=") {
                let db = PathBuf::from(dir.trim()).join("digikam4.db");
                if db.exists() {
                    return Some(db);
                }
            }
        }
    }
    let default = dirs::picture_dir().unwrap_or_else(|| home.join("Pictures")).join("digikam4.db");
    default.exists().then_some(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A digiKam database with the tables (and columns) the import reads.
    fn digikam_db(path: &Path, root: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE AlbumRoots (id INTEGER PRIMARY KEY, label TEXT, status INTEGER, type INTEGER,
                                      identifier TEXT, specificPath TEXT);
             CREATE TABLE Albums (id INTEGER PRIMARY KEY, albumRoot INTEGER, relativePath TEXT);
             CREATE TABLE Images (id INTEGER PRIMARY KEY, album INTEGER, name TEXT, status INTEGER,
                                  category INTEGER);
             CREATE TABLE ImageInformation (imageid INTEGER PRIMARY KEY, rating INTEGER);
             CREATE TABLE Tags (id INTEGER PRIMARY KEY, pid INTEGER, name TEXT);
             CREATE TABLE ImageTags (imageid INTEGER, tagid INTEGER);
             CREATE TABLE TagProperties (tagid INTEGER, property TEXT, value TEXT);",
        )
        .unwrap();
        // The root folder name has a space, stored URL-encoded.
        let identifier = format!("volumeid:?path={}", root.to_string_lossy().replace(' ', "%20"));
        conn.execute(
            "INSERT INTO AlbumRoots VALUES (1, 'Photos', 0, 1, ?1, '/')",
            [identifier],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO Albums VALUES (1, 1, '/'), (2, 1, '/2024/Paris');
             INSERT INTO Images VALUES
                 (1, 1, 'a.jpg', 1, 1),
                 (2, 2, 'b.jpg', 1, 1),
                 (3, 2, 'gone.jpg', 3, 1),   -- trashed in digiKam
                 (4, 2, 'audio.mp3', 1, 3),  -- audio (unsupported)
                 (5, 2, 'missing.jpg', 1, 1);
             INSERT INTO ImageInformation VALUES (1, 4), (2, -1);
             INSERT INTO Tags VALUES
                 (1, 0, 'Places'), (2, 1, 'Paris'),
                 (10, 0, '_Digikam_Internal_Tags_'),
                 (11, 10, '_Pick_Label_Accepted_'), (12, 10, '_Pick_Label_Rejected_'),
                 (13, 10, '_Color_Label_Red_'),
                 (20, 0, 'People'), (21, 20, 'Unknown');
             INSERT INTO TagProperties VALUES (21, 'internalTag', NULL);
             INSERT INTO ImageTags VALUES (1, 2), (1, 11), (1, 13), (1, 21), (2, 12);",
        )
        .unwrap();
    }

    fn setup() -> (tempfile::TempDir, PathBuf, DigikamSource) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("My Photos");
        fs::create_dir_all(root.join("2024/Paris")).unwrap();
        for f in ["a.jpg", "2024/Paris/b.jpg", "2024/Paris/gone.jpg", "2024/Paris/audio.mp3"] {
            fs::write(root.join(f), b"x").unwrap();
        }
        let db = dir.path().join("digikam4.db");
        digikam_db(&db, &root);
        (dir, root, DigikamSource::new(db))
    }

    #[test]
    fn scans_visible_existing_photos() {
        let (_dir, root, source) = setup();
        let mut paths = source.scan().unwrap();
        paths.sort();
        assert_eq!(paths, vec![root.join("2024/Paris/b.jpg"), root.join("a.jpg")]);
    }

    #[test]
    fn reads_ratings_picks_and_user_tags() {
        let (_dir, _root, source) = setup();
        let infos = source.infos(&source.open().unwrap()).unwrap();
        assert_eq!(infos[&1], PhotoInfo { rating: 4, flag: 1, tags: vec!["Paris".into()] });
        assert_eq!(infos[&2], PhotoInfo { rating: 0, flag: -1, tags: vec![] });
    }

    #[test]
    fn carries_metadata_into_the_library() {
        let (_dir, root, source) = setup();
        let db = photon_core::db::Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        let mut img = photon_core::models::Image::new(root.join("a.jpg"), "h1".into(), 1);
        img.format = Some(ImageFormat::Jpeg);
        let tx = conn.transaction().unwrap();
        queries::insert_image(&tx, &img).unwrap();
        tx.commit().unwrap();

        assert_eq!(source.apply_metadata(&conn).unwrap(), 1);
        let id = queries::image_id_by_path(&conn, &root.join("a.jpg").to_string_lossy()).unwrap().unwrap();
        let stored = queries::get_image(&conn, id).unwrap().unwrap();
        assert_eq!((stored.rating, stored.flagged), (4, 1));
        let tags: Vec<String> = queries::get_tags_for_image(&conn, id).unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(tags, ["Paris"]);

        // Also in the sidecar, so reading it back keeps them.
        let xmp = crate::sidecar::find_xmp(&root.join("a.jpg")).expect("sidecar written");
        let read = crate::sidecar::read_xmp_metadata(&xmp).unwrap();
        assert_eq!((read.rating, read.tags), (Some(4), vec!["Paris".to_string()]));
        assert!(!crate::sidecar::read_image_xmp(&mut conn, id, &root.join("a.jpg"), stored.xmp_mtime).unwrap());
    }

    #[test]
    fn roots_resolve_from_path_and_unknown_uuid() {
        assert_eq!(resolve_root("volumeid:?path=/media/My%20Disk", "/"), PathBuf::from("/media/My Disk"));
        assert_eq!(
            resolve_root("volumeid:?uuid=00000000-no-such-disk", "/home/me/Pictures"),
            PathBuf::from("/home/me/Pictures")
        );
    }
}
