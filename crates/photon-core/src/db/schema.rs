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
        (7, MIGRATION_007_THUMBHASH),
        (8, MIGRATION_008_ALBUMS),
        (9, MIGRATION_009_EVENTS),
        (10, MIGRATION_010_XMP_MTIME),
        (11, MIGRATION_011_MISSING),
        (12, MIGRATION_012_EXPORT_PRESETS),
        (13, MIGRATION_013_XMP_REJECT_REPAIR),
        (14, MIGRATION_014_DEFAULT_EXPORT_PRESETS),
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

// ---------------------------------------------------------------------------
// Migration 007: ThumbHash micro-previews for fast placeholder rendering
// ---------------------------------------------------------------------------

const MIGRATION_007_THUMBHASH: &str = "
ALTER TABLE images ADD COLUMN thumbhash BLOB;
";

// ---------------------------------------------------------------------------
// Migration 008: Albums (manual collections)
// ---------------------------------------------------------------------------

const MIGRATION_008_ALBUMS: &str = "
CREATE TABLE IF NOT EXISTS albums (
    id             INTEGER PRIMARY KEY,
    name           TEXT NOT NULL,
    created_at     INTEGER NOT NULL DEFAULT (unixepoch()),
    cover_image_id INTEGER,
    FOREIGN KEY (cover_image_id) REFERENCES images(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS album_images (
    album_id INTEGER NOT NULL,
    image_id INTEGER NOT NULL,
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (album_id, image_id),
    FOREIGN KEY (album_id) REFERENCES albums(id) ON DELETE CASCADE,
    FOREIGN KEY (image_id) REFERENCES images(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_album_images_album ON album_images(album_id);
CREATE INDEX IF NOT EXISTS idx_album_images_image ON album_images(image_id);
";

// ---------------------------------------------------------------------------
// Migration 009: Named events (date range groupings)
// ---------------------------------------------------------------------------

const MIGRATION_009_EVENTS: &str = "
CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    start_date INTEGER NOT NULL,
    end_date   INTEGER NOT NULL,
    comment    TEXT
);

CREATE INDEX IF NOT EXISTS idx_events_dates ON events(start_date, end_date);
";

// ---------------------------------------------------------------------------
// Migration 010: XMP read-back
// ---------------------------------------------------------------------------

const MIGRATION_010_XMP_MTIME: &str = "
ALTER TABLE images ADD COLUMN xmp_mtime INTEGER;
";

// ---------------------------------------------------------------------------
// Migration 011: Missing/offline files
// ---------------------------------------------------------------------------

const MIGRATION_011_MISSING: &str = "
ALTER TABLE images ADD COLUMN missing INTEGER NOT NULL DEFAULT 0;
";

// ---------------------------------------------------------------------------
// Migration 012: Export presets
// ---------------------------------------------------------------------------

const MIGRATION_012_EXPORT_PRESETS: &str = "
CREATE TABLE IF NOT EXISTS export_presets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    config_json TEXT NOT NULL
);
";

// ---------------------------------------------------------------------------
// Migration 013: repair rejects read back from XMP as a -1 rating
// ---------------------------------------------------------------------------

// Before this fix, `xmp:Rating="-1"` was stored in `rating` instead of as a
// reject in `flagged`. The stars it overwrote are gone, so it becomes 0.
const MIGRATION_013_XMP_REJECT_REPAIR: &str = "
UPDATE images SET flagged = -1, rating = 0 WHERE rating < 0;
";

// ---------------------------------------------------------------------------
// Migration 014: default export presets
// ---------------------------------------------------------------------------

// photon-import's `ExportConfig` as JSON; fields left out take their defaults
// (a test there checks these parse).
const MIGRATION_014_DEFAULT_EXPORT_PRESETS: &str = r#"
INSERT OR IGNORE INTO export_presets (name, config_json) VALUES
 ('Web 2048 sRGB', '{"format":{"Jpeg":{"quality":85}},"resize":{"FitLongEdge":2048},"sharpening":"Screen","suffix":"_web","color_space":"Srgb"}'),
 ('Client full-res', '{"format":{"Jpeg":{"quality":95}},"resize":"Original","sharpening":"None","color_space":"Srgb"}'),
 ('Print TIFF', '{"format":{"Tiff":{"bit_depth":16}},"resize":"Original","sharpening":"Print","color_space":"AdobeRgb"}');
"#;

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

    #[test]
    fn albums_and_events_tables_exist() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        conn.execute(
            "INSERT INTO albums (name) VALUES ('Vacation 2026')",
            [],
        )
        .unwrap();
        let album_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO events (name, start_date, end_date) VALUES ('Trip', 1700000000, 1700100000)",
            [],
        )
        .unwrap();
        let event_id = conn.last_insert_rowid();

        assert!(album_id > 0);
        assert!(event_id > 0);
    }
}
