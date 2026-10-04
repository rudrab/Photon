//! Import engine: a streaming pipeline, so photos appear in the library while
//! the import is still running (as in Apple Photos) instead of all at the end.
//!
//! ```text
//!  scan ─▶ [I/O stage] ───────────▶ [thumbnail stage] ─▶ [committer]
//!          bounded thread pool       all cores            batched DB
//!          stat + EXIF header        embedded previews    transactions,
//!          duplicate checks          → cache              progress events
//!          copy/move + hash (1 read)
//! ```
//!
//! Per file, the I/O stage does the cheapest thing that can decide its fate:
//!   1. In-place re-import: a path already in the library is skipped unread.
//!   2. Suspected duplicate (Lightroom's rule): same original file name, size
//!      and capture time as a library photo → skipped after reading only the
//!      EXIF header. Re-inserting a full card costs almost nothing.
//!   3. Otherwise the file is copied *while* being hashed (one read of the card),
//!      staged under a temporary name, and only kept if its content is new.
//!      Moves hash first and never touch the source of a duplicate; cross-device
//!      moves are fsynced before the source is deleted.
//!
//! Sidecar groups (RAW+JPG pairs, `_modified` edits) are computed from the
//! scanned paths up front, so streaming never splits a group.

use crate::dedup;
use crate::library::{self, DestinationPlanner};
use crate::metadata::{self, ImageMetadata};
use crate::sidecar;
use crate::sources::ImportSource;
use crate::thumbnails::{ThumbSize, ThumbnailGenerator};
use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use photon_core::db::queries::{self, DuplicateKey};
use photon_core::db::Database;
use photon_core::models::{FolderImportMode, Image, ImportBatch, ImportProgress};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long new photos may wait before being committed and shown.
const COMMIT_INTERVAL: Duration = Duration::from_millis(400);

/// Configuration for an import job.
#[derive(Debug, Clone)]
pub struct ImportConfig {
    pub mode: FolderImportMode,
    pub destination_dir: Option<PathBuf>,
    pub generate_thumbnails: bool,
    pub extract_metadata: bool,
    /// Skip known paths and suspected duplicates without reading them.
    /// (Exact duplicates, by content hash, are always skipped.)
    pub deduplicate: bool,
    /// Max images per DB transaction.
    pub batch_size: usize,
    /// Concurrent file reads/copies; 0 picks by device ([`crate::device`]):
    /// one stream on USB drives, memory cards and hard disks, which serve
    /// one sequential stream much faster than several (measured: 4× on a USB
    /// SSD), several on internal SSDs and network shares.
    pub io_threads: usize,
    /// Copy and Move: read every copy back from disk and compare it with the
    /// original before it counts as imported (and before a move deletes it).
    pub verify: bool,
    /// Copy and Move: also copy each new photo here, in the library's
    /// layout, so two copies exist before the card is formatted. A move only
    /// deletes a file from the card once both copies are made.
    pub backup_dir: Option<PathBuf>,
    pub cancel: CancelToken,
}

impl Default for ImportConfig {
    fn default() -> Self {
        Self {
            mode: FolderImportMode::InPlace,
            destination_dir: None,
            generate_thumbnails: true,
            extract_metadata: true,
            deduplicate: true,
            batch_size: 200,
            io_threads: 0,
            verify: true,
            backup_dir: None,
            cancel: CancelToken::default(),
        }
    }
}

/// Shared flag to stop an import. Files already in flight finish; everything
/// committed so far stays in the library.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What happened to one file.
enum FileOutcome {
    /// Imported; with a problem to report (e.g. no preview could be made).
    New(Box<Image>, Option<String>),
    Duplicate,
    Failed(String),
}

/// Import-wide state shared by the I/O workers.
struct Context<'a> {
    config: &'a ImportConfig,
    groups: HashMap<PathBuf, String>,
    suspected: HashSet<DuplicateKey>,
    /// Content hashes in the library plus those claimed by this import.
    hashes: Mutex<HashSet<String>>,
    planner: Mutex<DestinationPlanner>,
    library_root: PathBuf,
    /// Problems with files that were nonetheless imported (e.g. no backup).
    warnings: Mutex<Vec<String>>,
}

impl Context<'_> {
    /// Reserve `hash` for this file. False if the content is already present.
    fn claim(&self, hash: &str) -> bool {
        self.hashes.lock().unwrap().insert(hash.to_string())
    }

    fn release(&self, hash: &str) {
        self.hashes.lock().unwrap().remove(hash);
    }
}

#[derive(Default)]
struct Outcome {
    inserted: usize,
    duplicates: usize,
    errors: Vec<String>,
}

/// The import engine. Stateless apart from DB + thumbnail handles.
pub struct ImportEngine {
    db: Database,
    thumbnail_gen: Option<ThumbnailGenerator>,
}

impl ImportEngine {
    pub fn new(db: Database, thumbnail_gen: Option<ThumbnailGenerator>) -> Self {
        Self { db, thumbnail_gen }
    }

    pub fn thumbnails(&self) -> Option<&ThumbnailGenerator> {
        self.thumbnail_gen.as_ref()
    }

    /// Run a full import from the given source, reporting on `progress_tx`.
    /// Returns the completed `ImportBatch`.
    pub fn import(
        &self,
        source: &dyn ImportSource,
        config: &ImportConfig,
        progress_tx: Option<&Sender<ImportProgress>>,
    ) -> anyhow::Result<ImportBatch> {
        let emit = |p: ImportProgress| {
            if let Some(tx) = progress_tx {
                let _ = tx.send(p);
            }
        };

        let mut files = source.scan()?;
        if files.is_empty() {
            anyhow::bail!("No importable image files found.");
        }

        let mut batch = ImportBatch::new(
            source.source_type().to_string(),
            source.source_description().to_string(),
            files.len() as i32,
        );
        let conn = self.db.conn()?;
        batch.id = Some(queries::insert_import_batch(&conn, &batch)?);

        let mut out = Outcome::default();
        if config.deduplicate && config.mode == FolderImportMode::InPlace {
            // For linked files the path *is* the identity: skip known ones unread.
            let known = queries::known_paths(&conn)?;
            let before = files.len();
            files.retain(|p| !known.contains(p.to_string_lossy().as_ref()));
            out.duplicates += before - files.len();
        }

        let ctx = Context {
            config,
            groups: sidecar::group_map(&files),
            suspected: if config.deduplicate {
                queries::duplicate_keys(&conn)?
            } else {
                HashSet::new()
            },
            hashes: Mutex::new(queries::all_hashes(&conn)?),
            planner: Mutex::new(DestinationPlanner::new(library::library_root(
                config.destination_dir.as_deref(),
            ))),
            library_root: library::library_root(config.destination_dir.as_deref()),
            warnings: Mutex::new(Vec::new()),
        };
        drop(conn);

        emit(ImportProgress::Started { total: files.len() });
        let result = self.run_pipeline(&files, &ctx, &mut out, &emit);
        out.errors.extend(ctx.warnings.into_inner().unwrap_or_else(|e| e.into_inner()));
        if result.is_ok() {
            // Also for files that were already in the library: re-importing
            // picks up ratings and tags set in the other app since.
            match source.apply_metadata(&*self.db.conn()?) {
                Ok(0) => {}
                Ok(n) => log::info!("{}: carried over ratings/tags for {n} photos", source.source_type()),
                Err(e) => out.errors.push(format!("Ratings and tags not imported: {e}")),
            }
        }

        let file_errors = out.errors.len();
        let cancelled = config.cancel.is_cancelled();
        if config.mode == FolderImportMode::Move && result.is_ok() && !cancelled {
            let src_root = Path::new(source.source_description());
            if src_root.is_dir() {
                let remaining_unsupported: usize = walkdir::WalkDir::new(src_root)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .filter(|e| {
                        let path = e.path();
                        let name = e.file_name().to_string_lossy();
                        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                        !name.starts_with('.')
                            && !name.ends_with(".xmp")
                            && photon_core::models::ImageFormat::from_extension(ext)
                                == photon_core::models::ImageFormat::Unknown
                    })
                    .count();
                if remaining_unsupported > 0 {
                    log::warn!(
                        "{} unsupported files were left behind in source folder",
                        remaining_unsupported
                    );
                    out.errors.push(format!(
                        "{} unsupported files were left behind in source folder",
                        remaining_unsupported
                    ));
                }
            }
        }
        batch.imported_count = out.inserted as i32;
        batch.duplicate_count = out.duplicates as i32;
        batch.error_count = file_errors as i32;
        batch.completed_at = Some(chrono::Utc::now().timestamp());
        batch.status = match (&result, cancelled) {
            (Err(_), _) => "failed",
            (Ok(()), true) => "cancelled",
            (Ok(()), false) => "completed",
        }
        .to_string();
        queries::complete_import_batch(&*self.db.conn()?, &batch)?;
        result?;

        emit(ImportProgress::Completed {
            imported: out.inserted,
            duplicates: out.duplicates,
            errors: out.errors,
            cancelled,
        });
        Ok(batch)
    }

    fn run_pipeline(
        &self,
        files: &[PathBuf],
        ctx: &Context,
        out: &mut Outcome,
        emit: &(impl Fn(ImportProgress) + Sync),
    ) -> anyhow::Result<()> {
        let config = ctx.config;
        let io_threads = if config.io_threads == 0 {
            let n = crate::device::suggested_io_threads(files);
            log::info!("Reading {n} file(s) at a time ({})", files.first().map_or(String::new(), |f| f.display().to_string()));
            n
        } else {
            config.io_threads
        };
        let io_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(io_threads.max(1))
            .thread_name(|i| format!("photon-import-io-{i}"))
            .build()?;
        let thumbs = self.thumbnail_gen.as_ref().filter(|_| config.generate_thumbnails);

        std::thread::scope(|scope| {
            let (read_tx, read_rx) = bounded::<(PathBuf, FileOutcome)>(64);
            let (ready_tx, ready_rx) = bounded::<(PathBuf, FileOutcome)>(64);

            // I/O stage: a few parallel streams from the source device.
            scope.spawn(move || {
                io_pool.install(|| {
                    files.par_iter().for_each_with(read_tx, |tx, path| {
                        if config.cancel.is_cancelled() {
                            return;
                        }
                        let outcome = process_file(path, ctx);
                        let _ = tx.send((path.clone(), outcome));
                    })
                })
            });

            // Thumbnail stage: CPU-bound, all cores. The file was just read, so
            // it is usually still in the page cache.
            scope.spawn(move || {
                read_rx
                    .into_iter()
                    .par_bridge()
                    .for_each_with(ready_tx, |tx, (path, mut outcome)| {
                        if let (FileOutcome::New(img, warning), Some(thumbs)) = (&mut outcome, thumbs) {
                            match thumbs.ensure_grid(img) {
                                Ok((_, th)) => {
                                    img.thumbhash = th;
                                }
                                Err(e) => {
                                    log::warn!("Thumbnail failed for {}: {e:#}", img.path.display());
                                    // Most likely a damaged file, or not what its
                                    // extension says: the user should know now,
                                    // while the card is still at hand.
                                    warning.get_or_insert(format!(
                                        "{}: imported, but it can't be shown (damaged, or not really a .{} file?): {e:#}",
                                        path.display(),
                                        path.extension().unwrap_or_default().to_string_lossy()
                                    ));
                                }
                            }
                        }
                        let _ = tx.send((path, outcome));
                    })
            });

            // Committer (this thread): batch inserts so the UI can show photos early.
            let result = self.commit_stream(ready_rx, files.len(), config, out, emit);
            if result.is_err() {
                config.cancel.cancel(); // stop the stages; they drain quickly
            }
            result
        })
    }

    fn commit_stream(
        &self,
        ready_rx: crossbeam_channel::Receiver<(PathBuf, FileOutcome)>,
        total: usize,
        config: &ImportConfig,
        out: &mut Outcome,
        emit: &impl Fn(ImportProgress),
    ) -> anyhow::Result<()> {
        let mut conn = self.db.conn()?;
        let mut pending: Vec<Image> = Vec::new();
        let mut last_commit = Instant::now();
        let mut processed = 0;

        let mut flush = |pending: &mut Vec<Image>, out: &mut Outcome| -> anyhow::Result<()> {
            if pending.is_empty() {
                return Ok(());
            }
            let (inserted, dupes) = queries::batch_insert_images(&mut conn, pending)?;
            
            for img in pending.iter() {
                if let Some(id) = img.id {
                    if let Err(e) = crate::sidecar::read_image_xmp(&mut conn, id, &img.path, None) {
                        log::warn!("Could not update image {id} from XMP: {e}");
                    }
                }
            }
            
            out.inserted += inserted;
            out.duplicates += dupes;
            pending.clear();
            emit(ImportProgress::Committed {
                imported: out.inserted,
            });
            Ok(())
        };

        loop {
            match ready_rx.recv_timeout(COMMIT_INTERVAL) {
                Ok((path, outcome)) => {
                    processed += 1;
                    match outcome {
                        FileOutcome::New(img, warning) => {
                            pending.push(*img);
                            out.errors.extend(warning);
                        }
                        FileOutcome::Duplicate => out.duplicates += 1,
                        FileOutcome::Failed(msg) => out.errors.push(msg),
                    }
                    emit(ImportProgress::Processing {
                        processed,
                        total,
                        current_file: file_name(&path),
                    });
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if pending.len() >= config.batch_size.max(1)
                || (!pending.is_empty() && last_commit.elapsed() >= COMMIT_INTERVAL)
            {
                flush(&mut pending, out)?;
                last_commit = Instant::now();
            }
        }
        flush(&mut pending, out)
    }

    /// Generate grid thumbnails for library images that have none (e.g. after
    /// a cache wipe or for rows imported by older versions). Runs in parallel;
    /// `on_progress(done, total)` is called from worker threads.
    /// Returns how many thumbnails were generated.
    pub fn generate_missing_thumbnails(
        &self,
        on_progress: impl Fn(usize, usize) + Sync,
    ) -> anyhow::Result<usize> {
        let Some(thumb_gen) = &self.thumbnail_gen else {
            return Ok(0);
        };
        // Release the connection before the parallel part.
        let images = queries::get_all_images(&*self.db.conn()?, i32::MAX, 0)?;
        let missing: Vec<&Image> = images
            .iter()
            .filter(|img| !thumb_gen.exists(img, ThumbSize::Grid))
            .collect();

        let total = missing.len().max(1);
        let done = AtomicUsize::new(0);
        let offline = AtomicUsize::new(0);

        // 1. Thumbnails; each also yields its ThumbHash in the same pass.
        let results: Vec<Option<(i64, Option<Vec<u8>>)>> = missing
            .par_iter()
            .map(|img| {
                let result = if !img.path.exists() {
                    // Files on an unplugged drive: try again next time, quietly.
                    offline.fetch_add(1, Ordering::Relaxed);
                    None
                } else {
                    match thumb_gen.ensure_grid(img) {
                        Ok((_, thumbhash)) => img.id.map(|id| (id, thumbhash)),
                        Err(e) => {
                            log::warn!("Thumbnail failed for {}: {e:#}", img.path.display());
                            None
                        }
                    }
                };
                on_progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
                result
            })
            .collect();
        let generated = results.iter().flatten().count();
        let mut thumbhashes: Vec<(i64, Vec<u8>)> = results
            .into_iter()
            .flatten()
            .filter_map(|(id, th)| Some((id, th?)))
            .collect();
        queries::save_thumbhashes(&mut *self.db.conn()?, &thumbhashes)?;

        // 2. ThumbHashes for photos whose thumbnail already existed (older
        //    versions didn't compute them). Queried *after* step 1, so the
        //    photos just handled aren't decoded a second time.
        let still_missing = queries::get_images_missing_thumbhash(&*self.db.conn()?)?;
        thumbhashes = still_missing
            .par_iter()
            .filter_map(|(id, hash)| Some((*id, thumb_gen.thumbhash_from_disk(hash)?)))
            .collect();
        queries::save_thumbhashes(&mut *self.db.conn()?, &thumbhashes)?;

        let offline = offline.into_inner();
        if offline > 0 {
            log::info!("{offline} photos are offline (file not found); thumbnails deferred");
        }
        Ok(generated)
    }

    /// Fast pre-scan that inspects file headers to classify candidates into
    /// new photos and already-imported duplicates, without full content hashing.
    pub fn pre_scan(
        &self,
        files: &[PathBuf],
        mode: FolderImportMode,
    ) -> anyhow::Result<PreImportReport> {
        let conn = self.db.conn()?;
        let suspected = queries::duplicate_keys(&conn)?;
        let known = if mode == FolderImportMode::InPlace {
            queries::known_paths(&conn)?
        } else {
            HashSet::new()
        };
        drop(conn);

        // Same bounded parallelism as the import's I/O stage: a few streams
        // keep a card reader busy without making it seek.
        let io_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(crate::device::PARALLEL_IO_THREADS)
            .thread_name(|i| format!("photon-prescan-{i}"))
            .build()?;
        let classified: Vec<Option<(bool, PreImportItem)>> = io_pool.install(|| {
            files
                .par_iter()
                .map(|src| {
                    let fs_meta = fs::metadata(src).ok()?;
                    let filename = file_name(src);
                    let size_bytes = fs_meta.len() as i64;

                    if known.contains(src.to_string_lossy().as_ref()) {
                        let item = PreImportItem {
                            path: src.clone(),
                            filename,
                            size_bytes,
                            created_at: None,
                        };
                        return Some((true, item));
                    }

                    // EXIF header only: never reads whole files.
                    let meta = metadata::extract(src).unwrap_or_default();
                    let created_at = meta.capture_date.or_else(|| {
                        let mtime = fs_meta.modified().or_else(|_| fs_meta.created()).ok()?;
                        Some(chrono::DateTime::<chrono::Utc>::from(mtime).timestamp())
                    });
                    let duplicate =
                        suspected.contains(&DuplicateKey::new(&filename, size_bytes, created_at));
                    let item = PreImportItem {
                        path: src.clone(),
                        filename,
                        size_bytes,
                        created_at,
                    };
                    Some((duplicate, item))
                })
                .collect()
        });

        let mut report = PreImportReport::default();
        for (duplicate, item) in classified.into_iter().flatten() {
            if duplicate {
                report.duplicates.push(item);
            } else {
                report.new_files.push(item);
            }
        }

        Ok(report)
    }
}

#[derive(Debug, Clone)]
pub struct PreImportItem {
    pub path: PathBuf,
    pub filename: String,
    pub size_bytes: i64,
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct PreImportReport {
    pub new_files: Vec<PreImportItem>,
    pub duplicates: Vec<PreImportItem>,
}

/// Decide one file's fate with as little I/O as possible, and place it.
fn process_file(src: &Path, ctx: &Context) -> FileOutcome {
    match try_process_file(src, ctx) {
        Ok(Some(img)) => FileOutcome::New(Box::new(img), None),
        Ok(None) => FileOutcome::Duplicate,
        Err(e) => FileOutcome::Failed(format!("{}: {e:#}", src.display())),
    }
}

fn try_process_file(src: &Path, ctx: &Context) -> anyhow::Result<Option<Image>> {
    let config = ctx.config;
    let fs_meta = fs::metadata(src)?;
    if fs_meta.len() == 0 {
        // Nothing to keep; a card write that never finished.
        anyhow::bail!("empty file (0 bytes), not imported");
    }
    let original_name = file_name(src);

    // EXIF lives in the header: cheap even for 50 MB RAW files.
    let meta = if config.extract_metadata {
        metadata::extract(src).unwrap_or_else(|e| {
            log::warn!("Metadata extraction failed for {}: {e:#}", src.display());
            ImageMetadata::default()
        })
    } else {
        ImageMetadata::default()
    };
    // The year/month/day drill-down needs a date; without EXIF, fall back to
    // mtime (wrong if the file was copied without preserving it).
    let created_at = meta.capture_date.or_else(|| {
        let mtime = fs_meta.modified().or_else(|_| fs_meta.created()).ok()?;
        Some(chrono::DateTime::<chrono::Utc>::from(mtime).timestamp())
    });

    let key = DuplicateKey::new(&original_name, fs_meta.len() as i64, created_at);
    if ctx.suspected.contains(&key) {
        return Ok(None);
    }

    // How the XMP sidecar follows the photo: as the photo did. A move whose
    // source had to stay (no backup) copies the sidecar, so the photo left
    // on the card keeps its edits and ratings.
    let mut sidecar_mode = config.mode;

    // Content identity + placement.
    let (path, hash) = match config.mode {
        FolderImportMode::InPlace => {
            let hash = dedup::blake3_hash_file(src)?;
            if !ctx.claim(&hash) {
                return Ok(None);
            }
            (src.to_path_buf(), hash)
        }
        FolderImportMode::Copy => {
            let dest = ctx.planner.lock().unwrap().plan(src, created_at)?;
            let staged = library::stage_copy(src, &dest, config.verify)?;
            let hash = staged.hash.clone();
            if !ctx.claim(&hash) {
                return Ok(None); // dropping `staged` deletes the copy
            }
            match staged.commit() {
                Ok(dest) => {
                    back_up(ctx, src, &dest, &hash);
                    (dest, hash)
                }
                Err(e) => {
                    ctx.release(&hash);
                    return Err(e.into());
                }
            }
        }
        FolderImportMode::Move => {
            let hash = dedup::blake3_hash_file(src)?;
            if !ctx.claim(&hash) {
                return Ok(None); // the source of a duplicate is left untouched
            }
            let dest = ctx.planner.lock().unwrap().plan(src, created_at)?;
            let moved = match library::move_into_place(src, &dest, Some(&hash), config.verify) {
                Ok(moved) => moved,
                Err(e) => {
                    ctx.release(&hash);
                    return Err(e.into());
                }
            };
            // The card's copy goes only once the library copy (and backup) exist.
            let backed_up = back_up(ctx, src, &dest, &hash);
            if moved == library::Moved::Copied {
                if !backed_up {
                    sidecar_mode = FolderImportMode::Copy;
                } else if let Err(e) = fs::remove_file(src) {
                    ctx.warn(format!("{}: imported, but not removed from the source: {e}", src.display()));
                    sidecar_mode = FolderImportMode::Copy;
                }
            }
            (dest, hash)
        }
    };

    let mut image = Image::new(path.clone(), hash, fs_meta.len() as i64);
    image.original_filename = Some(original_name);
    image.format = Some(metadata::format_of(src));
    image.created_at = created_at;
    image.width = meta.width;
    image.height = meta.height;
    image.camera_make = meta.camera_make;
    image.camera_model = meta.camera_model;
    image.lens_model = meta.lens_model;
    image.aperture = meta.f_number;
    image.shutter_speed = meta.exposure_time;
    image.iso = meta.iso;
    image.focal_length = meta.focal_length;
    image.latitude = meta.latitude;
    image.longitude = meta.longitude;
    image.orientation = meta.orientation;

    let mut meta_obj = serde_json::Map::new();
    if let Some(d) = meta.duration {
        meta_obj.insert("duration".to_string(), serde_json::Value::String(d));
    }

    if let Some(group) = ctx.groups.get(src) {
        image.group_hash = Some(group.clone());
        image.has_sidecar = true;
    }

    if let Some(xmp) = sidecar::find_xmp(src) {
        let xmp_path = if config.mode == FolderImportMode::InPlace {
            Some(xmp)
        } else {
            let xmp_dest = sidecar::xmp_destination(src, &xmp, &path);
            match library::transfer_sidecar(sidecar_mode, &xmp, &xmp_dest) {
                Ok(()) => {
                    if let Some(backup_root) = &config.backup_dir {
                        let backed_up = fs::read(&xmp_dest).map_err(Into::into).and_then(|bytes| {
                            library::back_up(&xmp_dest, &blake3::hash(&bytes).to_hex(), &ctx.library_root, backup_root)
                        });
                        if let Err(e) = backed_up {
                            ctx.warn(format!("{}: XMP sidecar not backed up: {e}", src.display()));
                        }
                    }
                    Some(xmp_dest)
                }
                Err(e) => {
                    log::warn!("XMP sidecar for {} not transferred: {e}", src.display());
                    None
                }
            }
        };
        if let Some(xmp_path) = xmp_path {
            image.has_sidecar = true;
            meta_obj.insert(
                "xmp_sidecar".to_string(),
                serde_json::Value::String(xmp_path.to_string_lossy().to_string()),
            );
        }
    }

    if !meta_obj.is_empty() {
        image.metadata_json = Some(serde_json::Value::Object(meta_obj).to_string());
    }

    Ok(Some(image))
}

/// Make the backup copy of newly placed library file `dest` (from `src`),
/// if a backup folder is set. True when the file is now safe to delete from
/// the source: backed up, or no backup wanted. Failures are reported as
/// warnings — the photo itself is imported.
fn back_up(ctx: &Context, src: &Path, dest: &Path, hash: &str) -> bool {
    let Some(backup_root) = &ctx.config.backup_dir else { return true };
    match library::back_up(dest, hash, &ctx.library_root, backup_root) {
        Ok(_) => true,
        Err(e) => {
            let kept = if ctx.config.mode == FolderImportMode::Move { "; kept on the source" } else { "" };
            ctx.warn(format!("{}: imported, but not backed up{kept}: {e}", src.display()));
            false
        }
    }
}

impl Context<'_> {
    fn warn(&self, message: String) {
        log::warn!("{message}");
        self.warnings.lock().unwrap().push(message);
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::DiskSource;
    use crate::testutil::{write_jpeg, Spec};

    fn engine() -> ImportEngine {
        ImportEngine::new(Database::open_in_memory().unwrap(), None)
    }

    fn config(mode: FolderImportMode, lib: &Path) -> ImportConfig {
        ImportConfig {
            mode,
            destination_dir: Some(lib.to_path_buf()),
            generate_thumbnails: false,
            ..Default::default()
        }
    }

    fn photos(engine: &ImportEngine) -> Vec<Image> {
        queries::get_all_images(&engine.db.conn().unwrap(), 1000, 0).unwrap()
    }

    fn files_in(dir: &Path) -> usize {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .count()
    }

    #[test]
    fn in_place_import_keeps_exif_and_skips_known_paths_on_reimport() {
        let dir = tempfile::tempdir().unwrap();
        write_jpeg(&dir.path().join("a.jpg"), &Spec { seed: 1, ..Default::default() });
        write_jpeg(
            &dir.path().join("b.jpg"),
            &Spec { seed: 2, date: "2021:12:31 23:59:59", ..Default::default() },
        );

        let engine = engine();
        let cfg = config(FolderImportMode::InPlace, dir.path());
        let source = DiskSource::new(dir.path().to_path_buf(), true);

        let batch = engine.import(&source, &cfg, None).unwrap();
        assert_eq!((batch.imported_count, batch.duplicate_count, batch.error_count), (2, 0, 0));

        let all = photos(&engine);
        let b = all.iter().find(|i| i.filename == "b.jpg").unwrap();
        assert_eq!(b.camera_model.as_deref(), Some("P1"));
        assert_eq!(b.lens_model.as_deref(), Some("50mm Prime"));
        assert_eq!(b.orientation, Some(6));
        assert_eq!(b.latitude, Some(48.5));
        assert_eq!(b.longitude, Some(-2.25));
        assert_eq!(b.original_filename.as_deref(), Some("b.jpg"));

        // Drill-down levels come from EXIF, not from file times.
        {
            let conn = engine.db.conn().unwrap(); // in-memory pool holds one connection
            assert_eq!(queries::get_years(&conn).unwrap(), vec![(2023, 1), (2021, 1)]);
            assert_eq!(queries::get_images_by_day(&conn, 2021, 12, 31).unwrap().len(), 1);
        }

        let again = engine.import(&source, &cfg, None).unwrap();
        assert_eq!((again.imported_count, again.duplicate_count), (0, 2));
    }

    #[test]
    fn copy_import_files_by_date_and_never_copies_duplicates() {
        let card = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write_jpeg(&card.path().join("DCIM/one.jpg"), &Spec { seed: 1, ..Default::default() });
        fs::copy(card.path().join("DCIM/one.jpg"), card.path().join("DCIM/one_copy.jpg")).unwrap();

        let engine = engine();
        let cfg = config(FolderImportMode::Copy, lib.path());
        let source = DiskSource::new(card.path().to_path_buf(), true);

        let batch = engine.import(&source, &cfg, None).unwrap();
        assert_eq!((batch.imported_count, batch.duplicate_count), (1, 1));
        assert!(lib.path().join("2023/07/04").is_dir());
        assert_eq!(files_in(lib.path()), 1, "duplicate must not be copied");

        // Re-importing the same card copies nothing new.
        let again = engine.import(&source, &cfg, None).unwrap();
        assert_eq!((again.imported_count, again.duplicate_count), (0, 2));
        assert_eq!(files_in(lib.path()), 1);
        assert!(card.path().join("DCIM/one.jpg").exists());
    }

    #[test]
    fn suspected_duplicates_are_skipped_without_hashing() {
        let card = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let photo = card.path().join("IMG_1.jpg");
        write_jpeg(&photo, &Spec { seed: 1, ..Default::default() });

        let engine = engine();
        let cfg = config(FolderImportMode::Copy, lib.path());
        let source = DiskSource::new(card.path().to_path_buf(), true);
        engine.import(&source, &cfg, None).unwrap();

        // Same name, size and EXIF date, different pixels: a content hash would
        // call it new, so being skipped proves the file was never fully read.
        let mut bytes = fs::read(&photo).unwrap();
        let last = bytes.len() - 3;
        bytes[last] ^= 0xFF;
        fs::write(&photo, bytes).unwrap();

        let again = engine.import(&source, &cfg, None).unwrap();
        assert_eq!((again.imported_count, again.duplicate_count), (0, 1));
        assert_eq!(files_in(lib.path()), 1);
    }

    #[test]
    fn move_import_relocates_files_and_their_xmp() {
        let card = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let src = card.path().join("IMG_9.jpg");
        write_jpeg(&src, &Spec::default());
        fs::write(card.path().join("IMG_9.jpg.xmp"), "<xmp/>").unwrap();

        let engine = engine();
        let source = DiskSource::new(card.path().to_path_buf(), true);
        engine.import(&source, &config(FolderImportMode::Move, lib.path()), None).unwrap();

        assert!(!src.exists());
        assert!(!card.path().join("IMG_9.jpg.xmp").exists());
        let dest = lib.path().join("2023/07/04");
        assert!(dest.join("IMG_9.jpg").exists());
        assert!(dest.join("IMG_9.jpg.xmp").exists());

        let img = &photos(&engine)[0];
        assert!(img.has_sidecar);
        assert!(img.metadata_json.as_deref().unwrap().contains("IMG_9.jpg.xmp"));
    }

    #[test]
    fn copy_import_makes_a_backup_copy_in_library_layout() {
        let card = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let backup = tempfile::tempdir().unwrap();
        write_jpeg(&card.path().join("IMG_9.jpg"), &Spec::default());
        fs::write(card.path().join("IMG_9.jpg.xmp"), "<xmp/>").unwrap();

        let cfg = ImportConfig {
            backup_dir: Some(backup.path().to_path_buf()),
            ..config(FolderImportMode::Copy, lib.path())
        };
        let batch = engine().import(&DiskSource::new(card.path().to_path_buf(), true), &cfg, None).unwrap();

        assert_eq!((batch.imported_count, batch.error_count), (1, 0));
        let day = Path::new("2023/07/04");
        assert_eq!(
            fs::read(backup.path().join(day).join("IMG_9.jpg")).unwrap(),
            fs::read(lib.path().join(day).join("IMG_9.jpg")).unwrap()
        );
        assert!(backup.path().join(day).join("IMG_9.jpg.xmp").exists());
    }

    /// A card on another filesystem than the library (so a move copies):
    /// /tmp is usually tmpfs, the build directory is not.
    fn cross_device_dirs() -> Option<(tempfile::TempDir, tempfile::TempDir)> {
        use std::os::unix::fs::MetadataExt;
        let card = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let dev = |p: &Path| fs::metadata(p).unwrap().dev();
        (dev(card.path()) != dev(lib.path())).then_some((card, lib))
    }

    #[test]
    fn move_keeps_the_card_copy_until_it_is_backed_up() {
        let Some((card, lib)) = cross_device_dirs() else {
            eprintln!("skipped: needs /tmp and the build dir on different filesystems");
            return;
        };
        let src = card.path().join("IMG_9.jpg");
        write_jpeg(&src, &Spec::default());
        let src_xmp = card.path().join("IMG_9.jpg.xmp");
        fs::write(&src_xmp, "<xmp/>").unwrap();
        // A backup folder that can't be created.
        let blocker = card.path().join("not-a-dir");
        fs::write(&blocker, b"").unwrap();

        let cfg = ImportConfig {
            backup_dir: Some(blocker.join("backup")),
            ..config(FolderImportMode::Move, lib.path())
        };
        let engine = engine();
        let batch = engine.import(&DiskSource::new(card.path().to_path_buf(), true), &cfg, None).unwrap();

        // Two warnings: the photo and its sidecar weren't backed up.
        assert_eq!(batch.imported_count, 1);
        assert!(batch.error_count >= 1);
        assert!(src.exists(), "no backup, so the card copy stays");
        assert!(src_xmp.exists(), "and so does its sidecar");
        assert!(lib.path().join("2023/07/04/IMG_9.jpg").exists());
        assert!(lib.path().join("2023/07/04/IMG_9.jpg.xmp").exists(), "the library copy has one too");
        fs::remove_file(&src_xmp).unwrap();

        // With a working backup, the move completes.
        let backup = tempfile::tempdir().unwrap();
        let src2 = card.path().join("IMG_10.jpg");
        write_jpeg(&src2, &Spec { seed: 9, ..Default::default() });
        let cfg = ImportConfig { backup_dir: Some(backup.path().to_path_buf()), ..cfg };
        let batch = engine.import(&DiskSource::new(card.path().to_path_buf(), true), &cfg, None).unwrap();
        assert_eq!(batch.imported_count, 1);
        assert!(!src2.exists());
        assert!(backup.path().join("2023/07/04/IMG_10.jpg").exists());
    }

    #[test]
    fn move_never_touches_the_source_of_a_duplicate() {
        let card1 = tempfile::tempdir().unwrap();
        let card2 = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write_jpeg(&card1.path().join("a.jpg"), &Spec { seed: 5, ..Default::default() });
        fs::copy(card1.path().join("a.jpg"), card2.path().join("renamed.jpg")).unwrap();

        let engine = engine();
        let cfg = config(FolderImportMode::Move, lib.path());
        engine.import(&DiskSource::new(card1.path().to_path_buf(), true), &cfg, None).unwrap();
        let second = engine
            .import(&DiskSource::new(card2.path().to_path_buf(), true), &cfg, None)
            .unwrap();

        assert_eq!((second.imported_count, second.duplicate_count), (0, 1));
        assert!(card2.path().join("renamed.jpg").exists());
        assert_eq!(files_in(lib.path()), 1);
    }

    #[test]
    fn photos_are_committed_progressively() {
        let dir = tempfile::tempdir().unwrap();
        for seed in 0..5 {
            write_jpeg(&dir.path().join(format!("{seed}.jpg")), &Spec { seed, ..Default::default() });
        }
        let engine = engine();
        let cfg = ImportConfig {
            batch_size: 1,
            ..config(FolderImportMode::InPlace, dir.path())
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        engine
            .import(&DiskSource::new(dir.path().to_path_buf(), true), &cfg, Some(&tx))
            .unwrap();

        let commits: Vec<usize> = rx
            .try_iter()
            .filter_map(|p| match p {
                ImportProgress::Committed { imported } => Some(imported),
                _ => None,
            })
            .collect();
        assert_eq!(commits, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn cancelled_import_stops_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        write_jpeg(&dir.path().join("a.jpg"), &Spec::default());
        let engine = engine();
        let cfg = config(FolderImportMode::InPlace, dir.path());
        cfg.cancel.cancel();

        let (tx, rx) = crossbeam_channel::unbounded();
        let batch = engine
            .import(&DiskSource::new(dir.path().to_path_buf(), true), &cfg, Some(&tx))
            .unwrap();
        assert_eq!(batch.imported_count, 0);
        assert_eq!(batch.status, "cancelled");
        assert!(rx
            .try_iter()
            .any(|p| matches!(p, ImportProgress::Completed { cancelled: true, .. })));
    }

    #[test]
    fn pre_scan_classifies_without_importing() {
        let card = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write_jpeg(&card.path().join("old.jpg"), &Spec { seed: 1, ..Default::default() });

        let engine = engine();
        let cfg = config(FolderImportMode::Copy, lib.path());
        let source = DiskSource::new(card.path().to_path_buf(), true);
        engine.import(&source, &cfg, None).unwrap();

        write_jpeg(&card.path().join("new.jpg"), &Spec { seed: 2, ..Default::default() });
        let files = source.scan().unwrap();
        let report = engine.pre_scan(&files, FolderImportMode::Copy).unwrap();

        let names = |items: &[PreImportItem]| -> Vec<String> {
            items.iter().map(|i| i.filename.clone()).collect()
        };
        assert_eq!(names(&report.new_files), ["new.jpg"]);
        assert_eq!(names(&report.duplicates), ["old.jpg"]);
        assert_eq!(photos(&engine).len(), 1, "pre-scan must not import");
    }

    #[test]
    fn backfill_generates_thumbnails_and_thumbhashes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_jpeg(&dir.path().join("a.jpg"), &Spec { width: 64, height: 32, ..Default::default() });

        // Import without thumbnails, as older versions effectively did.
        let engine = ImportEngine::new(
            Database::open_in_memory().unwrap(),
            Some(ThumbnailGenerator::new(cache.path().to_path_buf())),
        );
        let source = DiskSource::new(dir.path().to_path_buf(), true);
        engine.import(&source, &config(FolderImportMode::InPlace, dir.path()), None).unwrap();
        assert!(photos(&engine)[0].thumbhash.is_none());

        assert_eq!(engine.generate_missing_thumbnails(|_, _| {}).unwrap(), 1);
        let img = &photos(&engine)[0];
        assert!(engine.thumbnails().unwrap().exists(img, ThumbSize::Grid));
        assert!(img.thumbhash.as_ref().is_some_and(|th| !th.is_empty()));

        // Second run: nothing left to do.
        assert_eq!(engine.generate_missing_thumbnails(|_, _| {}).unwrap(), 0);
    }

    #[test]
    fn unparseable_file_does_not_abort_the_import() {
        let dir = tempfile::tempdir().unwrap();
        write_jpeg(&dir.path().join("good.jpg"), &Spec::default());
        fs::write(dir.path().join("bad.jpg"), b"not really a jpeg").unwrap();

        let engine = engine();
        let source = DiskSource::new(dir.path().to_path_buf(), true);
        let (tx, rx) = crossbeam_channel::unbounded();
        let batch = engine
            .import(&source, &config(FolderImportMode::InPlace, dir.path()), Some(&tx))
            .unwrap();

        // The garbage file still imports (hash + mtime date); nothing panics or aborts.
        assert_eq!(batch.imported_count, 2);
        assert!(rx.try_iter().any(|p| matches!(p, ImportProgress::Completed { .. })));
    }

    #[test]
    fn damaged_and_unreadable_files_are_reported_and_the_import_continues() {
        use std::os::unix::fs::PermissionsExt;
        let card = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let good = card.path().join("good.jpg");
        write_jpeg(&good, &Spec { seed: 1, ..Default::default() });
        let jpeg = fs::read(&good).unwrap();

        fs::write(card.path().join("empty.jpg"), b"").unwrap();
        fs::write(card.path().join("notes.png"), b"shopping list, not a picture").unwrap();
        fs::write(card.path().join("half.jpg"), &jpeg[..jpeg.len() / 2]).unwrap();
        let locked = card.path().join("locked.jpg");
        write_jpeg(&locked, &Spec { seed: 2, ..Default::default() });
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads anything: then there is no permission error to see.
        let can_lock = fs::File::open(&locked).is_err();

        let engine = ImportEngine::new(
            Database::open_in_memory().unwrap(),
            Some(ThumbnailGenerator::new(cache.path().to_path_buf())),
        );
        let cfg = ImportConfig { generate_thumbnails: true, ..config(FolderImportMode::Copy, lib.path()) };
        let (tx, rx) = crossbeam_channel::unbounded();
        engine.import(&DiskSource::new(card.path().to_path_buf(), true), &cfg, Some(&tx)).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();

        let errors = rx
            .try_iter()
            .find_map(|p| match p {
                ImportProgress::Completed { errors, .. } => Some(errors),
                _ => None,
            })
            .expect("the import completes");
        let reported = |name: &str, why: &str| errors.iter().any(|e| e.contains(name) && e.contains(why));

        let names: Vec<String> = photos(&engine).into_iter().map(|i| i.filename).collect();
        assert!(names.contains(&"good.jpg".to_string()), "{names:?}");
        assert!(!names.contains(&"empty.jpg".to_string()), "empty files are not imported");
        assert!(reported("empty.jpg", "empty file"), "{errors:?}");
        assert!(reported("notes.png", "can't be shown"), "{errors:?}");
        if can_lock {
            assert!(!names.contains(&"locked.jpg".to_string()));
            assert!(reported("locked.jpg", "ermission denied"), "{errors:?}");
        }
        // The truncated JPEG may or may not decode; either way it is imported
        // (its bytes are the photo) and nothing aborted.
        assert!(names.contains(&"half.jpg".to_string()), "{names:?}");
        assert_eq!(files_in(card.path()), 5, "a copy import never touches the card");
    }
}
