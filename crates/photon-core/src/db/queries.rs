//! All database queries: inserts, lookups, hierarchical browsing, FTS search, tags.

use crate::error::PhotonError;
use crate::models::{
    Album, Event, Image, ImageFormat, ImportBatch, LibraryQuery, Tag, TimelineItem,
};
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
            img.flagged,
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

/// Insert an image, preserving its explicit `id` if present (for restore from trash/undo).
pub fn restore_image(tx: &Transaction, img: &Image) -> Result<Option<i64>, PhotonError> {
    if let Some(id) = img.id {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO images (
                id, hash, path, filename, size_bytes, width, height,
                created_at, imported_at, format, has_sidecar, metadata_json, thumbnail_hash,
                camera_make, camera_model, lens_model, focal_length, aperture, shutter_speed, iso,
                latitude, longitude, location_name,
                rating, flagged, hidden, title, description, group_hash, orientation, original_filename,
                thumbhash
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18, ?19, ?20,
                ?21, ?22, ?23,
                ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31,
                ?32
            )",
        )?;
        let changed = stmt.execute(params![
            id,
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
            img.flagged,
            img.hidden as i32,
            img.title,
            img.description,
            img.group_hash,
            img.orientation,
            img.original_filename,
            img.thumbhash,
        ])?;
        if changed > 0 {
            Ok(Some(id))
        } else {
            Ok(None)
        }
    } else {
        insert_image(tx, img)
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
// 31: thumbhash 32: xmp_mtime 33: missing

const IMAGE_SELECT: &str = "SELECT id, hash, path, filename, size_bytes, width, height,
            created_at, imported_at, format, has_sidecar, metadata_json, thumbnail_hash,
            camera_make, camera_model, lens_model, focal_length, aperture, shutter_speed, iso,
            latitude, longitude, location_name,
            rating, flagged, hidden, title, description, group_hash, orientation,
            original_filename, thumbhash, xmp_mtime, COALESCE(missing, 0) as missing
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
        flagged: row.get::<_, i32>(24).unwrap_or(0),
        hidden: row.get::<_, i32>(25).unwrap_or(0) != 0,
        title: row.get(26)?,
        description: row.get(27)?,
        group_hash: row.get(28)?,
        orientation: row.get(29)?,
        original_filename: row.get(30)?,
        thumbhash: row.get(31)?,
        xmp_mtime: row.get(32)?,
        missing: row.get::<_, i32>(33).unwrap_or(0) != 0,
        is_video: false,
        duration: None,
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

/// Remove images from the library (the files are not touched). Tags and
/// edit history go with them (ON DELETE CASCADE); the FTS index is kept in
/// sync by its trigger. Returns how many rows were removed.
pub fn delete_images(conn: &mut Connection, ids: &[i64]) -> Result<usize, PhotonError> {
    let tx = conn.transaction()?;
    let mut removed = 0;
    {
        let mut stmt = tx.prepare_cached("DELETE FROM images WHERE id = ?1")?;
        for id in ids {
            removed += stmt.execute(params![id])?;
        }
    }
    tx.commit()?;
    Ok(removed)
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
    Tag(String),
    Album(i64),
    Event(i64),
    Missing,
}

/// Which star ratings a timeline shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RatingFilter {
    #[default]
    Any,
    /// Rated at least this many stars (1–5).
    AtLeast(i32),
    /// No stars yet (rating 0).
    Unrated,
}

/// Timeline tiles for `filter`, newest first. Only the columns needed for layout.
pub fn timeline_items(
    conn: &Connection,
    filter: &TimelineFilter,
) -> Result<Vec<TimelineItem>, PhotonError> {
    timeline_items_with_cull(conn, filter, RatingFilter::Any, None)
}

fn parse_duration_from_meta(json_str: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    if let Some(s) = v.get("duration").and_then(|d| d.as_str()) {
        return Some(s.to_string());
    }
    if let Some(secs) = v.get("duration_seconds").and_then(|d| d.as_f64()).or_else(|| v.get("duration").and_then(|d| d.as_f64())) {
        let total_secs = secs.round() as u64;
        let mins = total_secs / 60;
        let rem_secs = total_secs % 60;
        return Some(format!("{}:{:02}", mins, rem_secs));
    }
    None
}

/// Timeline tiles for `filter`, narrowed by star `rating` and flag (-1 reject, 0 unflagged, 1 pick).
pub fn timeline_items_with_cull(
    conn: &Connection,
    filter: &TimelineFilter,
    rating: RatingFilter,
    flag: Option<i32>,
) -> Result<Vec<TimelineItem>, PhotonError> {
    const COLS: &str = "SELECT id, hash, created_at, width, height, orientation, thumbhash, rating, flagged, format, metadata_json, COALESCE(missing, 0) FROM images";
    const ORDER: &str = "ORDER BY COALESCE(created_at, imported_at) DESC, id DESC";

    let map = |r: &rusqlite::Row| -> rusqlite::Result<TimelineItem> {
        let format_str: Option<String> = r.get(9)?;
        let is_video = format_str
            .as_deref()
            .map(ImageFormat::from_db_str)
            .map_or(false, |f| f.is_video());
        let meta_json: Option<String> = r.get(10)?;
        let duration = meta_json.as_deref().and_then(parse_duration_from_meta);
        let missing_val: i32 = r.get(11).unwrap_or(0);
        Ok(TimelineItem {
            id: r.get(0)?,
            hash: r.get(1)?,
            created_at: r.get(2)?,
            width: r.get(3)?,
            height: r.get(4)?,
            orientation: r.get(5)?,
            thumbhash: r.get(6)?,
            rating: r.get::<_, i32>(7).unwrap_or(0),
            flagged: r.get::<_, i32>(8).unwrap_or(0),
            is_video,
            duration,
            missing: missing_val != 0,
        })
    };

    let mut extra = String::new();
    let mut extra_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    match rating {
        RatingFilter::Any => {}
        RatingFilter::AtLeast(r) => {
            extra.push_str(" AND rating >= ?");
            extra_params.push(Box::new(r));
        }
        RatingFilter::Unrated => extra.push_str(" AND COALESCE(rating, 0) = 0"),
    }
    if let Some(f) = flag {
        extra.push_str(" AND flagged = ?");
        extra_params.push(Box::new(f));
    }

    let items = match filter {
        TimelineFilter::All => {
            let sql = format!("{COLS} WHERE hidden = 0 {extra} {ORDER}");
            let mut stmt = conn.prepare(&sql)?;
            let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                extra_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), map)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        TimelineFilter::Day(y, m, d) => {
            let sql = format!(
                "{COLS} WHERE hidden = 0 AND year = ?1 AND month = ?2 AND day = ?3 {extra} {ORDER}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
                Box::new(*y),
                Box::new(*m as i32),
                Box::new(*d as i32),
            ];
            all_params.extend(extra_params);
            let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                all_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), map)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        TimelineFilter::Tag(tag) => {
            let sql = format!(
                "{COLS} WHERE hidden = 0 AND id IN (
                    SELECT it.image_id FROM image_tags it
                    JOIN tags t ON t.id = it.tag_id
                    WHERE t.name = ?1 COLLATE NOCASE
                ) {extra} {ORDER}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(tag.clone())];
            all_params.extend(extra_params);
            let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                all_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), map)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        TimelineFilter::Search(text) => {
            let trimmed = text.trim();
            if let Some(tag_query) = trimmed.strip_prefix("tag:") {
                let sql = format!(
                    "{COLS} WHERE hidden = 0 AND id IN (
                        SELECT it.image_id FROM image_tags it
                        JOIN tags t ON t.id = it.tag_id
                        WHERE t.name LIKE ?1
                    ) {extra} {ORDER}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> =
                    vec![Box::new(format!("%{}%", tag_query.trim()))];
                all_params.extend(extra_params);
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    all_params.iter().map(|p| p.as_ref()).collect();
                let rows = stmt.query_map(params_refs.as_slice(), map)?;
                rows.collect::<Result<Vec<_>, _>>()?
            } else if let Some(cam_query) = trimmed
                .strip_prefix("camera:")
                .or_else(|| trimmed.strip_prefix("make:"))
            {
                let sql = format!(
                    "{COLS} WHERE hidden = 0 AND (camera_make LIKE ?1 OR camera_model LIKE ?1) {extra} {ORDER}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> =
                    vec![Box::new(format!("%{}%", cam_query.trim()))];
                all_params.extend(extra_params);
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    all_params.iter().map(|p| p.as_ref()).collect();
                let rows = stmt.query_map(params_refs.as_slice(), map)?;
                rows.collect::<Result<Vec<_>, _>>()?
            } else if let Some(lens_query) = trimmed.strip_prefix("lens:") {
                let sql = format!(
                    "{COLS} WHERE hidden = 0 AND lens_model LIKE ?1 {extra} {ORDER}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> =
                    vec![Box::new(format!("%{}%", lens_query.trim()))];
                all_params.extend(extra_params);
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    all_params.iter().map(|p| p.as_ref()).collect();
                let rows = stmt.query_map(params_refs.as_slice(), map)?;
                rows.collect::<Result<Vec<_>, _>>()?
            } else if let Some(flag_query) = trimmed.strip_prefix("is:") {
                let f_val = match flag_query.trim().to_lowercase().as_str() {
                    "pick" | "picked" => 1,
                    "reject" | "rejected" => -1,
                    _ => 0,
                };
                let sql = format!("{COLS} WHERE hidden = 0 AND flagged = ?1 {extra} {ORDER}");
                let mut stmt = conn.prepare(&sql)?;
                let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(f_val)];
                all_params.extend(extra_params);
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    all_params.iter().map(|p| p.as_ref()).collect();
                let rows = stmt.query_map(params_refs.as_slice(), map)?;
                rows.collect::<Result<Vec<_>, _>>()?
            } else {
                let fts = fts_query(trimmed);
                let tag_pattern = format!("%{}%", trimmed);
                let sql = format!(
                    "{COLS} WHERE hidden = 0 AND (
                        id IN (SELECT rowid FROM images_fts WHERE images_fts MATCH ?1)
                        OR id IN (SELECT it.image_id FROM image_tags it JOIN tags t ON t.id = it.tag_id WHERE t.name LIKE ?2)
                    ) {extra} {ORDER}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> =
                    vec![Box::new(fts), Box::new(tag_pattern)];
                all_params.extend(extra_params);
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    all_params.iter().map(|p| p.as_ref()).collect();
                let rows = stmt.query_map(params_refs.as_slice(), map)?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        }
        TimelineFilter::Album(album_id) => {
            let sql = format!(
                "{COLS}
                 WHERE hidden = 0 AND id IN (
                     SELECT image_id FROM album_images WHERE album_id = ?1
                 ) {extra} {ORDER}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(*album_id)];
            all_params.extend(extra_params);
            let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                all_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), map)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        TimelineFilter::Event(event_id) => {
            let sql = format!(
                "{COLS} WHERE hidden = 0 AND created_at >= (SELECT start_date FROM events WHERE id = ?1)
                                      AND created_at <= (SELECT end_date FROM events WHERE id = ?1)
                 {extra} {ORDER}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(*event_id)];
            all_params.extend(extra_params);
            let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                all_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), map)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        TimelineFilter::Missing => {
            let sql = format!("{COLS} WHERE hidden = 0 AND missing = 1 {extra} {ORDER}");
            let mut stmt = conn.prepare(&sql)?;
            let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                extra_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), map)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok(items)
}

/// Update rating (0..=5) of an image.
pub fn set_rating(conn: &Connection, id: i64, rating: i32) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE images SET rating = ?1 WHERE id = ?2",
        params![rating.clamp(0, 5), id],
    )?;
    Ok(())
}

/// Update flag (-1 = rejected, 0 = unflagged, 1 = pick) of an image.
pub fn set_flag(conn: &Connection, id: i64, flag: i32) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE images SET flagged = ?1 WHERE id = ?2",
        params![flag.clamp(-1, 1), id],
    )?;
    Ok(())
}

/// Batch update rating for multiple photos in a single transaction.
pub fn batch_set_rating(conn: &mut Connection, ids: &[i64], rating: i32) -> Result<(), PhotonError> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached("UPDATE images SET rating = ?1 WHERE id = ?2")?;
        let r = rating.clamp(0, 5);
        for id in ids {
            stmt.execute(params![r, id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Batch update flag for multiple photos in a single transaction.
pub fn batch_set_flag(conn: &mut Connection, ids: &[i64], flag: i32) -> Result<(), PhotonError> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached("UPDATE images SET flagged = ?1 WHERE id = ?2")?;
        let f = flag.clamp(-1, 1);
        for id in ids {
            stmt.execute(params![f, id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Update orientation for a single image.
pub fn set_orientation(conn: &Connection, id: i64, orientation: u16) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE images SET orientation = ?1 WHERE id = ?2",
        params![orientation, id],
    )?;
    Ok(())
}

/// Batch update orientation for multiple photos in a single transaction.
pub fn batch_set_orientation(
    conn: &mut Connection,
    updates: &[(i64, u16)],
) -> Result<(), PhotonError> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached("UPDATE images SET orientation = ?1 WHERE id = ?2")?;
        for (id, orientation) in updates {
            stmt.execute(params![orientation, id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Save a computed thumbhash for an image.
pub fn save_thumbhash(conn: &Connection, id: i64, thumbhash: &[u8]) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE images SET thumbhash = ?1 WHERE id = ?2",
        params![thumbhash, id],
    )?;
    Ok(())
}

/// Save many thumbhashes in one transaction (one fsync instead of thousands).
pub fn save_thumbhashes(conn: &mut Connection, hashes: &[(i64, Vec<u8>)]) -> Result<(), PhotonError> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached("UPDATE images SET thumbhash = ?1 WHERE id = ?2")?;
        for (id, thumbhash) in hashes {
            stmt.execute(params![thumbhash, id])?;
        }
    }
    tx.commit()?;
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

/// `selected` grouped by shot, in selection order: each shot's versions (all
/// files of its RAW+JPG group, or just the photo if it has none) and the ids
/// of the versions that were selected.
pub fn shots_of(
    conn: &Connection,
    selected: Vec<Image>,
) -> Result<Vec<(Vec<Image>, HashSet<i64>)>, PhotonError> {
    let mut shots: Vec<(Vec<Image>, HashSet<i64>)> = Vec::new();
    let mut group_index: std::collections::HashMap<String, usize> = Default::default();
    for img in selected {
        let id = img.id.unwrap_or_default();
        match img.group_hash.clone() {
            Some(group) => {
                let i = match group_index.get(&group) {
                    Some(&i) => i,
                    None => {
                        shots.push((get_images_in_group(conn, &group)?, HashSet::new()));
                        group_index.insert(group, shots.len() - 1);
                        shots.len() - 1
                    }
                };
                shots[i].1.insert(id);
            }
            None => shots.push((vec![img], HashSet::from([id]))),
        }
    }
    Ok(shots)
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

/// The id of tag `name`, creating it if needed. (Unlike [`create_tag`],
/// correct when the tag already exists.)
pub fn ensure_tag(conn: &Connection, name: &str) -> Result<i64, PhotonError> {
    conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", params![name])?;
    Ok(conn.query_row("SELECT id FROM tags WHERE name = ?1", params![name], |r| r.get(0))?)
}

/// The id of the photo at `path`, if it is in the library.
pub fn image_id_by_path(conn: &Connection, path: &str) -> Result<Option<i64>, PhotonError> {
    Ok(conn
        .query_row("SELECT id FROM images WHERE path = ?1", params![path], |r| r.get(0))
        .optional()?)
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

/// Get all tags that are assigned to at least one photo, with their photo counts.
pub fn get_tags_with_counts(conn: &Connection) -> Result<Vec<(Tag, u32)>, PhotonError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.color, t.is_category, COUNT(it.image_id) as count
         FROM tags t
         JOIN image_tags it ON it.tag_id = t.id
         JOIN images i ON i.id = it.image_id
         WHERE i.hidden = 0
         GROUP BY t.id
         HAVING count > 0
         ORDER BY count DESC, t.name ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            Tag {
                id: row.get(0)?,
                name: row.get(1)?,
                color: row.get(2)?,
                is_category: row.get::<_, i32>(3).unwrap_or(0) != 0,
            },
            row.get::<_, u32>(4)?,
        ))
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Update title of an image.
pub fn set_title(conn: &Connection, id: i64, title: &str) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE images SET title = ?1 WHERE id = ?2",
        params![title, id],
    )?;
    Ok(())
}

/// Update description/caption of an image.
pub fn set_description(conn: &Connection, id: i64, description: &str) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE images SET description = ?1 WHERE id = ?2",
        params![description, id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Albums (manual collections)
// ---------------------------------------------------------------------------

pub fn create_album(conn: &Connection, name: &str) -> Result<i64, PhotonError> {
    conn.execute("INSERT INTO albums (name) VALUES (?1)", params![name])?;
    Ok(conn.last_insert_rowid())
}

pub fn rename_album(conn: &Connection, id: i64, new_name: &str) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE albums SET name = ?1 WHERE id = ?2",
        params![new_name, id],
    )?;
    Ok(())
}

pub fn delete_album(conn: &Connection, id: i64) -> Result<(), PhotonError> {
    conn.execute("DELETE FROM albums WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn get_album(conn: &Connection, id: i64) -> Result<Option<Album>, PhotonError> {
    let mut stmt = conn.prepare("SELECT id, name, created_at, cover_image_id FROM albums WHERE id = ?1")?;
    let album = stmt
        .query_row(params![id], |row| {
            Ok(Album {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
                cover_image_id: row.get(3)?,
            })
        })
        .optional()?;
    Ok(album)
}

pub fn get_all_albums_with_counts(conn: &Connection) -> Result<Vec<(Album, u32)>, PhotonError> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.created_at, a.cover_image_id, COUNT(i.id) as count
         FROM albums a
         LEFT JOIN album_images ai ON ai.album_id = a.id
         LEFT JOIN images i ON i.id = ai.image_id AND i.hidden = 0
         GROUP BY a.id
         ORDER BY a.name COLLATE NOCASE ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            Album {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
                cover_image_id: row.get(3)?,
            },
            row.get::<_, u32>(4)?,
        ))
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn add_images_to_album(
    conn: &mut Connection,
    album_id: i64,
    image_ids: &[i64],
) -> Result<usize, PhotonError> {
    let tx = conn.transaction()?;
    let mut added = 0;
    {
        let max_pos: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(position), -1) FROM album_images WHERE album_id = ?1",
                params![album_id],
                |r| r.get(0),
            )
            .unwrap_or(-1);
        let mut pos = max_pos + 1;

        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO album_images (album_id, image_id, position) VALUES (?1, ?2, ?3)",
        )?;
        for &img_id in image_ids {
            let rows = stmt.execute(params![album_id, img_id, pos])?;
            if rows > 0 {
                added += 1;
                pos += 1;
            }
        }

        // If cover_image_id is NULL and we added photos, set cover to the first one
        let has_cover: Option<Option<i64>> = tx
            .query_row(
                "SELECT cover_image_id FROM albums WHERE id = ?1",
                params![album_id],
                |r| r.get(0),
            )
            .optional()?;
        if matches!(has_cover, Some(None)) {
            if let Some(&first_id) = image_ids.first() {
                tx.execute(
                    "UPDATE albums SET cover_image_id = ?1 WHERE id = ?2",
                    params![first_id, album_id],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(added)
}

pub fn remove_images_from_album(
    conn: &mut Connection,
    album_id: i64,
    image_ids: &[i64],
) -> Result<usize, PhotonError> {
    let tx = conn.transaction()?;
    let mut removed = 0;
    {
        let mut stmt =
            tx.prepare_cached("DELETE FROM album_images WHERE album_id = ?1 AND image_id = ?2")?;
        for &img_id in image_ids {
            removed += stmt.execute(params![album_id, img_id])?;
        }

        // Check if current cover was removed
        let cover: Option<Option<i64>> = tx
            .query_row(
                "SELECT cover_image_id FROM albums WHERE id = ?1",
                params![album_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(Some(cov_id)) = cover {
            if image_ids.contains(&cov_id) {
                let next_cover: Option<i64> = tx
                    .query_row(
                        "SELECT image_id FROM album_images WHERE album_id = ?1 ORDER BY position ASC LIMIT 1",
                        params![album_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                tx.execute(
                    "UPDATE albums SET cover_image_id = ?1 WHERE id = ?2",
                    params![next_cover, album_id],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(removed)
}

pub fn set_album_cover(
    conn: &Connection,
    album_id: i64,
    cover_image_id: Option<i64>,
) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE albums SET cover_image_id = ?1 WHERE id = ?2",
        params![cover_image_id, album_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Named events
// ---------------------------------------------------------------------------

pub fn create_event(
    conn: &Connection,
    name: &str,
    start_date: i64,
    end_date: i64,
    comment: Option<&str>,
) -> Result<i64, PhotonError> {
    conn.execute(
        "INSERT INTO events (name, start_date, end_date, comment) VALUES (?1, ?2, ?3, ?4)",
        params![name, start_date, end_date, comment],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_event(
    conn: &Connection,
    id: i64,
    name: &str,
    start_date: i64,
    end_date: i64,
    comment: Option<&str>,
) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE events SET name = ?1, start_date = ?2, end_date = ?3, comment = ?4 WHERE id = ?5",
        params![name, start_date, end_date, comment, id],
    )?;
    Ok(())
}

pub fn delete_event(conn: &Connection, id: i64) -> Result<(), PhotonError> {
    conn.execute("DELETE FROM events WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn get_event(conn: &Connection, id: i64) -> Result<Option<Event>, PhotonError> {
    let mut stmt =
        conn.prepare("SELECT id, name, start_date, end_date, comment FROM events WHERE id = ?1")?;
    let ev = stmt
        .query_row(params![id], |row| {
            Ok(Event {
                id: row.get(0)?,
                name: row.get(1)?,
                start_date: row.get(2)?,
                end_date: row.get(3)?,
                comment: row.get(4)?,
            })
        })
        .optional()?;
    Ok(ev)
}

pub fn get_all_events_with_counts(conn: &Connection) -> Result<Vec<(Event, u32)>, PhotonError> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.name, e.start_date, e.end_date, e.comment, COUNT(i.id) as count
         FROM events e
         LEFT JOIN images i ON i.created_at >= e.start_date AND i.created_at <= e.end_date AND i.hidden = 0
         GROUP BY e.id
         ORDER BY e.start_date DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            Event {
                id: row.get(0)?,
                name: row.get(1)?,
                start_date: row.get(2)?,
                end_date: row.get(3)?,
                comment: row.get(4)?,
            },
            row.get::<_, u32>(5)?,
        ))
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn get_event_for_timestamp(
    conn: &Connection,
    timestamp: i64,
) -> Result<Option<Event>, PhotonError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, start_date, end_date, comment FROM events
         WHERE start_date <= ?1 AND end_date >= ?1
         ORDER BY (end_date - start_date) ASC LIMIT 1",
    )?;
    let ev = stmt
        .query_row(params![timestamp], |row| {
            Ok(Event {
                id: row.get(0)?,
                name: row.get(1)?,
                start_date: row.get(2)?,
                end_date: row.get(3)?,
                comment: row.get(4)?,
            })
        })
        .optional()?;
    Ok(ev)
}

pub fn day_bounds(year: i32, month: u32, day: u32) -> (i64, i64) {
    if let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, day) {
        let start = date
            .and_hms_opt(0, 0, 0)
            .map(|d| d.and_utc().timestamp())
            .unwrap_or(0);
        let end = date
            .and_hms_opt(23, 59, 59)
            .map(|d| d.and_utc().timestamp())
            .unwrap_or(start + 86399);
        (start, end)
    } else {
        (0, 0)
    }
}

pub fn get_event_for_day(
    conn: &Connection,
    year: i32,
    month: u32,
    day: u32,
) -> Result<Option<Event>, PhotonError> {
    let (day_start, day_end) = day_bounds(year, month, day);
    let mut stmt = conn.prepare(
        "SELECT id, name, start_date, end_date, comment FROM events
         WHERE start_date <= ?2 AND end_date >= ?1
         ORDER BY (end_date - start_date) ASC LIMIT 1",
    )?;
    let ev = stmt
        .query_row(params![day_start, day_end], |row| {
            Ok(Event {
                id: row.get(0)?,
                name: row.get(1)?,
                start_date: row.get(2)?,
                end_date: row.get(3)?,
                comment: row.get(4)?,
            })
        })
        .optional()?;
    Ok(ev)
}

pub fn name_day_event(
    conn: &Connection,
    name: &str,
    year: i32,
    month: u32,
    day: u32,
) -> Result<i64, PhotonError> {
    let (day_start, day_end) = day_bounds(year, month, day);
    if let Some(existing) = get_event_for_day(conn, year, month, day)? {
        update_event(
            conn,
            existing.id,
            name,
            existing.start_date,
            existing.end_date,
            existing.comment.as_deref(),
        )?;
        Ok(existing.id)
    } else {
        create_event(conn, name, day_start, day_end, None)
    }
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

// ---------------------------------------------------------------------------
// R-1: XMP Sync
// ---------------------------------------------------------------------------

pub fn set_xmp_mtime(conn: &Connection, id: i64, mtime: i64) -> Result<(), PhotonError> {
    conn.execute("UPDATE images SET xmp_mtime = ?1 WHERE id = ?2", params![mtime, id])?;
    Ok(())
}

pub fn get_images_with_stale_xmp(conn: &Connection, paths: &[&str]) -> Result<Vec<(i64, String, Option<i64>)>, PhotonError> {
    let mut result = Vec::new();
    for chunk in paths.chunks(1000) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!("SELECT id, path, xmp_mtime FROM images WHERE path IN ({})", placeholders);
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = chunk.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        for r in rows {
            result.push(r?);
        }
    }
    Ok(result)
}

/// Apply what an XMP sidecar says (`xmp`) to image `id`, and record the
/// sidecar's mtime so it isn't read again until something else changes it.
pub fn update_from_xmp(
    conn: &mut Connection,
    id: i64,
    xmp: &crate::models::XmpReadResult,
    xmp_mtime: i64,
) -> Result<(), PhotonError> {
    let tx = conn.transaction()?;

    if let Some(r) = xmp.rating {
        tx.execute("UPDATE images SET rating = ?1 WHERE id = ?2", params![r.clamp(0, 5) as i32, id])?;
    }
    // A reject lives in `flagged`, like one made in Photon. A plain rating
    // un-rejects, but leaves a pick alone (picks aren't in XMP).
    if xmp.rejected == Some(false) {
        tx.execute("UPDATE images SET flagged = 0 WHERE id = ?1 AND flagged = -1", params![id])?;
    }
    if xmp.rejected == Some(true) {
        tx.execute("UPDATE images SET flagged = -1 WHERE id = ?1", params![id])?;
    }
    let (title, description, orientation) = (xmp.title.as_deref(), xmp.description.as_deref(), xmp.orientation);
    if let Some(t) = title {
        tx.execute("UPDATE images SET title = ?1 WHERE id = ?2", params![t, id])?;
    }
    if let Some(d) = description {
        tx.execute("UPDATE images SET description = ?1 WHERE id = ?2", params![d, id])?;
    }
    if let Some(o) = orientation {
        tx.execute("UPDATE images SET orientation = ?1 WHERE id = ?2", params![o as u16, id])?;
    }
    tx.execute("UPDATE images SET xmp_mtime = ?1 WHERE id = ?2", params![xmp_mtime, id])?;

    // The sidecar has all of the photo's tags (every tag change in Photon is
    // written to it), so a tag missing from it was removed in another tool.
    let mut keep = Vec::with_capacity(xmp.tags.len());
    for t in &xmp.tags {
        let tag_id = ensure_tag(&tx, t)?;
        tx.execute(
            "INSERT OR IGNORE INTO image_tags (image_id, tag_id) VALUES (?1, ?2)",
            params![id, tag_id],
        )?;
        keep.push(tag_id);
    }
    let keep = serde_json::to_string(&keep).map_err(|e| PhotonError::Other(e.to_string()))?;
    tx.execute(
        "DELETE FROM image_tags WHERE image_id = ?1 AND tag_id NOT IN (SELECT value FROM json_each(?2))",
        params![id, keep],
    )?;

    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// R-10: Missing files
// ---------------------------------------------------------------------------

pub fn mark_missing(conn: &Connection, ids: &[i64], missing: bool) -> Result<(), PhotonError> {
    let val = if missing { 1 } else { 0 };
    for chunk in ids.chunks(1000) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!("UPDATE images SET missing = {} WHERE id IN ({})", val, placeholders);
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = chunk.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
        stmt.execute(params.as_slice())?;
    }
    Ok(())
}

pub fn get_missing_images(conn: &Connection) -> Result<Vec<Image>, PhotonError> {
    let sql = format!("{} WHERE missing = 1", IMAGE_SELECT);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_image)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn relink_image(conn: &Connection, id: i64, new_path: &str) -> Result<(), PhotonError> {
    conn.execute(
        "UPDATE images SET path = ?1, missing = 0 WHERE id = ?2",
        params![new_path, id],
    )?;
    Ok(())
}

pub fn get_all_images_for_integrity_check(
    conn: &Connection,
) -> Result<Vec<(i64, PathBuf, Option<i64>, bool)>, PhotonError> {
    let mut stmt = conn.prepare("SELECT id, path, xmp_mtime, missing FROM images WHERE hidden = 0")?;
    let rows = stmt.query_map([], |row| {
        let id: i64 = row.get(0)?;
        let path_str: String = row.get(1)?;
        let xmp_mtime: Option<i64> = row.get(2)?;
        let missing_val: i32 = row.get(3).unwrap_or(0);
        Ok((id, PathBuf::from(path_str), xmp_mtime, missing_val != 0))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Save export preset `name`, replacing one of the same name. `config_json`
/// is photon-import's `ExportConfig` as JSON (the core crate doesn't know it).
pub fn save_export_preset(conn: &Connection, name: &str, config_json: &str) -> Result<i64, PhotonError> {
    conn.execute(
        "INSERT INTO export_presets (name, config_json) VALUES (?1, ?2)
         ON CONFLICT(name) DO UPDATE SET config_json = excluded.config_json",
        params![name, config_json],
    )?;
    Ok(conn.query_row("SELECT id FROM export_presets WHERE name = ?1", [name], |r| r.get(0))?)
}

/// Every export preset as (id, name, config JSON), oldest first.
pub fn get_export_presets(conn: &Connection) -> Result<Vec<(i64, String, String)>, PhotonError> {
    let mut stmt = conn.prepare("SELECT id, name, config_json FROM export_presets ORDER BY id")?;
    let presets = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    Ok(presets)
}

pub fn delete_export_preset(conn: &Connection, id: i64) -> Result<(), PhotonError> {
    conn.execute("DELETE FROM export_presets WHERE id = ?1", params![id])?;
    Ok(())
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
    fn delete_images_removes_rows_tags_and_search_entries() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        insert(&mut conn, "keep.jpg", 1_600_000_000, (3, 2), 1);
        insert(&mut conn, "gone.jpg", 1_600_000_100, (3, 2), 1);
        let all = timeline_items(&conn, &TimelineFilter::All).unwrap();
        let gone = all.iter().find(|i| i.hash == "gone.jpg").unwrap().id;
        let tag = create_tag(&conn, "trip", None).unwrap();
        tag_image(&conn, gone, tag).unwrap();

        assert_eq!(delete_images(&mut conn, &[gone, 999]).unwrap(), 1);
        assert!(get_image(&conn, gone).unwrap().is_none());
        assert_eq!(timeline_items(&conn, &TimelineFilter::All).unwrap().len(), 1);
        assert!(timeline_items(&conn, &TimelineFilter::Search("gone".into())).unwrap().is_empty());
        let tagged: i64 = conn
            .query_row("SELECT COUNT(*) FROM image_tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tagged, 0);
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

    #[test]
    fn culling_rating_and_flags_filter_and_update() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        insert(&mut conn, "img1.jpg", 1_700_000_000, (3, 2), 1);
        insert(&mut conn, "img2.jpg", 1_700_000_100, (3, 2), 1);
        insert(&mut conn, "img3.jpg", 1_700_000_200, (3, 2), 1);

        let all = timeline_items(&conn, &TimelineFilter::All).unwrap();
        let id1 = all.iter().find(|i| i.hash == "img1.jpg").unwrap().id;
        let id2 = all.iter().find(|i| i.hash == "img2.jpg").unwrap().id;
        let id3 = all.iter().find(|i| i.hash == "img3.jpg").unwrap().id;

        set_rating(&conn, id1, 4).unwrap();
        set_flag(&conn, id1, 1).unwrap(); // Pick

        set_rating(&conn, id2, 2).unwrap();
        set_flag(&conn, id2, -1).unwrap(); // Rejected

        // Filter by min_rating >= 3
        let rated = timeline_items_with_cull(&conn, &TimelineFilter::All, RatingFilter::AtLeast(3), None).unwrap();
        assert_eq!(rated.len(), 1);
        assert_eq!(rated[0].id, id1);
        assert_eq!(rated[0].rating, 4);
        assert_eq!(rated[0].flagged, 1);

        // Filter by Pick flag (1)
        let picks = timeline_items_with_cull(&conn, &TimelineFilter::All, RatingFilter::Any, Some(1)).unwrap();
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].id, id1);

        // Filter by Reject flag (-1)
        let rejects = timeline_items_with_cull(&conn, &TimelineFilter::All, RatingFilter::Any, Some(-1)).unwrap();
        assert_eq!(rejects.len(), 1);
        assert_eq!(rejects[0].id, id2);

        // Batch update
        batch_set_rating(&mut conn, &[id2, id3], 5).unwrap();
        batch_set_flag(&mut conn, &[id2, id3], 1).unwrap();

        let top = timeline_items_with_cull(&conn, &TimelineFilter::All, RatingFilter::AtLeast(5), Some(1)).unwrap();
        assert_eq!(top.len(), 2);

        // img2 and img3 are 5★ now; clearing img1 leaves it the only unrated one.
        set_rating(&conn, id1, 0).unwrap();
        let unrated = timeline_items_with_cull(&conn, &TimelineFilter::All, RatingFilter::Unrated, None).unwrap();
        assert_eq!(unrated.iter().map(|i| i.id).collect::<Vec<_>>(), vec![id1]);
    }

    #[test]
    fn tag_filter_and_smart_search_work() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        insert(&mut conn, "img1.jpg", 1_700_000_000, (3, 2), 1);
        insert(&mut conn, "img2.jpg", 1_700_000_100, (3, 2), 1);

        let all = timeline_items(&conn, &TimelineFilter::All).unwrap();
        let id1 = all.iter().find(|i| i.hash == "img1.jpg").unwrap().id;
        let id2 = all.iter().find(|i| i.hash == "img2.jpg").unwrap().id;

        // Set title and description
        set_title(&conn, id1, "Sunset at Beach").unwrap();
        set_description(&conn, id1, "Golden hour reflection on water").unwrap();

        // Tag img1 with "landscape" and "sunset"
        let t_land = create_tag(&conn, "landscape", None).unwrap();
        let t_sun = create_tag(&conn, "sunset", None).unwrap();
        tag_image(&conn, id1, t_land).unwrap();
        tag_image(&conn, id1, t_sun).unwrap();

        // Tag img2 with "landscape"
        tag_image(&conn, id2, t_land).unwrap();

        // Check get_tags_with_counts
        let tag_counts = get_tags_with_counts(&conn).unwrap();
        assert_eq!(tag_counts.len(), 2);
        let land_count = tag_counts.iter().find(|(t, _)| t.name == "landscape").unwrap().1;
        assert_eq!(land_count, 2);

        // Test TimelineFilter::Tag
        let tagged_sunset = timeline_items_with_cull(&conn, &TimelineFilter::Tag("sunset".to_string()), RatingFilter::Any, None).unwrap();
        assert_eq!(tagged_sunset.len(), 1);
        assert_eq!(tagged_sunset[0].id, id1);

        // Test Smart Search syntax: "tag:sunset"
        let search_tag = timeline_items_with_cull(&conn, &TimelineFilter::Search("tag:sunset".to_string()), RatingFilter::Any, None).unwrap();
        assert_eq!(search_tag.len(), 1);
        assert_eq!(search_tag[0].id, id1);

        // Test Smart Search syntax: "is:pick"
        set_flag(&conn, id2, 1).unwrap();
        let search_pick = timeline_items_with_cull(&conn, &TimelineFilter::Search("is:pick".to_string()), RatingFilter::Any, None).unwrap();
        assert_eq!(search_pick.len(), 1);
        assert_eq!(search_pick[0].id, id2);
    }

    #[test]
    fn set_and_batch_set_orientation_work() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        insert(&mut conn, "img1.jpg", 1_700_000_000, (3, 2), 1);
        insert(&mut conn, "img2.jpg", 1_700_000_100, (3, 2), 1);

        let all = timeline_items(&conn, &TimelineFilter::All).unwrap();
        let id1 = all.iter().find(|i| i.hash == "img1.jpg").unwrap().id;
        let id2 = all.iter().find(|i| i.hash == "img2.jpg").unwrap().id;

        // Test set_orientation single
        set_orientation(&conn, id1, 6).unwrap();
        let img1 = get_image(&conn, id1).unwrap().unwrap();
        assert_eq!(img1.orientation, Some(6));

        // Test batch_set_orientation
        batch_set_orientation(&mut conn, &[(id1, 3), (id2, 8)]).unwrap();
        let img1_b = get_image(&conn, id1).unwrap().unwrap();
        let img2_b = get_image(&conn, id2).unwrap().unwrap();
        assert_eq!(img1_b.orientation, Some(3));
        assert_eq!(img2_b.orientation, Some(8));
    }

    #[test]
    fn albums_crud_and_timeline_filter_work() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        insert(&mut conn, "img1.jpg", 1_700_000_000, (3, 2), 1);
        insert(&mut conn, "img2.jpg", 1_700_000_100, (3, 2), 1);
        insert(&mut conn, "img3.jpg", 1_700_000_200, (3, 2), 1);

        let all = timeline_items(&conn, &TimelineFilter::All).unwrap();
        let id1 = all.iter().find(|i| i.hash == "img1.jpg").unwrap().id;
        let id2 = all.iter().find(|i| i.hash == "img2.jpg").unwrap().id;
        let id3 = all.iter().find(|i| i.hash == "img3.jpg").unwrap().id;

        // Create album
        let album_id = create_album(&conn, "Summer Trip").unwrap();
        let alb = get_album(&conn, album_id).unwrap().unwrap();
        assert_eq!(alb.name, "Summer Trip");

        // Rename album
        rename_album(&conn, album_id, "Summer 2026").unwrap();
        assert_eq!(get_album(&conn, album_id).unwrap().unwrap().name, "Summer 2026");

        // Add photos to album
        let added = add_images_to_album(&mut conn, album_id, &[id1, id2]).unwrap();
        assert_eq!(added, 2);

        // Check count
        let albums = get_all_albums_with_counts(&conn).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].1, 2);
        assert_eq!(albums[0].0.cover_image_id, Some(id1));

        // Filter timeline by album
        let album_items = timeline_items(&conn, &TimelineFilter::Album(album_id)).unwrap();
        assert_eq!(album_items.len(), 2);
        let album_item_ids: Vec<i64> = album_items.iter().map(|i| i.id).collect();
        assert!(album_item_ids.contains(&id1));
        assert!(album_item_ids.contains(&id2));
        assert!(!album_item_ids.contains(&id3));

        // Remove photo from album
        let removed = remove_images_from_album(&mut conn, album_id, &[id1]).unwrap();
        assert_eq!(removed, 1);
        let albums_after = get_all_albums_with_counts(&conn).unwrap();
        assert_eq!(albums_after[0].1, 1);
        // Cover should have fallen back to id2
        assert_eq!(albums_after[0].0.cover_image_id, Some(id2));

        // Delete album
        delete_album(&conn, album_id).unwrap();
        assert!(get_album(&conn, album_id).unwrap().is_none());
        assert!(get_all_albums_with_counts(&conn).unwrap().is_empty());
    }

    #[test]
    fn events_crud_and_timeline_filter_work() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        // 2023-11-14: timestamp ~ 1_700_000_000
        insert(&mut conn, "img1.jpg", 1_700_000_000, (3, 2), 1);
        // 2023-11-15: timestamp ~ 1_700_086_400
        insert(&mut conn, "img2.jpg", 1_700_086_400, (3, 2), 1);

        let event_id = create_event(&conn, "Conference", 1_699_999_000, 1_700_050_000, None).unwrap();
        let ev = get_event(&conn, event_id).unwrap().unwrap();
        assert_eq!(ev.name, "Conference");

        let events_with_counts = get_all_events_with_counts(&conn).unwrap();
        assert_eq!(events_with_counts.len(), 1);
        assert_eq!(events_with_counts[0].1, 1); // only img1 falls within range

        let ev_items = timeline_items(&conn, &TimelineFilter::Event(event_id)).unwrap();
        assert_eq!(ev_items.len(), 1);
        assert_eq!(ev_items[0].hash, "img1.jpg");

        // Test name_day_event
        let day_ev_id = name_day_event(&conn, "Nov 15 Day", 2023, 11, 15).unwrap();
        let day_ev = get_event(&conn, day_ev_id).unwrap().unwrap();
        assert_eq!(day_ev.name, "Nov 15 Day");

        // get_event_for_day
        let found = get_event_for_day(&conn, 2023, 11, 15).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Nov 15 Day");

        delete_event(&conn, event_id).unwrap();
        assert!(get_event(&conn, event_id).unwrap().is_none());
    }

    #[test]
    fn update_from_xmp_sets_fields_and_mtime() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.pool.get().unwrap();
        let img = Image::new(PathBuf::from("/p/test.jpg"), "hash1".to_string(), 10);
        let id = {
            let tx = conn.transaction().unwrap();
            let id = insert_image(&tx, &img).unwrap().unwrap();
            tx.commit().unwrap();
            id
        };
        
        let xmp = crate::models::XmpReadResult {
            rating: Some(3),
            rejected: Some(false),
            tags: vec!["holiday".to_string()],
            ..Default::default()
        };
        update_from_xmp(&mut conn, id, &xmp, 12345).unwrap();
        
        let img2 = get_image(&conn, id).unwrap().unwrap();
        assert_eq!(img2.rating, 3);
        assert_eq!(img2.xmp_mtime, Some(12345));
        
        let tags = get_tags_for_image(&conn, id).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "holiday");
    }

    #[test]
    fn xmp_keywords_replace_the_photos_tags() {
        use crate::models::XmpReadResult;
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.pool.get().unwrap();
        let id = {
            let tx = conn.transaction().unwrap();
            let img = Image::new(PathBuf::from("/p/k.jpg"), "hash-k".to_string(), 10);
            let id = insert_image(&tx, &img).unwrap().unwrap();
            tx.commit().unwrap();
            id
        };
        let tags = |conn: &Connection| {
            let mut names: Vec<String> = get_tags_for_image(conn, id).unwrap().into_iter().map(|t| t.name).collect();
            names.sort();
            names
        };
        let with = |names: &[&str]| XmpReadResult { tags: names.iter().map(|n| n.to_string()).collect(), ..Default::default() };

        update_from_xmp(&mut conn, id, &with(&["a", "b", "c"]), 1).unwrap();
        assert_eq!(tags(&conn), ["a", "b", "c"]);
        // "b" removed in darktable, "d" added.
        update_from_xmp(&mut conn, id, &with(&["a", "c", "d"]), 2).unwrap();
        assert_eq!(tags(&conn), ["a", "c", "d"]);
        // All removed.
        update_from_xmp(&mut conn, id, &with(&[]), 3).unwrap();
        assert!(tags(&conn).is_empty());
        // The tags themselves stay in the library.
        assert!(get_all_tags(&conn).unwrap().iter().any(|t| t.name == "b"));
    }

    #[test]
    fn xmp_reject_sets_the_flag_not_the_rating() {
        use crate::models::XmpReadResult;
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.pool.get().unwrap();
        let id = {
            let tx = conn.transaction().unwrap();
            let img = Image::new(PathBuf::from("/p/r.jpg"), "hash-r".to_string(), 10);
            let id = insert_image(&tx, &img).unwrap().unwrap();
            tx.commit().unwrap();
            id
        };
        let flags = |conn: &Connection| {
            let img = get_image(conn, id).unwrap().unwrap();
            (img.rating, img.flagged)
        };
        set_rating(&conn, id, 4).unwrap();

        // Rejected in darktable: the stars stay, the photo is rejected.
        let reject = XmpReadResult { rejected: Some(true), ..Default::default() };
        update_from_xmp(&mut conn, id, &reject, 1).unwrap();
        assert_eq!(flags(&conn), (4, -1));

        // Un-rejected with 2 stars.
        let rated = XmpReadResult { rating: Some(2), rejected: Some(false), ..Default::default() };
        update_from_xmp(&mut conn, id, &rated, 2).unwrap();
        assert_eq!(flags(&conn), (2, 0));

        // A pick (DB only) survives a rating change from outside...
        set_flag(&conn, id, 1).unwrap();
        update_from_xmp(&mut conn, id, &rated, 3).unwrap();
        assert_eq!(flags(&conn), (2, 1));

        // ...but not a reject.
        update_from_xmp(&mut conn, id, &reject, 4).unwrap();
        assert_eq!(flags(&conn), (2, -1));
    }

    #[test]
    fn mark_missing_and_relink_work() {
        let db = Database::open_in_memory().unwrap();
        let mut conn = db.pool.get().unwrap();
        let img1 = Image::new(PathBuf::from("/p/test1.jpg"), "hash1".to_string(), 10);
        let img2 = Image::new(PathBuf::from("/p/test2.jpg"), "hash2".to_string(), 10);
        let (id1, _id2) = {
            let tx = conn.transaction().unwrap();
            let id1 = insert_image(&tx, &img1).unwrap().unwrap();
            let id2 = insert_image(&tx, &img2).unwrap().unwrap();
            tx.commit().unwrap();
            (id1, id2)
        };
        
        mark_missing(&conn, &[id1], true).unwrap();
        
        let missing = get_missing_images(&conn).unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].id.unwrap(), id1);
        
        relink_image(&conn, id1, "/new/test1.jpg").unwrap();
        
        let missing_after = get_missing_images(&conn).unwrap();
        assert_eq!(missing_after.len(), 0);
        
        let img = get_image(&conn, id1).unwrap().unwrap();
        assert_eq!(img.path, PathBuf::from("/new/test1.jpg"));
        assert_eq!(img.missing, false);
    }

    #[test]
    fn export_presets_are_seeded_saved_by_name_and_deleted() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.pool.get().unwrap();
        let names = |conn: &Connection| get_export_presets(conn).unwrap().into_iter().map(|p| p.1).collect::<Vec<_>>();
        assert_eq!(names(&conn), ["Web 2048 sRGB", "Client full-res", "Print TIFF"]);

        let id = save_export_preset(&conn, "Mine", r#"{"prefix":"a"}"#).unwrap();
        let again = save_export_preset(&conn, "Mine", r#"{"prefix":"b"}"#).unwrap();
        assert_eq!(id, again);
        let presets = get_export_presets(&conn).unwrap();
        assert_eq!(presets.len(), 4);
        assert_eq!(presets[3], (id, "Mine".to_string(), r#"{"prefix":"b"}"#.to_string()));

        delete_export_preset(&conn, id).unwrap();
        assert_eq!(names(&conn).len(), 3);
    }
}
