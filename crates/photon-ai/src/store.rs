//! Local model file storage, verification, and downloads (AI-0).

use crate::manifest::ModelSpec;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Status of a model in local storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    NotDownloaded,
    Ready { size_bytes: u64 },
    Corrupt { reason: String },
}

#[derive(Debug, Clone)]
pub struct ModelStore {
    base_dir: PathBuf,
}

impl ModelStore {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn default_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("photon")
            .join("models")
    }

    pub fn default_store() -> Self {
        Self::new(Self::default_dir())
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn model_path(&self, spec: &ModelSpec) -> PathBuf {
        self.base_dir.join(&spec.file)
    }

    /// Check if the model is downloaded and valid on disk.
    pub fn status(&self, spec: &ModelSpec) -> ModelStatus {
        let path = self.model_path(spec);
        if !path.exists() {
            return ModelStatus::NotDownloaded;
        }

        match fs::metadata(&path) {
            Ok(meta) => {
                let size = meta.len();
                // Cheap check first: size comparison
                if size != spec.size_bytes {
                    return ModelStatus::Corrupt {
                        reason: format!(
                            "Size mismatch: expected {} bytes, found {}",
                            spec.size_bytes, size
                        ),
                    };
                }

                // Verify SHA-256 hash
                match self.compute_sha256(&path) {
                    Ok(hash) => {
                        if hash.eq_ignore_ascii_case(&spec.sha256) {
                            ModelStatus::Ready { size_bytes: size }
                        } else {
                            ModelStatus::Corrupt {
                                reason: format!(
                                    "Hash mismatch: expected {}, computed {}",
                                    spec.sha256, hash
                                ),
                            }
                        }
                    }
                    Err(e) => ModelStatus::Corrupt {
                        reason: format!("Failed to read file for hashing: {e}"),
                    },
                }
            }
            Err(e) => ModelStatus::Corrupt {
                reason: format!("Failed to access file: {e}"),
            },
        }
    }

    /// Verify SHA-256 of the model file before loading.
    pub fn verify(&self, spec: &ModelSpec) -> Result<PathBuf> {
        let path = self.model_path(spec);
        if !path.exists() {
            bail!("Model {} ({}) is not downloaded", spec.id, spec.file);
        }

        let computed = self.compute_sha256(&path)?;
        if !computed.eq_ignore_ascii_case(&spec.sha256) {
            bail!(
                "SHA-256 mismatch for {}: expected {}, found {}",
                spec.file,
                spec.sha256,
                computed
            );
        }

        Ok(path)
    }

    /// Download a model to `.part` file, verify hash, then rename.
    pub fn download(
        &self,
        spec: &ModelSpec,
        progress_cb: Option<&(dyn Fn(u64, u64) + Sync)>,
        cancel: Option<&AtomicBool>,
    ) -> Result<PathBuf> {
        fs::create_dir_all(&self.base_dir)
            .with_context(|| format!("failed to create dir {}", self.base_dir.display()))?;

        let final_path = self.model_path(spec);
        let part_path = self.base_dir.join(format!("{}.part", spec.file));

        if part_path.exists() {
            let _ = fs::remove_file(&part_path);
        }

        log::info!("Downloading model {} from {}", spec.id, spec.url);
        // A stalled connection must fail, not hang the caller forever.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(30))
            .build();
        let resp = agent
            .get(&spec.url)
            .call()
            .with_context(|| format!("HTTP GET failed for {}", spec.url))?;

        let total_size: u64 = resp
            .header("Content-Length")
            .and_then(|h| h.parse().ok())
            .unwrap_or(spec.size_bytes);

        let mut reader = resp.into_reader();
        let mut file = File::create(&part_path)
            .with_context(|| format!("failed to create {}", part_path.display()))?;

        let mut hasher = Sha256::new();
        let mut buf = [0u8; 32768];
        let mut downloaded = 0u64;

        loop {
            if let Some(c) = cancel {
                if c.load(Ordering::Relaxed) {
                    let _ = fs::remove_file(&part_path);
                    bail!("Download of {} cancelled", spec.id);
                }
            }

            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }

            file.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            downloaded += n as u64;

            if let Some(cb) = progress_cb {
                cb(downloaded, total_size);
            }
        }

        file.flush()?;
        drop(file);

        let computed_hash = format!("{:x}", hasher.finalize());
        if !computed_hash.eq_ignore_ascii_case(&spec.sha256) {
            let _ = fs::remove_file(&part_path);
            bail!(
                "Downloaded file hash mismatch for {}: expected {}, found {}",
                spec.id,
                spec.sha256,
                computed_hash
            );
        }

        fs::rename(&part_path, &final_path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                part_path.display(),
                final_path.display()
            )
        })?;

        log::info!("Successfully downloaded and verified {}", spec.id);
        Ok(final_path)
    }

    /// Delete a downloaded model file.
    pub fn delete(&self, spec: &ModelSpec) -> Result<()> {
        let path = self.model_path(spec);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        let part = self.base_dir.join(format!("{}.part", spec.file));
        if part.exists() {
            let _ = fs::remove_file(part);
        }
        Ok(())
    }

    /// Total size in bytes of all valid models stored locally.
    pub fn total_size(&self, specs: &[ModelSpec]) -> u64 {
        let mut total = 0u64;
        for spec in specs {
            if let ModelStatus::Ready { size_bytes } = self.status(spec) {
                total += size_bytes;
            }
        }
        total
    }

    fn compute_sha256(&self, path: &Path) -> Result<String> {
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 32768];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn store_status_and_corruption_detection() {
        let dir = tempdir().unwrap();
        let store = ModelStore::new(dir.path().to_path_buf());

        let spec = ModelSpec {
            id: "test-model".into(),
            task: "test".into(),
            file: "model.onnx".into(),
            url: "http://example.com/model.onnx".into(),
            sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into(), // hash of "test"
            size_bytes: 4,
            licence: "MIT".into(),
            licence_url: "http://example.com".into(),
            source: "Test".into(),
            inputs: vec![vec![1, 3]],
            preferred_devices: vec!["cpu".into()],
        };

        // Not downloaded yet
        assert_eq!(store.status(&spec), ModelStatus::NotDownloaded);

        // Write corrupt file (wrong size)
        let path = store.model_path(&spec);
        fs::write(&path, b"hello world").unwrap();
        match store.status(&spec) {
            ModelStatus::Corrupt { .. } => {}
            other => panic!("expected corrupt status, got {:?}", other),
        }

        // Write correct file
        fs::write(&path, b"test").unwrap();
        assert_eq!(store.status(&spec), ModelStatus::Ready { size_bytes: 4 });

        // Verify succeeds
        assert!(store.verify(&spec).is_ok());

        // Delete removes file
        store.delete(&spec).unwrap();
        assert_eq!(store.status(&spec), ModelStatus::NotDownloaded);
    }
}
