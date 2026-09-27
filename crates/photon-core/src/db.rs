//! Database layer: connection pool, schema management, and query API.
//!
//! Uses `db/schema.rs` for DDL and migrations, `db/queries.rs` for all reads/writes.

pub mod queries;
pub mod schema;

use crate::error::PhotonError;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::path::Path;

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
}
