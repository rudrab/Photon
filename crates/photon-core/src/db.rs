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
    /// copies are what the user will need.
    pub fn back_up(&self, dir: &Path, keep: usize, min_age: Duration) -> Result<BackupOutcome, PhotonError> {
        std::fs::create_dir_all(dir)?;
        let mut backups = list_backups(dir)?;
        if let Some((_, newest)) = backups.last() {
            if newest.elapsed().is_ok_and(|age| age < min_age) {
                return Ok(BackupOutcome::Recent);
            }
        }

        let conn = self.conn()?;
        let problems: Vec<String> = conn
            .prepare("PRAGMA quick_check")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        if problems != ["ok"] {
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
        let excess = backups.len().saturating_sub(keep.max(1));
        for (old, _) in backups.drain(..excess) {
            if let Err(e) = std::fs::remove_file(&old) {
                log::warn!("Removing old backup {}: {e}", old.display());
            }
        }
        Ok(BackupOutcome::Saved(dest))
    }
}

/// What [`Database::back_up`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum BackupOutcome {
    Saved(PathBuf),
    /// A recent enough backup already exists.
    Recent,
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
}
