//! Schema definitions and migrations.
//!
//! All DDL lives here. Migrations run sequentially on first open.

use crate::error::PhotonError;
use rusqlite::Connection;

/// Run all migrations in order. Idempotent (uses a version table).
pub fn run_migrations(conn: &Connection) -> Result<(), PhotonError> {
    // Version tracking table
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _schema_version (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER DEFAULT (unixepoch())
        );",
    )?;

    let current: i32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let migrations: Vec<(i32, &str)> = vec![
        (1, MIGRATION_001_INITIAL),
        (2, MIGRATION_002_FTS5),
        (3, MIGRATION_003_EDIT_HISTORY),
        (4, MIGRATION_004_GROUP_HASH),
        (5, MIGRATION_005_ORIENTATION_AND_PATH_INDEX),
        (6, MIGRATION_006_ORIGINAL_FILENAME),
    ];

    for (version, sql) in migrations {
        if version > current {
            // A migration and its version stamp commit together or not at all.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO _schema_version (version) VALUES (?1)",
                [version],
            )?;
            tx.commit()?;
            log::info!("Applied migration v{}", version);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Migration 001: Core tables
// ---------------------------------------------------------------------------

const MIGRATION_001_INITIAL: &str = "
CREATE TABLE IF NOT EXISTS images (
    id          INTEGER PRIMARY KEY,
    hash        TEXT NOT NULL UNIQUE,
    path        TEXT NOT NULL,
    filename    TEXT NOT NULL,
    size_bytes  INTEGER,
    width       INTEGER,
    height      INTEGER,
    created_at  INTEGER,
    imported_at INTEGER NOT NULL DEFAULT (unixepoch()),
    year        INTEGER GENERATED ALWAYS AS (CAST(strftime('%Y', datetime(created_at, 'unixepoch')) AS INTEGER)) STORED,
    month       INTEGER GENERATED ALWAYS AS (CAST(strftime('%m', datetime(created_at, 'unixepoch')) AS INTEGER)) STORED,
    day         INTEGER GENERATED ALWAYS AS (CAST(strftime('%d', datetime(created_at, 'unixepoch')) AS INTEGER)) STORED,
    format          TEXT,
    has_sidecar     INTEGER DEFAULT 0,
    metadata_json   TEXT,
    thumbnail_hash  TEXT,
    camera_make     TEXT,
    camera_model    TEXT,
    lens_model      TEXT,
    focal_length    REAL,
    aperture        REAL,
    shutter_speed   TEXT,
    iso             INTEGER,
    latitude        REAL,
    longitude       REAL,
    location_name   TEXT,
    rating          INTEGER DEFAULT 0,
    flagged         INTEGER DEFAULT 0,
    hidden          INTEGER DEFAULT 0,
    title           TEXT,
    description     TEXT
);

CREATE INDEX IF NOT EXISTS idx_images_year_month_day ON images(year, month, day);
CREATE INDEX IF NOT EXISTS idx_images_created_at     ON images(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_images_hash           ON images(hash);
CREATE INDEX IF NOT EXISTS idx_images_format         ON images(format);

-- Tags
CREATE TABLE IF NOT EXISTS tags (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE COLLATE NOCASE,
    color       TEXT,
    is_category INTEGER DEFAULT 0
);

-- Many-to-many: image ↔ tag
CREATE TABLE IF NOT EXISTS image_tags (
    image_id INTEGER NOT NULL,
    tag_id   INTEGER NOT NULL,
    added_at INTEGER DEFAULT (unixepoch()),
    PRIMARY KEY (image_id, tag_id),
    FOREIGN KEY (image_id) REFERENCES images(id) ON DELETE CASCADE,
    FOREIGN KEY (tag_id)   REFERENCES tags(id)   ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_image_tags_image ON image_tags(image_id);
CREATE INDEX IF NOT EXISTS idx_image_tags_tag   ON image_tags(tag_id);

-- Import batches
CREATE TABLE IF NOT EXISTS import_batches (
    id              INTEGER PRIMARY KEY,
    source_type     TEXT,
    source_path     TEXT,
    total_files     INTEGER,
    imported_count  INTEGER DEFAULT 0,
    duplicate_count INTEGER DEFAULT 0,
    error_count     INTEGER DEFAULT 0,
    status          TEXT DEFAULT 'running',
    started_at      INTEGER,
    completed_at    INTEGER
);

-- Library key-value metadata
CREATE TABLE IF NOT EXISTS library_meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);
";

// ---------------------------------------------------------------------------
// Migration 002: FTS5 full-text search
// ---------------------------------------------------------------------------

const MIGRATION_002_FTS5: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS images_fts USING fts5(
    filename,
    metadata_json,
    camera_make,
    camera_model,
    title,
    description,
    content = 'images',
    content_rowid = 'id'
);

-- Triggers to keep FTS in sync
CREATE TRIGGER IF NOT EXISTS images_ai AFTER INSERT ON images BEGIN
    INSERT INTO images_fts(rowid, filename, metadata_json, camera_make, camera_model, title, description)
    VALUES (new.id, new.filename, new.metadata_json, new.camera_make, new.camera_model, new.title, new.description);
END;

CREATE TRIGGER IF NOT EXISTS images_ad AFTER DELETE ON images BEGIN
    INSERT INTO images_fts(images_fts, rowid, filename, metadata_json, camera_make, camera_model, title, description)
    VALUES ('delete', old.id, old.filename, old.metadata_json, old.camera_make, old.camera_model, old.title, old.description);
END;

CREATE TRIGGER IF NOT EXISTS images_au AFTER UPDATE ON images BEGIN
    INSERT INTO images_fts(images_fts, rowid, filename, metadata_json, camera_make, camera_model, title, description)
    VALUES ('delete', old.id, old.filename, old.metadata_json, old.camera_make, old.camera_model, old.title, old.description);
    INSERT INTO images_fts(rowid, filename, metadata_json, camera_make, camera_model, title, description)
    VALUES (new.id, new.filename, new.metadata_json, new.camera_make, new.camera_model, new.title, new.description);
END;
";

// ---------------------------------------------------------------------------
// Migration 003: External edit history
// ---------------------------------------------------------------------------

const MIGRATION_003_EDIT_HISTORY: &str = "
CREATE TABLE IF NOT EXISTS edit_history (
    id             INTEGER PRIMARY KEY,
    image_id       INTEGER NOT NULL,
    tool_name      TEXT,
    operation_type TEXT,
    sidecar_path   TEXT,
    edited_at      INTEGER DEFAULT (unixepoch()),
    FOREIGN KEY (image_id) REFERENCES images(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_edit_history_image ON edit_history(image_id);
CREATE INDEX IF NOT EXISTS idx_edit_history_tool  ON edit_history(tool_name);
";

// ---------------------------------------------------------------------------
// Migration 004: Sidecar grouping (RAW+JPG pairs, edits, XMP)
// ---------------------------------------------------------------------------

const MIGRATION_004_GROUP_HASH: &str = "
ALTER TABLE images ADD COLUMN group_hash TEXT;
CREATE INDEX IF NOT EXISTS idx_images_group_hash ON images(group_hash);
";

// ---------------------------------------------------------------------------
// Migration 005: EXIF orientation, path lookup index
// ---------------------------------------------------------------------------

// `hash` is already indexed by its UNIQUE constraint, so idx_images_hash is redundant.
// The path index lets re-imports skip already-known files without hashing them.
const MIGRATION_005_ORIENTATION_AND_PATH_INDEX: &str = "
ALTER TABLE images ADD COLUMN orientation INTEGER;
CREATE INDEX IF NOT EXISTS idx_images_path ON images(path);
DROP INDEX IF EXISTS idx_images_hash;
";

// ---------------------------------------------------------------------------
// Migration 006: original filename (for Lightroom-style suspected-duplicate checks)
// ---------------------------------------------------------------------------

const MIGRATION_006_ORIGINAL_FILENAME: &str = "
ALTER TABLE images ADD COLUMN original_filename TEXT;
UPDATE images SET original_filename = filename;
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap(); // second run must not fail
    }

    #[test]
    fn generated_columns_work() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Insert an image with created_at = 2024-06-15 12:00:00 UTC
        // That's unix timestamp 1718452800
        conn.execute(
            "INSERT INTO images (hash, path, filename, size_bytes, created_at, imported_at)
             VALUES ('abc123', '/tmp/test.jpg', 'test.jpg', 1024, 1718452800, 1718452800)",
            [],
        )
        .unwrap();

        let (year, month, day): (i32, i32, i32) = conn
            .query_row(
                "SELECT year, month, day FROM images WHERE hash = 'abc123'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(year, 2024);
        assert_eq!(month, 6);
        assert_eq!(day, 15);
    }

    #[test]
    fn group_hash_column_exists() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        conn.execute(
            "INSERT INTO images (hash, path, filename, size_bytes, created_at, imported_at, group_hash)
             VALUES ('g1', '/tmp/IMG_001.ORF', 'IMG_001.ORF', 2048, 1718452800, 1718452800, 'grp_abc')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO images (hash, path, filename, size_bytes, created_at, imported_at, group_hash)
             VALUES ('g2', '/tmp/IMG_001.JPG', 'IMG_001.JPG', 1024, 1718452800, 1718452800, 'grp_abc')",
            [],
        )
        .unwrap();

        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM images WHERE group_hash = 'grp_abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }
}
