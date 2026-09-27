//! All database queries: inserts, lookups, hierarchical browsing, FTS search, tags.

use crate::error::PhotonError;
use crate::models::{Image, ImageFormat, ImportBatch, LibraryQuery, Tag, TimelineItem};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::HashSet;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Image writes
// ---------------------------------------------------------------------------

/// Insert an image, ignoring duplicates (by hash). Returns `Some(id)` on success,
/// `None` if the hash already existed.
pub fn insert_image(tx: &Transaction, img: &Image) -> Result<Option<i64>, PhotonError> {
    let mut stmt = tx.prepare_cached(
        "INSERT OR IGNORE INTO images (
            hash, path, filename, size_bytes, width, height,
            created_at, imported_at, format, has_sidecar, metadata_json, thumbnail_hash,
            camera_make, camera_model, lens_model, focal_length, aperture, shutter_speed, iso,
            latitude, longitude, location_name,
            rating, flagged, hidden, title, description, group_hash, orientation, original_filename,
            thumbhash
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16, ?17, ?18, ?19,
            ?20, ?21, ?22,
            ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30,
            ?31
        )",
    )?;
    let changed = stmt.execute(params![
            img.hash,
            img.path.to_string_lossy(),
            img.filename,
            img.size_bytes,
            img.width,
            img.height,
            img.created_at,
            img.imported_at,
            img.format.as_ref().map(|f| f.as_str()),
            img.has_sidecar as i32,
            img.metadata_json,
            img.thumbnail_hash,
            img.camera_make,
            img.camera_model,
            img.lens_model,
            img.focal_length,
            img.aperture,
            img.shutter_speed,
            img.iso,
            img.latitude,
            img.longitude,
            img.location_name,
            img.rating,
            img.flagged as i32,
            img.hidden as i32,
            img.title,
            img.description,
            img.group_hash,
            img.orientation,
            img.original_filename,
            img.thumbhash,
    ])?;

    if changed > 0 {
        Ok(Some(tx.last_insert_rowid()))
    } else {
        Ok(None) // duplicate hash
    }
}

/// Batch-insert images inside a single transaction. Returns (inserted, duplicates).
pub fn batch_insert_images(
    conn: &mut Connection,
    images: &mut [Image],
) -> Result<(usize, usize), PhotonError> {
    let tx = conn.transaction()?;
    let mut inserted = 0usize;
    let mut duplicates = 0usize;

    for img in images.iter_mut() {
        match insert_image(&tx, img)? {
            Some(id) => {
                img.id = Some(id);
                inserted += 1;
            }
            None => duplicates += 1,
        }
    }

    tx.commit()?;
    Ok((inserted, duplicates))
}

// ---------------------------------------------------------------------------
// Import dedup lookups
// ---------------------------------------------------------------------------

/// Every path already in the library. Lets re-imports skip known files without hashing.
pub fn known_paths(conn: &Connection) -> Result<HashSet<String>, PhotonError> {
    let mut stmt = conn.prepare("SELECT path FROM images")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Every content hash in the library.
pub fn all_hashes(conn: &Connection) -> Result<HashSet<String>, PhotonError> {
    let mut stmt = conn.prepare("SELECT hash FROM images")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Cheap identity of a photo that doesn't require reading the whole file:
/// original file name (case-insensitive), size, capture time. The same
/// criteria Lightroom uses for "suspected duplicates".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DuplicateKey {
    pub filename: String,
    pub size_bytes: i64,
    pub created_at: Option<i64>,
}

impl DuplicateKey {
    pub fn new(filename: &str, size_bytes: i64, created_at: Option<i64>) -> Self {
        Self {
            filename: filename.to_lowercase(),
            size_bytes,
            created_at,
        }
    }
}

/// The duplicate keys of every photo in the library.
pub fn duplicate_keys(conn: &Connection) -> Result<HashSet<DuplicateKey>, PhotonError> {
    let mut stmt =
        conn.prepare("SELECT COALESCE(original_filename, filename), size_bytes, created_at FROM images")?;
    let rows = stmt.query_map([], |r| {
        Ok(DuplicateKey::new(&r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

// ---------------------------------------------------------------------------
// Image reads — positional column access for robustness
// ---------------------------------------------------------------------------

// Column indices must match IMAGE_SELECT exactly:
//  0: id           1: hash          2: path          3: filename
//  4: size_bytes   5: width         6: height        7: created_at
//  8: imported_at  9: format       10: has_sidecar  11: metadata_json
// 12: thumbnail_hash               13: camera_make  14: camera_model
// 15: lens_model  16: focal_length 17: aperture     18: shutter_speed
// 19: iso         20: latitude     21: longitude    22: location_name
// 23: rating      24: flagged      25: hidden       26: title
// 27: description 28: group_hash 29: orientation 30: original_filename
// 31: thumbhash

const IMAGE_SELECT: &str = "SELECT id, hash, path, filename, size_bytes, width, height,
            created_at, imported_at, format, has_sidecar, metadata_json, thumbnail_hash,
            camera_make, camera_model, lens_model, focal_length, aperture, shutter_speed, iso,
            latitude, longitude, location_name,
            rating, flagged, hidden, title, description, group_hash, orientation,
            original_filename, thumbhash
     FROM images";

fn row_to_image(row: &rusqlite::Row) -> rusqlite::Result<Image> {
    let format_str: Option<String> = row.get(9)?;
    Ok(Image {
        id: row.get(0)?,
        hash: row.get(1)?,
        path: PathBuf::from(row.get::<_, String>(2)?),
        filename: row.get(3)?,
        size_bytes: row.get(4)?,
        width: row.get(5)?,
        height: row.get(6)?,
        created_at: row.get(7)?,
        imported_at: row.get(8)?,
        format: format_str.map(|s| ImageFormat::from_db_str(&s)),
        has_sidecar: row.get::<_, i32>(10).unwrap_or(0) != 0,
        metadata_json: row.get(11)?,
        thumbnail_hash: row.get(12)?,
        camera_make: row.get(13)?,
        camera_model: row.get(14)?,
        lens_model: row.get(15)?,
        focal_length: row.get(16)?,
        aperture: row.get(17)?,
        shutter_speed: row.get(18)?,
        iso: row.get(19)?,
        latitude: row.get(20)?,
        longitude: row.get(21)?,
        location_name: row.get(22)?,
        rating: row.get::<_, i32>(23).unwrap_or(0),
        flagged: row.get::<_, i32>(24).unwrap_or(0) != 0,
        hidden: row.get::<_, i32>(25).unwrap_or(0) != 0,
        title: row.get(26)?,
        description: row.get(27)?,
        group_hash: row.get(28)?,
        orientation: row.get(29)?,
        original_filename: row.get(30)?,
        thumbhash: row.get(31)?,
    })
}

/// Collect rows with error logging instead of silent swallowing.
fn collect_images(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row) -> rusqlite::Result<Image>>,
) -> Vec<Image> {
    let mut result = Vec::new();
    for row in rows {
        match row {
            Ok(img) => result.push(img),
            Err(e) => log::error!("row_to_image failed: {:?}", e),
        }
    }
    result
}

pub fn get_all_images(
    conn: &Connection,
    limit: i32,
    offset: i32,
) -> Result<Vec<Image>, PhotonError> {
    let sql = format!(
        "{} ORDER BY COALESCE(created_at, imported_at) DESC LIMIT ?1 OFFSET ?2",
        IMAGE_SELECT
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit, offset], row_to_image)?;
    Ok(collect_images(rows))
}

pub fn get_images_by_year(conn: &Connection, year: i32) -> Result<Vec<Image>, PhotonError> {
    let sql = format!(
        "{} WHERE year = ?1 ORDER BY COALESCE(created_at, imported_at) DESC",
        IMAGE_SELECT
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![year], row_to_image)?;
    Ok(collect_images(rows))
}

pub fn get_images_by_month(
    conn: &Connection,
    year: i32,
    month: u32,
) -> Result<Vec<Image>, PhotonError> {
    let sql = format!(
        "{} WHERE year = ?1 AND month = ?2 ORDER BY COALESCE(created_at, imported_at) DESC",
        IMAGE_SELECT
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![year, month as i32], row_to_image)?;
    Ok(collect_images(rows))
}

pub fn get_images_by_day(
    conn: &Connection,
    year: i32,
    month: u32,
    day: u32,
) -> Result<Vec<Image>, PhotonError> {
    let sql = format!(
        "{} WHERE year = ?1 AND month = ?2 AND day = ?3 ORDER BY COALESCE(created_at, imported_at) DESC",
        IMAGE_SELECT
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![year, month as i32, day as i32], row_to_image)?;
    Ok(collect_images(rows))
}

/// A single image by id.
pub fn get_image(conn: &Connection, id: i64) -> Result<Option<Image>, PhotonError> {
    let sql = format!("{} WHERE id = ?1", IMAGE_SELECT);
    Ok(conn.query_row(&sql, params![id], row_to_image).optional()?)
}

/// Which photos a timeline shows.
#[derive(Debug, Clone, PartialEq)]
pub enum TimelineFilter {
    All,
    Day(i32, u32, u32),
    Search(String),
}

/// Timeline tiles for `filter`, newest first. Only the columns needed for layout.
pub fn timeline_items(
    conn: &Connection,
    filter: &TimelineFilter,
) -> Result<Vec<TimelineItem>, PhotonError> {
    const COLS: &str = "SELECT id, hash, created_at, width, height, orientation, thumbhash FROM images";
    const ORDER: &str = "ORDER BY COALESCE(created_at, imported_at) DESC, id DESC";

    let map = |r: &rusqlite::Row| -> rusqlite::Result<TimelineItem> {
        Ok(TimelineItem {
            id: r.get(0)?,
            hash: r.get(1)?,
            created_at: r.get(2)?,
            width: r.get(3)?,
            height: r.get(4)?,
            orientation: r.get(5)?,
            thumbhash: r.get(6)?,
        })
    };

    let items = match filter {
        TimelineFilter::All => {
            let mut stmt = conn.prepare(&format!("{COLS} WHERE hidden = 0 {ORDER}"))?;
            let rows = stmt.query_map([], map)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        TimelineFilter::Day(y, m, d) => {
            let mut stmt = conn.prepare(&format!(
                "{COLS} WHERE hidden = 0 AND year = ?1 AND month = ?2 AND day = ?3 {ORDER}"
            ))?;
            let rows = stmt.query_map(params![y, *m as i32, *d as i32], map)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        TimelineFilter::Search(text) => {
            let mut stmt = conn.prepare(&format!(
                "{COLS} WHERE hidden = 0 AND id IN (SELECT rowid FROM images_fts WHERE images_fts MATCH ?1) {ORDER}"
            ))?;
            let rows = stmt.query_map(params![fts_query(text)], map)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok(items)
}

/// Save a computed thumbhash for an image.
pub fn save_thumbhash(conn: &Connection, id: i64, thumbhash: &[u8]) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE images SET thumbhash = ?1 WHERE id = ?2",
        params![thumbhash, id],
    )?;
    Ok(())
}

/// Get all image ids and hashes that don't have a thumbhash yet (for backfilling).
pub fn get_images_missing_thumbhash(conn: &Connection) -> Result<Vec<(i64, String)>, PhotonError> {
    let mut stmt = conn.prepare("SELECT id, hash FROM images WHERE thumbhash IS NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Turn free text into a safe FTS5 query: each word becomes a quoted prefix
/// term, so input like `f/2.8` or `"` can't cause a syntax error.
fn fts_query(text: &str) -> String {
    text.split_whitespace()
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Flexible query driven by `LibraryQuery`.
pub fn search_images(conn: &Connection, query: &LibraryQuery) -> Result<Vec<Image>, PhotonError> {
    let mut conditions = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;

    if let Some(year) = query.year {
        conditions.push(format!("year = ?{}", idx));
        params_vec.push(Box::new(year));
        idx += 1;
    }
    if let Some(month) = query.month {
        conditions.push(format!("month = ?{}", idx));
        params_vec.push(Box::new(month as i32));
        idx += 1;
    }
    if let Some(day) = query.day {
        conditions.push(format!("day = ?{}", idx));
        params_vec.push(Box::new(day as i32));
        idx += 1;
    }
    if let Some((start, end)) = query.date_range {
        conditions.push(format!(
            "created_at >= ?{} AND created_at <= ?{}",
            idx,
            idx + 1
        ));
        params_vec.push(Box::new(start));
        params_vec.push(Box::new(end));
        idx += 2;
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };

    let mut sql = format!(
        "{}{} ORDER BY COALESCE(created_at, imported_at) DESC",
        IMAGE_SELECT, where_clause
    );

    if let Some(limit) = query.limit {
        sql.push_str(&format!(" LIMIT ?{}", idx));
        params_vec.push(Box::new(limit));
        idx += 1;
    }
    if let Some(offset) = query.offset {
        sql.push_str(&format!(" OFFSET ?{}", idx));
        params_vec.push(Box::new(offset));
    }

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), row_to_image)?;
    Ok(collect_images(rows))
}

/// Full-text search via FTS5.
pub fn fts_search(
    conn: &Connection,
    query_text: &str,
    limit: i32,
) -> Result<Vec<Image>, PhotonError> {
    let sql = format!(
        "{} WHERE id IN (
            SELECT rowid FROM images_fts WHERE images_fts MATCH ?1 ORDER BY rank LIMIT ?2
        )",
        IMAGE_SELECT
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![query_text, limit], row_to_image)?;
    Ok(collect_images(rows))
}

// ---------------------------------------------------------------------------
// Sidecar group queries
// ---------------------------------------------------------------------------

/// Get all images in the same sidecar group (RAW+JPG pair, edits, etc.)
pub fn get_images_in_group(
    conn: &Connection,
    group_hash: &str,
) -> Result<Vec<Image>, PhotonError> {
    let sql = format!(
        "{} WHERE group_hash = ?1 ORDER BY format ASC, filename ASC",
        IMAGE_SELECT
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![group_hash], row_to_image)?;
    Ok(collect_images(rows))
}

// ---------------------------------------------------------------------------
// Sidebar / Hierarchy
// ---------------------------------------------------------------------------

/// Photo counts per day for the whole library, newest first, in one query
/// (the `(year, month, day)` index makes this a single index scan).
/// Rows are `(year, month, day, count)`.
pub fn date_tree(conn: &Connection) -> Result<Vec<(i32, u32, u32, u32)>, PhotonError> {
    let mut stmt = conn.prepare(
        "SELECT year, month, day, COUNT(*) FROM images
         WHERE year IS NOT NULL AND year > 0 AND hidden = 0
         GROUP BY year, month, day
         ORDER BY year DESC, month DESC, day DESC",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn get_years(conn: &Connection) -> Result<Vec<(i32, u32)>, PhotonError> {
    let mut stmt = conn.prepare(
        "SELECT year, COUNT(*) FROM images
         WHERE year IS NOT NULL AND year > 0
         GROUP BY year ORDER BY year DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        let y: i32 = row.get(0)?;
        let c: u32 = row.get(1)?;
        Ok((y, c))
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn get_months_in_year(conn: &Connection, year: i32) -> Result<Vec<(u32, u32)>, PhotonError> {
    let mut stmt = conn.prepare(
        "SELECT month, COUNT(*) FROM images
         WHERE year = ?1
         GROUP BY month ORDER BY month DESC",
    )?;
    let rows = stmt.query_map(params![year], |row| {
        let m: u32 = row.get(0)?;
        let c: u32 = row.get(1)?;
        Ok((m, c))
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn get_days_in_month(
    conn: &Connection,
    year: i32,
    month: u32,
) -> Result<Vec<(u32, u32)>, PhotonError> {
    let mut stmt = conn.prepare(
        "SELECT day, COUNT(*) FROM images
         WHERE year = ?1 AND month = ?2
         GROUP BY day ORDER BY day DESC",
    )?;
    let rows = stmt.query_map(params![year, month as i32], |row| {
        let d: u32 = row.get(0)?;
        let c: u32 = row.get(1)?;
        Ok((d, c))
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Total image count (fast, for status bar).
pub fn image_count(conn: &Connection) -> Result<i64, PhotonError> {
    Ok(conn.query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0))?)
}

/// Get one representative (cover) image per month in a given year.
/// Returns Vec<(month, count, cover_image)>.
pub fn get_month_covers(
    conn: &Connection,
    year: i32,
) -> Result<Vec<(u32, u32, Image)>, PhotonError> {
    let months = get_months_in_year(conn, year)?;
    let mut result = Vec::new();

    for (month, count) in months {
        let sql = format!(
            "{} WHERE year = ?1 AND month = ?2 ORDER BY created_at ASC LIMIT 1",
            IMAGE_SELECT
        );
        let mut stmt = conn.prepare(&sql)?;
        if let Ok(img) = stmt.query_row(params![year, month as i32], row_to_image) {
            result.push((month, count, img));
        }
    }
    Ok(result)
}

/// Get one representative (cover) image per day in a given month.
/// Returns Vec<(day, count, cover_image)>.
pub fn get_day_covers(
    conn: &Connection,
    year: i32,
    month: u32,
) -> Result<Vec<(u32, u32, Image)>, PhotonError> {
    let days = get_days_in_month(conn, year, month)?;
    let mut result = Vec::new();

    for (day, count) in days {
        let sql = format!(
            "{} WHERE year = ?1 AND month = ?2 AND day = ?3 ORDER BY created_at ASC LIMIT 1",
            IMAGE_SELECT
        );
        let mut stmt = conn.prepare(&sql)?;
        if let Ok(img) = stmt.query_row(params![year, month as i32, day as i32], row_to_image) {
            result.push((day, count, img));
        }
    }
    Ok(result)
}

/// Load Preferences from library_meta. Returns defaults if not stored.
pub fn load_preferences(conn: &Connection) -> crate::models::Preferences {
    match get_meta(conn, "preferences") {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
        _ => crate::models::Preferences::default(),
    }
}

/// Save Preferences to library_meta.
pub fn save_preferences(
    conn: &Connection,
    prefs: &crate::models::Preferences,
) -> Result<(), PhotonError> {
    let json = serde_json::to_string(prefs).map_err(|e| PhotonError::Other(e.to_string()))?;
    set_meta(conn, "preferences", &json)
}

// ---------------------------------------------------------------------------
// Edit history
// ---------------------------------------------------------------------------

/// Record an edit action (opening a file in an external tool).
pub fn record_edit(
    conn: &Connection,
    image_id: i64,
    tool_name: &str,
    sidecar_path: Option<&str>,
) -> Result<i64, PhotonError> {
    conn.execute(
        "INSERT INTO edit_history (image_id, tool_name, sidecar_path)
         VALUES (?1, ?2, ?3)",
        params![image_id, tool_name, sidecar_path],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Update an edit record with sidecar path (after external tool saves).
pub fn update_edit_sidecar(
    conn: &Connection,
    edit_id: i64,
    sidecar_path: &str,
) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE edit_history SET sidecar_path = ?1 WHERE id = ?2",
        params![sidecar_path, edit_id],
    )?;
    Ok(())
}

/// Mark an image as having a sidecar.
pub fn set_has_sidecar(conn: &Connection, image_id: i64, has: bool) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE images SET has_sidecar = ?1 WHERE id = ?2",
        params![has as i32, image_id],
    )?;
    Ok(())
}

/// Get edit history for an image.
pub fn get_edit_history(
    conn: &Connection,
    image_id: i64,
) -> Result<Vec<crate::models::EditRecord>, PhotonError> {
    let mut stmt = conn.prepare(
        "SELECT id, image_id, tool_name, operation_type, sidecar_path, edited_at
         FROM edit_history WHERE image_id = ?1 ORDER BY edited_at DESC",
    )?;
    let rows = stmt.query_map(params![image_id], |row| {
        Ok(crate::models::EditRecord {
            id: row.get(0)?,
            image_id: row.get(1)?,
            tool_name: row.get(2)?,
            operation_type: row.get(3)?,
            sidecar_path: row.get(4)?,
            edited_at: row.get(5)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

// ---------------------------------------------------------------------------
// Import batch tracking
// ---------------------------------------------------------------------------

pub fn insert_import_batch(conn: &Connection, batch: &ImportBatch) -> Result<i64, PhotonError> {
    conn.execute(
        "INSERT INTO import_batches (source_type, source_path, total_files, imported_count, error_count, status, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            batch.source_type,
            batch.source_path,
            batch.total_files,
            batch.imported_count,
            batch.error_count,
            batch.status,
            batch.started_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn complete_import_batch(conn: &Connection, batch: &ImportBatch) -> Result<(), PhotonError> {
    if let Some(id) = batch.id {
        conn.execute(
            "UPDATE import_batches
             SET imported_count = ?1, duplicate_count = ?2, error_count = ?3,
                 status = ?4, completed_at = ?5
             WHERE id = ?6",
            params![
                batch.imported_count,
                batch.duplicate_count,
                batch.error_count,
                batch.status,
                chrono::Utc::now().timestamp(),
                id,
            ],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

pub fn create_tag(conn: &Connection, name: &str, color: Option<&str>) -> Result<i64, PhotonError> {
    conn.execute(
        "INSERT OR IGNORE INTO tags (name, color) VALUES (?1, ?2)",
        params![name, color],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn tag_image(conn: &Connection, image_id: i64, tag_id: i64) -> Result<(), PhotonError> {
    conn.execute(
        "INSERT OR IGNORE INTO image_tags (image_id, tag_id) VALUES (?1, ?2)",
        params![image_id, tag_id],
    )?;
    Ok(())
}

pub fn untag_image(conn: &Connection, image_id: i64, tag_id: i64) -> Result<(), PhotonError> {
    conn.execute(
        "DELETE FROM image_tags WHERE image_id = ?1 AND tag_id = ?2",
        params![image_id, tag_id],
    )?;
    Ok(())
}

pub fn get_all_tags(conn: &Connection) -> Result<Vec<Tag>, PhotonError> {
    let mut stmt = conn.prepare("SELECT id, name, color, is_category FROM tags ORDER BY name")?;
    let rows = stmt.query_map([], |row| {
        Ok(Tag {
            id: row.get(0)?,
            name: row.get(1)?,
            color: row.get(2)?,
            is_category: row.get::<_, i32>(3).unwrap_or(0) != 0,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn get_tags_for_image(conn: &Connection, image_id: i64) -> Result<Vec<Tag>, PhotonError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.color, t.is_category
         FROM tags t
         JOIN image_tags it ON it.tag_id = t.id
         WHERE it.image_id = ?1
         ORDER BY t.name",
    )?;
    let rows = stmt.query_map(params![image_id], |row| {
        Ok(Tag {
            id: row.get(0)?,
            name: row.get(1)?,
            color: row.get(2)?,
            is_category: row.get::<_, i32>(3).unwrap_or(0) != 0,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

// ---------------------------------------------------------------------------
// Library metadata (key-value)
// ---------------------------------------------------------------------------

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), PhotonError> {
    conn.execute(
        "INSERT OR REPLACE INTO library_meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>, PhotonError> {
    let result = conn.query_row(
        "SELECT value FROM library_meta WHERE key = ?1",
        params![key],
        |r| r.get(0),
    );
    match result {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(PhotonError::Db(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn insert(conn: &mut Connection, name: &str, created_at: i64, dims: (u32, u32), orientation: u16) {
        let mut img = Image::new(PathBuf::from(format!("/p/{name}")), name.to_string(), 10);
        img.created_at = Some(created_at);
        img.width = Some(dims.0);
        img.height = Some(dims.1);
        img.orientation = Some(orientation);
        batch_insert_images(conn, &mut [img]).unwrap();
    }

    #[test]
    fn timeline_items_filter_and_order() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        insert(&mut conn, "old.jpg", 1_600_000_000, (6000, 4000), 1); // 2020-09-13
        insert(&mut conn, "new.jpg", 1_700_000_000, (4000, 6000), 1);
        insert(&mut conn, "rotated.jpg", 1_700_000_100, (6000, 4000), 6);

        let all = timeline_items(&conn, &TimelineFilter::All).unwrap();
        let hashes: Vec<_> = all.iter().map(|i| i.hash.as_str()).collect();
        assert_eq!(hashes, ["rotated.jpg", "new.jpg", "old.jpg"]);
        assert!((all[0].display_aspect() - 2.0 / 3.0).abs() < 1e-9); // swapped by orientation

        let day = timeline_items(&conn, &TimelineFilter::Day(2020, 9, 13)).unwrap();
        assert_eq!(day.len(), 1);

        let found = timeline_items(&conn, &TimelineFilter::Search("rot".into())).unwrap();
        assert_eq!(found.len(), 1);
        // FTS syntax characters must not error out.
        timeline_items(&conn, &TimelineFilter::Search("f/2.8 \"(".into())).unwrap();

        let id = all[0].id;
        assert_eq!(get_image(&conn, id).unwrap().unwrap().filename, "rotated.jpg");
    }

    #[test]
    fn date_tree_counts_per_day_newest_first() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        insert(&mut conn, "a", 1_600_000_000, (3, 2), 1); // 2020-09-13
        insert(&mut conn, "b", 1_600_000_100, (3, 2), 1); // same day
        insert(&mut conn, "c", 1_700_000_000, (3, 2), 1); // 2023-11-14

        assert_eq!(
            date_tree(&conn).unwrap(),
            vec![(2023, 11, 14, 1), (2020, 9, 13, 2)]
        );
    }

    #[test]
    fn duplicate_keys_use_original_filename_case_insensitively() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        let mut img = Image::new(PathBuf::from("/lib/IMG_1_1.JPG"), "h".into(), 42);
        img.original_filename = Some("IMG_1.JPG".into());
        img.created_at = Some(7);
        batch_insert_images(&mut conn, &mut [img]).unwrap();

        let keys = duplicate_keys(&conn).unwrap();
        assert!(keys.contains(&DuplicateKey::new("img_1.jpg", 42, Some(7))));
        assert!(!keys.contains(&DuplicateKey::new("img_1.jpg", 43, Some(7))));
    }
}
