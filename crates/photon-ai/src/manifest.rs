//! Model manifest embedded in the application (AI-0).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const EMBEDDED_MANIFEST_TOML: &str = include_str!("../models.toml");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelSpec {
    pub id: String,
    pub task: String,
    pub file: String,
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub licence: String,
    pub licence_url: String,
    pub source: String,
    pub inputs: Vec<Vec<usize>>,
    pub preferred_devices: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelManifest {
    #[serde(rename = "model")]
    pub models: Vec<ModelSpec>,
}

impl ModelManifest {
    pub fn parse(toml_str: &str) -> Result<Self> {
        toml::from_str(toml_str).context("failed to parse models.toml manifest")
    }

    pub fn load_embedded() -> Self {
        Self::parse(EMBEDDED_MANIFEST_TOML).expect("embedded models.toml must be valid")
    }

    pub fn get_by_id(&self, id: &str) -> Option<&ModelSpec> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn get_by_task(&self, task: &str) -> Vec<&ModelSpec> {
        self.models.iter().filter(|m| m.task == task).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_is_valid_and_non_empty() {
        let manifest = ModelManifest::load_embedded();
        assert!(!manifest.models.is_empty());
        for m in &manifest.models {
            assert!(!m.id.is_empty());
            assert!(!m.task.is_empty());
            assert!(!m.file.is_empty());
            assert!(!m.url.is_empty());
            assert_eq!(m.sha256.len(), 64, "sha256 must be 64 hex characters");
            assert!(m.size_bytes > 0);
            assert!(!m.licence.is_empty());
            assert!(!m.licence_url.is_empty());
            assert!(!m.inputs.is_empty());
            assert!(!m.preferred_devices.is_empty());
        }
    }
}
