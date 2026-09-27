//! Content hashing for deduplication.
//!
//! Blake3 is fast enough that hashing is I/O-bound, so the only tuning that
//! matters is read size: large reads keep SD cards and NVMe streaming.

use std::fs::File;
use std::io::Read;
use std::path::Path;

const READ_BUF: usize = 1024 * 1024;

/// Streaming Blake3 hash of the file at `path`, hex-encoded.
pub fn blake3_hash_file(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; READ_BUF];

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_in_memory_blake3_across_buffer_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let data: Vec<u8> = (0..READ_BUF * 2 + 123).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        assert_eq!(
            blake3_hash_file(&path).unwrap(),
            blake3::hash(&data).to_hex().to_string()
        );
    }
}
