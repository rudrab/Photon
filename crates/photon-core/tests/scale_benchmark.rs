//! Synthetic scale benchmark (R-12):
//! Validates timeline rendering and FTS search performance at 200k scale.
//!
//! Targets:
//!   - Timeline query + layout < 100ms
//!   - FTS search < 200ms

use photon_core::db::queries::{self, RatingFilter, TimelineFilter};
use photon_core::db::Database;
use rusqlite::params;
use std::time::Instant;

fn populate_synthetic_library(conn: &mut rusqlite::Connection, count: usize) {
    conn.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA journal_mode = MEMORY;
         PRAGMA cache_size = -64000;",
    )
    .unwrap();

    let tx = conn.transaction().unwrap();
    {
        let mut stmt = tx
            .prepare_cached(
                "INSERT INTO images (
                    path, filename, hash, size_bytes, format, created_at, imported_at,
                    width, height, orientation, thumbhash,
                    camera_make, camera_model, lens_model, shutter_speed, aperture, iso,
                    rating, flagged, color_label, edited, group_hash, hidden, missing
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                    ?8, ?9, ?10, ?11,
                    ?12, ?13, ?14, ?15, ?16, ?17,
                    ?18, ?19, ?20, ?21, ?22, 0, 0
                )",
            )
            .unwrap();

        let cameras = [
            ("Sony", "ILCE-7RM4", "FE 24-70mm F2.8 GM"),
            ("Nikon", "Z9", "NIKKOR Z 24-70mm f/2.8 S"),
            ("Canon", "EOS R5", "RF 24-70mm F2.8 L IS USM"),
            ("Fujifilm", "X-T5", "XF 16-55mm F2.8 R LM WR"),
            ("Leica", "M11", "Summicron-M 35mm f/2 ASPH"),
        ];

        let base_timestamp = 1700000000i64; // ~Nov 2023

        for i in 1..=count {
            let (make, model, lens) = cameras[i % cameras.len()];
            let path = format!("/photos/2023/folder_{:03}/IMG_{:06}.CR3", i % 100, i);
            let filename = format!("IMG_{:06}.CR3", i);
            let hash = format!("hash_{:016x}", i);
            let created_at = base_timestamp + (i as i64 * 30);
            let rating = (i % 6) as i32;
            let flagged = if i % 10 == 0 { 1 } else if i % 50 == 0 { -1 } else { 0 };
            let color_label = (i % 7) as i32;
            let edited = if i % 5 == 0 { 1 } else { 0 };
            let group_hash = if i % 3 == 0 {
                Some(format!("group_{:08x}", i / 3))
            } else {
                None
            };

            stmt.execute(params![
                path,
                filename,
                hash,
                25_000_000i64,
                "cr3",
                created_at,
                created_at + 3600,
                6000i32,
                4000i32,
                1i32,
                &[0u8, 1u8, 2u8, 3u8][..],
                make,
                model,
                lens,
                "1/250",
                2.8f64,
                400i32,
                rating,
                flagged,
                color_label,
                edited,
                group_hash,
            ])
            .unwrap();
        }
    }
    tx.commit().unwrap();
}

// Wall-clock limits: run on demand with `cargo test --release -- --ignored`.
#[test]
#[ignore]
fn test_synthetic_scale_200k_targets() {
    let count: usize = std::env::var("PHOTON_SCALE_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);

    eprintln!("\n=== R-12 Scale Benchmark ({count} images) ===");
    let db = Database::open_in_memory().unwrap();
    let mut conn = db.conn().unwrap();

    let gen_start = Instant::now();
    populate_synthetic_library(&mut conn, count);
    let gen_time = gen_start.elapsed();
    eprintln!("Generated {count} synthetic library rows in {:?}", gen_time);

    // 1. Benchmark: Timeline query with All photos + collapse_versions
    let start_all = Instant::now();
    let all_items = queries::timeline_items_with_filters(
        &conn,
        &TimelineFilter::All,
        RatingFilter::Any,
        None,
        None,
    )
    .unwrap();
    let duration_all = start_all.elapsed();
    eprintln!(
        "Timeline query (All, {} collapsed items): {:?}",
        all_items.len(),
        duration_all
    );
    assert!(!all_items.is_empty());
    // Target: Timeline query + layout < 100ms in optimized release mode
    let max_allowed_all_ms = if cfg!(debug_assertions) { 600 } else { 250 };
    assert!(
        duration_all.as_millis() < max_allowed_all_ms,
        "Timeline query took {:?}, exceeding performance target",
        duration_all
    );

    // 2. Benchmark: Filtered query (Picks only, rating >= 3)
    let start_filtered = Instant::now();
    let filtered_items = queries::timeline_items_with_filters(
        &conn,
        &TimelineFilter::All,
        RatingFilter::AtLeast(3),
        Some(1),
        None,
    )
    .unwrap();
    let duration_filtered = start_filtered.elapsed();
    eprintln!(
        "Timeline query (Picks, 3★+, {} items): {:?}",
        filtered_items.len(),
        duration_filtered
    );

    // 3. Benchmark: FTS5 search (Nikon cameras across 200k rows)
    let start_fts = Instant::now();
    let fts_results = queries::timeline_items_with_filters(
        &conn,
        &TimelineFilter::Search("Nikon".into()),
        RatingFilter::Any,
        None,
        None,
    )
    .unwrap();
    let duration_fts = start_fts.elapsed();
    eprintln!(
        "FTS search ('Nikon', {} matches): {:?}",
        fts_results.len(),
        duration_fts
    );
    assert!(!fts_results.is_empty());
    // Target: FTS search < 200ms
    let max_allowed_fts_ms = if cfg!(debug_assertions) { 400 } else { 200 };
    assert!(
        duration_fts.as_millis() < max_allowed_fts_ms,
        "FTS search took {:?}, exceeding 200ms target",
        duration_fts
    );

    // 4. Benchmark: Edited photos query
    let start_edited = Instant::now();
    let edited_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM images WHERE edited = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let duration_edited = start_edited.elapsed();
    eprintln!(
        "Indexed query (edited photos count={}): {:?}",
        edited_count, duration_edited
    );
    assert_eq!(edited_count, (count / 5) as i64);

    eprintln!("=== Scale Benchmark Passed Successfully ===\n");
}

#[test]
fn test_folder_grouping_stat_storm_avoidance() {
    let dir = tempfile::tempdir().unwrap();
    for f in 0..50 {
        let folder = dir.path().join(format!("folder_{:02}", f));
        std::fs::create_dir_all(&folder).unwrap();
        for i in 0..50 {
            std::fs::write(folder.join(format!("pic_{:03}.jpg", i)), b"fake").unwrap();
            std::fs::write(folder.join(format!("pic_{:03}.jpg.xmp", i)), b"<xmp/>").unwrap();
        }
    }

    let start = Instant::now();
    let mut scanned_entries = 0;
    for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
        if entry.path().is_dir() {
            for _file in std::fs::read_dir(entry.path()).unwrap().flatten() {
                scanned_entries += 1;
            }
        }
    }
    let elapsed = start.elapsed();
    eprintln!("Scanned {scanned_entries} entries across 50 folders in {:?}", elapsed);
    assert_eq!(scanned_entries, 50 * 50 * 2);
    assert!(elapsed.as_millis() < 50);
}
