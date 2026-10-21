//! Originals of a Move import that wait for the user's "delete from the card?"
//! answer. They are kept on disk so that closing Photon before answering
//! doesn't lose the question: the next start asks again.
//!
//! Each entry is only ever added after the photos are committed to the
//! database, so deleting what it lists is as safe as it was at the time.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingRemoval {
    /// Identifies the entry (creation time, in nanoseconds).
    pub id: u64,
    /// The folder or card the photos came from (for the dialog text).
    pub source_path: String,
    /// (original, its sidecar) as in `ImportBatch::removable_sources`.
    pub sources: Vec<(PathBuf, Option<PathBuf>)>,
}

fn read_all(file: &Path) -> Vec<PendingRemoval> {
    match fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            log::warn!("Ignoring unreadable {}: {e}", file.display());
            Vec::new()
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            log::warn!("Could not read {}: {e}", file.display());
            Vec::new()
        }
    }
}

fn write_all(file: &Path, entries: &[PendingRemoval]) -> io::Result<()> {
    if entries.is_empty() {
        return match fs::remove_file(file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(entries).map_err(io::Error::other)?)?;
    fs::rename(&tmp, file)
}

/// Remember a batch of originals awaiting an answer. Returns its id.
pub fn add(
    file: &Path,
    source_path: &str,
    sources: &[(PathBuf, Option<PathBuf>)],
) -> io::Result<u64> {
    let mut entries = read_all(file);
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    entries.push(PendingRemoval {
        id,
        source_path: source_path.to_string(),
        sources: sources.to_vec(),
    });
    write_all(file, &entries)?;
    Ok(id)
}

/// Forget an entry (answered, either way).
pub fn resolve(file: &Path, id: u64) -> io::Result<()> {
    let mut entries = read_all(file);
    entries.retain(|e| e.id != id);
    write_all(file, &entries)
}

/// Entries still worth asking about: originals that are gone (card wiped, files
/// deleted by hand) are dropped, and entries with nothing left are forgotten.
pub fn load(file: &Path) -> Vec<PendingRemoval> {
    let mut entries = read_all(file);
    let before = entries.clone();
    for e in &mut entries {
        e.sources.retain(|(src, _)| src.exists());
    }
    entries.retain(|e| !e.sources.is_empty());
    if entries != before {
        if let Err(e) = write_all(file, &entries) {
            log::warn!("Could not update {}: {e}", file.display());
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn survives_a_restart_and_forgets_answered_and_vanished_entries() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pending_removals.json");
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("b.jpg");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();

        let id1 = add(&file, "/card", &[(a.clone(), None)]).unwrap();
        let id2 = add(&file, "/card2", &[(b.clone(), None)]).unwrap();
        assert_ne!(id1, id2);

        // "Restart": a fresh load sees both.
        assert_eq!(load(&file).len(), 2);

        // The card was wiped by hand: that entry is dropped.
        fs::remove_file(&b).unwrap();
        let left = load(&file);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, id1);

        resolve(&file, id1).unwrap();
        assert!(load(&file).is_empty());
        assert!(!file.exists(), "an empty list leaves no file behind");
    }

    #[test]
    fn a_corrupt_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.json");
        fs::write(&file, b"{not json").unwrap();
        assert!(load(&file).is_empty());
    }
}
