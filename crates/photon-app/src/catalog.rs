//! Safety copies of the library database: the daily backup, its mirror on
//! the import backup disk, "Back Up Now", and restoring a backup.

use gtk4::glib;
use photon_core::db::{self, BackupOutcome, Database};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Backups kept, in each place.
pub const KEEP: usize = 14;
/// The automatic backup runs when the newest one is older than this.
pub const EVERY: Duration = Duration::from_secs(20 * 3600);

pub fn data_dir() -> PathBuf {
    glib::user_data_dir().join("photon")
}

pub fn db_path() -> PathBuf {
    data_dir().join("photon.db")
}

pub fn backups_dir() -> PathBuf {
    data_dir().join("backups")
}

/// Where the mirror goes inside the import backup folder.
pub fn mirror_dir(import_backup: &Path) -> PathBuf {
    import_backup.join("Photon Catalog Backups")
}

/// What a backup run did.
pub struct Report {
    pub outcome: BackupOutcome,
    /// The mirror on the import backup disk failed: why, for the user.
    pub mirror_problem: Option<String>,
}

/// Blocking. Back up the database (unless the newest backup is younger than
/// `min_age`), then make sure the newest backup is also in the import backup
/// folder, when one is set. A backup disk that isn't plugged in is skipped
/// quietly: the mirror catches up the next time it is.
pub fn back_up(db: &Database, import_backup: Option<&Path>, min_age: Duration) -> Result<Report, String> {
    let outcome = db.back_up(&backups_dir(), KEEP, min_age).map_err(|e| e.to_string())?;
    let mirror_problem = import_backup.and_then(|dir| {
        if !dir.is_dir() {
            log::info!("Import backup folder {} is not available; catalog mirror skipped", dir.display());
            return None;
        }
        match db::mirror_newest_backup(&backups_dir(), &mirror_dir(dir), KEEP) {
            Ok(Some(copy)) => {
                log::info!("Library backup mirrored to {}", copy.display());
                None
            }
            Ok(None) => None,
            Err(e) => {
                log::warn!("Mirroring the library backup to {}: {e}", dir.display());
                Some(format!("The library backup couldn't be copied to {}: {e}", dir.display()))
            }
        }
    });
    Ok(Report { outcome, mirror_problem })
}

/// At startup, before the database is opened: put a restore chosen in
/// Preferences in place. Returns a message for the user when one was applied
/// or failed (then the current database is used as it was).
pub fn apply_pending_restore() -> Option<String> {
    match db::apply_staged_restore(&db_path(), &backups_dir()) {
        Ok(None) => None,
        Ok(Some(kept)) => {
            log::info!("Library restored from backup; the previous database is in {}", kept.display());
            Some(format!("Library restored from backup. The previous database is kept in {}", kept.display()))
        }
        Err(e) => {
            log::error!("Restoring the library from backup: {e}");
            Some(format!("The library could not be restored from backup: {e}"))
        }
    }
}
