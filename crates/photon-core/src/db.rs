//! Database layer: connection pool, schema management, and query API.
//!
//! Uses `db/schema.rs` for DDL and migrations, `db/queries.rs` for all reads/writes.

pub mod queries;
pub mod schema;

use crate::error::PhotonError;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub type DbPool = Pool<SqliteConnectionManager>;

/// Per-connection settings. `busy_timeout` comes first so everything after it
/// waits for locks instead of failing with "database is locked".
const CONNECTION_PRAGMAS: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA synchronous = NORMAL;
    PRAGMA foreign_keys = ON;
    PRAGMA cache_size = -16000;
    PRAGMA temp_store = MEMORY;
";

/// High-level database handle wrapping an r2d2 connection pool.
#[derive(Clone)]
pub struct Database {
    pool: DbPool,
}

impl Database {
    /// Open (or create) a database at `path` and run all migrations.
    pub fn open(path: &Path) -> Result<Self, PhotonError> {
        // WAL mode is a property of the file, and switching to it needs an
        // exclusive lock. Do it (and the migrations) on one connection before
        // the pool opens its connections in parallel, or they race on a new file.
        let setup = Connection::open(path)?;
        setup.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA journal_mode = WAL;")?;
        schema::run_migrations(&setup)?;
        drop(setup);

        Self::build_pool(SqliteConnectionManager::file(path), 4)
    }

    /// Open an in-memory database (for tests / benchmarks).
    pub fn open_in_memory() -> Result<Self, PhotonError> {
        // Every in-memory connection is its own database, so the pool must hold exactly one.
        let db = Self::build_pool(SqliteConnectionManager::memory(), 1)?;
        schema::run_migrations(&*db.conn()?)?;
        Ok(db)
    }

    fn build_pool(manager: SqliteConnectionManager, size: u32) -> Result<Self, PhotonError> {
        let manager = manager.with_init(|conn| conn.execute_batch(CONNECTION_PRAGMAS));
        let pool = Pool::builder()
            .max_size(size)
            .build(manager)
            .map_err(|e| PhotonError::Other(format!("Pool creation failed: {}", e)))?;
        Ok(Self { pool })
    }

    /// Get a raw connection from the pool.
    pub fn conn(
        &self,
    ) -> Result<r2d2::PooledConnection<SqliteConnectionManager>, PhotonError> {
        Ok(self.pool.get()?)
    }

    /// Expose the pool for crates that need direct access (e.g. batch transactions).
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// Check the database and save a copy of it to `dir` as
    /// `photon-YYYYMMDD-HHMMSS.db`, unless the newest copy there is younger
    /// than `min_age`. Keeps the newest `keep` copies.
    ///
    /// A database that fails SQLite's integrity check is not backed up, and
    /// no old copy is removed: the error says what is wrong, and the last good
    /// copies are what the user will need. A check that could only not run
    /// because other connections were writing is retried, then skipped
    /// ([`BackupOutcome::Busy`]), never reported as damage.
    pub fn back_up(&self, dir: &Path, keep: usize, min_age: Duration) -> Result<BackupOutcome, PhotonError> {
        self.back_up_checked(dir, keep, min_age, Duration::from_secs(2), quick_check)
    }

    fn back_up_checked(
        &self,
        dir: &Path,
        keep: usize,
        min_age: Duration,
        retry_delay: Duration,
        check: impl Fn(&Connection) -> Result<Vec<String>, rusqlite::Error>,
    ) -> Result<BackupOutcome, PhotonError> {
        std::fs::create_dir_all(dir)?;
        let mut backups = list_backups(dir)?;
        if let Some((_, newest)) = backups.last() {
            if newest.elapsed().is_ok_and(|age| age < min_age) {
                return Ok(BackupOutcome::Recent);
            }
        }

        let conn = self.conn()?;
        let mut attempts = 0;
        loop {
            let problems = match check(&conn) {
                Ok(problems) => problems,
                Err(e) if is_busy_error(&e) => vec![e.to_string()],
                Err(e) => return Err(e.into()),
            };
            if problems == ["ok"] {
                break;
            }
            if is_busy(&problems) {
                attempts += 1;
                if attempts >= CHECK_ATTEMPTS {
                    log::info!("Library backup skipped: the database stayed busy ({})", problems.join("; "));
                    return Ok(BackupOutcome::Busy);
                }
                std::thread::sleep(retry_delay);
                continue;
            }
            return Err(PhotonError::Other(format!(
                "The library database failed its integrity check: {}",
                problems.join("; ")
            )));
        }

        let name = chrono::Local::now().format("photon-%Y%m%d-%H%M%S.db").to_string();
        let dest = dir.join(&name);
        let part = dir.join(format!("{name}.part"));
        let _ = std::fs::remove_file(&part);
        // A consistent snapshot, even while other connections write.
        conn.execute("VACUUM INTO ?1", [part.to_string_lossy()])?;
        std::fs::File::open(&part)?.sync_all()?;
        std::fs::rename(&part, &dest)?;

        backups.push((dest.clone(), SystemTime::now()));
        rotate(backups, keep);
        Ok(BackupOutcome::Saved(dest))
    }
}

/// Remove the oldest of `backups` (oldest first) beyond `keep`.
fn rotate(mut backups: Vec<(PathBuf, SystemTime)>, keep: usize) {
    let excess = backups.len().saturating_sub(keep.max(1));
    for (old, _) in backups.drain(..excess) {
        if let Err(e) = std::fs::remove_file(&old) {
            log::warn!("Removing old backup {}: {e}", old.display());
        }
    }
}

/// A saved copy of the library database, as listed for a restore.
#[derive(Debug, Clone)]
pub struct CatalogBackup {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub size: u64,
    /// How many photos it holds; `None` if it can't be read (damaged, or
    /// not a Photon library).
    pub photos: Option<i64>,
}

/// The backups in `dir`, newest first.
pub fn catalog_backups(dir: &Path) -> Result<Vec<CatalogBackup>, PhotonError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut list: Vec<CatalogBackup> = list_backups(dir)?
        .into_iter()
        .map(|(path, modified)| CatalogBackup {
            size: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            photos: count_photos(&path).ok(),
            path,
            modified,
        })
        .collect();
    list.reverse();
    Ok(list)
}

/// Open a backup for reading only, without creating `-wal`/`-shm` files
/// next to it (it may be on a read-only or slow disk).
fn open_backup(path: &Path) -> Result<Connection, PhotonError> {
    let escaped = path.to_string_lossy().replace('%', "%25").replace('?', "%3f").replace('#', "%23");
    Ok(Connection::open_with_flags(
        format!("file:{escaped}?immutable=1"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?)
}

fn count_photos(path: &Path) -> Result<i64, PhotonError> {
    Ok(open_backup(path)?.query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0))?)
}

/// Copy the newest backup in `dir` to `mirror` (meant to be another disk),
/// unless it is already there, and keep the newest `keep` copies there.
/// Returns the copy made, if one was.
pub fn mirror_newest_backup(dir: &Path, mirror: &Path, keep: usize) -> Result<Option<PathBuf>, PhotonError> {
    let Some((newest, _)) = list_backups(dir)?.pop() else { return Ok(None) };
    let name = newest.file_name().unwrap_or_default().to_os_string();
    std::fs::create_dir_all(mirror)?;
    let dest = mirror.join(&name);
    if dest.exists() {
        return Ok(None);
    }
    let mut part = dest.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    std::fs::copy(&newest, &part)?;
    std::fs::File::open(&part)?.sync_all()?;
    std::fs::rename(&part, &dest)?;
    rotate(list_backups(mirror)?, keep);
    Ok(Some(dest))
}

/// Where a restore waits for the next start.
fn staged_restore_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.as_os_str().to_owned();
    p.push(".restore");
    PathBuf::from(p)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

/// Check `backup` and place a copy of it next to the library database
/// `db_path`, to replace it at the next start ([`apply_staged_restore`]).
/// Done this way because the running app's open connections can't safely
/// have the file swapped under them.
pub fn stage_restore(backup: &Path, db_path: &Path) -> Result<(), PhotonError> {
    let staged = staged_restore_path(db_path);
    let part = with_suffix(&staged, ".part");
    std::fs::copy(backup, &part)?;
    // Check the copy that will be used, not the original. (Read-write: the
    // check of the full-text index needs it.)
    let checked = check_library_file(&part).and_then(|()| Ok(std::fs::File::open(&part)?.sync_all()?));
    if let Err(e) = checked {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, &staged)?;
    Ok(())
}

/// SQLite's integrity check passes and the file is a Photon library.
fn check_library_file(path: &Path) -> Result<(), PhotonError> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let problems: Vec<String> = conn
        .prepare("PRAGMA quick_check")
        .and_then(|mut stmt| stmt.query_map([], |r| r.get(0))?.collect())
        .map_err(|e| PhotonError::Other(format!("The backup can't be read: {e}")))?;
    if problems != ["ok"] {
        return Err(PhotonError::Other(format!("The backup is damaged: {}", problems.join("; "))));
    }
    conn.query_row("SELECT COUNT(*) FROM images", [], |r| r.get::<_, i64>(0))
        .map_err(|e| PhotonError::Other(format!("Not a Photon library: {e}")))?;
    // Leave no -wal/-shm behind (the file is renamed next).
    conn.execute_batch("PRAGMA journal_mode = DELETE;")?;
    Ok(())
}

/// Whether a restore is waiting for the next start.
pub fn restore_pending(db_path: &Path) -> bool {
    staged_restore_path(db_path).exists()
}

/// At startup, before [`Database::open`]: if a restore was staged, put it in
/// place of the library database `db_path`. The current database — with
/// its `-wal` and `-shm` files, which hold its latest changes — is first
/// copied to a new `before-restore-…` folder in `keep_dir`, and nothing is
/// replaced unless that copy succeeded. Returns that folder.
pub fn apply_staged_restore(db_path: &Path, keep_dir: &Path) -> Result<Option<PathBuf>, PhotonError> {
    let staged = staged_restore_path(db_path);
    if !staged.exists() {
        return Ok(None);
    }
    let kept = keep_dir.join(chrono::Local::now().format("before-restore-%Y%m%d-%H%M%S").to_string());
    std::fs::create_dir_all(&kept)?;
    let files: Vec<PathBuf> =
        ["", "-wal", "-shm"].iter().map(|s| with_suffix(db_path, s)).filter(|f| f.exists()).collect();
    for file in &files {
        let copy = kept.join(file.file_name().unwrap_or_default());
        std::fs::copy(file, &copy)?;
        std::fs::File::open(&copy)?.sync_all()?;
    }
    // The old WAL must not be replayed into the restored database.
    for suffix in ["-wal", "-shm"] {
        let f = with_suffix(db_path, suffix);
        if f.exists() {
            std::fs::remove_file(&f)?;
        }
    }
    std::fs::rename(&staged, db_path)?;
    Ok(Some(kept))
}

/// What [`Database::back_up`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum BackupOutcome {
    Saved(PathBuf),
    /// A recent enough backup already exists.
    Recent,
    /// The database was too busy to be checked (other connections kept
    /// writing): nothing was done; the next run tries again.
    Busy,
}

/// How often a busy database is re-checked before a backup is put off.
const CHECK_ATTEMPTS: u32 = 5;

fn quick_check(conn: &Connection) -> Result<Vec<String>, rusqlite::Error> {
    conn.prepare("PRAGMA quick_check")?.query_map([], |r| r.get(0))?.collect()
}

/// SQLite reports a lock it couldn't wait out in the *text* of a check
/// result (e.g. "unable to validate the inverted index for FTS5 table
/// main.images_fts: database is locked"), which is no sign of damage.
fn is_busy(problems: &[String]) -> bool {
    !problems.is_empty()
        && problems.iter().all(|p| {
            let p = p.to_lowercase();
            p.contains("database is locked") || p.contains("database table is locked") || p.contains("database is busy")
        })
}

fn is_busy_error(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Backups in `dir`, oldest first.
fn list_backups(dir: &Path) -> Result<Vec<(PathBuf, SystemTime)>, PhotonError> {
    let mut backups = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with("photon-") && name.ends_with(".db") {
            let modified = std::fs::metadata(&path)?.modified()?;
            backups.push((path, modified));
        }
    }
    // Names sort by time, and survive copying where mtimes may not.
    backups.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(backups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_fresh_file_database_does_not_race() {
        // The pool used to switch a new file to WAL from 4 connections at once.
        for _ in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(&dir.path().join("photon.db")).unwrap();

            let conns: Vec<_> = (0..4).map(|_| db.conn().unwrap()).collect();
            for c in &conns {
                let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
                assert_eq!(mode, "wal");
                let fk: i32 = c.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
                assert_eq!(fk, 1);
            }
        }
    }

    #[test]
    fn newest_backup_is_mirrored_once_and_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("photon.db")).unwrap();
        let (backups, mirror) = (dir.path().join("backups"), dir.path().join("other disk/Photon Catalog Backups"));

        assert_eq!(mirror_newest_backup(&backups, &mirror, 2).ok(), None, "no backups dir yet");
        let BackupOutcome::Saved(first) = db.back_up(&backups, 5, Duration::ZERO).unwrap() else { panic!() };
        let copy = mirror_newest_backup(&backups, &mirror, 2).unwrap().expect("mirrored");
        assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(&first).unwrap());
        assert_eq!(mirror_newest_backup(&backups, &mirror, 2).unwrap(), None, "already there");

        for stamp in ["20200101-000000", "20210101-000000"] {
            std::fs::write(mirror.join(format!("photon-{stamp}.db")), b"old").unwrap();
        }
        std::thread::sleep(Duration::from_millis(1100));
        db.back_up(&backups, 5, Duration::ZERO).unwrap();
        mirror_newest_backup(&backups, &mirror, 2).unwrap().expect("the new one");
        let listed = catalog_backups(&mirror).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|b| b.photos == Some(0)), "{listed:?}");
        assert!(listed[0].path > listed[1].path, "newest first");
    }

    #[test]
    fn a_staged_restore_replaces_the_database_at_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("photon.db");
        let backups = dir.path().join("backups");
        let tags = |db: &Database| -> Vec<String> {
            let conn = db.conn().unwrap();
            let mut stmt = conn.prepare("SELECT name FROM tags ORDER BY name").unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };

        let db = Database::open(&db_path).unwrap();
        db.conn().unwrap().execute("INSERT INTO tags (name) VALUES ('before')", []).unwrap();
        let BackupOutcome::Saved(backup) = db.back_up(&backups, 5, Duration::ZERO).unwrap() else { panic!() };
        db.conn().unwrap().execute("INSERT INTO tags (name) VALUES ('after')", []).unwrap();

        // Garbage is refused, and nothing is staged.
        let junk = dir.path().join("photon-20200101-000000.db");
        std::fs::write(&junk, b"not a database").unwrap();
        assert!(stage_restore(&junk, &db_path).is_err());
        assert!(!restore_pending(&db_path));

        stage_restore(&backup, &db_path).unwrap();
        assert!(restore_pending(&db_path));
        assert_eq!(tags(&db), ["after", "before"], "nothing changes while running");
        drop(db);

        let kept = apply_staged_restore(&db_path, &backups).unwrap().expect("applied");
        assert!(!restore_pending(&db_path));
        let db = Database::open(&db_path).unwrap();
        assert_eq!(tags(&db), ["before"]);

        // The replaced database is kept, with its latest change.
        let old = Database::open(&kept.join("photon.db")).unwrap();
        assert_eq!(tags(&old), ["after", "before"]);
        // Not listed as a backup to restore.
        assert_eq!(catalog_backups(&backups).unwrap().len(), 1);
        assert_eq!(apply_staged_restore(&db_path, &backups).unwrap(), None);
    }

    #[test]
    fn backups_are_checked_rotated_and_not_repeated_too_soon() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("photon.db")).unwrap();
        db.conn().unwrap().execute("INSERT INTO tags (name) VALUES ('kept')", []).unwrap();
        let backups = dir.path().join("backups");

        let BackupOutcome::Saved(first) = db.back_up(&backups, 2, Duration::from_secs(3600)).unwrap() else {
            panic!("expected a backup");
        };
        let copy = Connection::open(&first).unwrap();
        let name: String = copy.query_row("SELECT name FROM tags", [], |r| r.get(0)).unwrap();
        assert_eq!(name, "kept");

        assert_eq!(db.back_up(&backups, 2, Duration::from_secs(3600)).unwrap(), BackupOutcome::Recent);

        // Older copies beyond `keep` go, oldest first.
        for stamp in ["20200101-000000", "20210101-000000"] {
            std::fs::write(backups.join(format!("photon-{stamp}.db")), b"old").unwrap();
        }
        std::fs::write(backups.join("unrelated.db"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(1100)); // a new timestamp in the name
        db.back_up(&backups, 2, Duration::ZERO).unwrap();
        let mut left: Vec<_> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left.len(), 3, "{left:?}");
        assert!(left.contains(&"unrelated.db".to_string()));
        assert!(!left.iter().any(|n| n.contains("2020") || n.contains("2021")));
    }

    #[test]
    fn a_busy_database_is_not_reported_as_damaged() {
        assert!(is_busy(&["unable to validate the inverted index for FTS5 table main.images_fts: database is locked".to_string()]));
        assert!(!is_busy(&["ok".to_string()]));
        assert!(!is_busy(&["*** in database main ***\nPage 9: btreeInitPage() returns error code 11".to_string()]));
        // A lock message next to real damage is still damage.
        assert!(!is_busy(&["database is locked".to_string(), "row 5 missing from index".to_string()]));

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("photon.db")).unwrap();
        let backups = dir.path().join("backups");
        let locked = |_: &Connection| Ok(vec!["unable to validate the inverted index: database is locked".to_string()]);

        // Busy every time: put off, no error, no backup made.
        let out = db.back_up_checked(&backups, 3, Duration::ZERO, Duration::ZERO, locked).unwrap();
        assert_eq!(out, BackupOutcome::Busy);
        assert!(list_backups(&backups).unwrap().is_empty());

        // Busy a few times, then fine: backed up.
        let calls = std::cell::Cell::new(0);
        let flaky = |c: &Connection| {
            calls.set(calls.get() + 1);
            if calls.get() < 3 { locked(c) } else { quick_check(c) }
        };
        let out = db.back_up_checked(&backups, 3, Duration::ZERO, Duration::ZERO, flaky).unwrap();
        assert!(matches!(out, BackupOutcome::Saved(_)), "{out:?}");
        assert_eq!(calls.get(), 3);

        // Real damage is still an error, and is not retried.
        let calls = std::cell::Cell::new(0);
        let damaged = |_: &Connection| {
            calls.set(calls.get() + 1);
            Ok(vec!["row 5 missing from index idx_images_hash".to_string()])
        };
        let err = db.back_up_checked(&backups, 3, Duration::ZERO, Duration::ZERO, damaged).unwrap_err();
        assert!(err.to_string().contains("integrity check"), "{err}");
        assert_eq!(calls.get(), 1);
    }
}
