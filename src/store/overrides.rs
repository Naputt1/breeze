//! The overrides index at `repos/<repo-id>/state/overrides.json`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{read_json, SCHEMA_VERSION};

/// How an overlay is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverrideMode {
    /// The overlay file replaces the working-tree file wholesale.
    Link,
    /// The overlay is a patch applied to the tracked file.
    Patch,
}

/// One recorded overlay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideEntry {
    /// Repository-relative path.
    pub path: String,
    pub mode: OverrideMode,
    /// Layer name, such as `shared`, `branch` or `worktree`.
    pub layer: String,
    /// Store-relative path of the overlay source.
    pub source: String,
    pub tracked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Optional human-readable patch name, unique per repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl OverrideEntry {
    /// `name (path)` for named patches, otherwise the bare path.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("{name} ({})", self.path),
            None => self.path.clone(),
        }
    }
}

/// Contents of `state/overrides.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverridesFile {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub overrides: Vec<OverrideEntry>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for OverridesFile {
    fn default() -> Self {
        Self::empty()
    }
}

impl OverridesFile {
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            overrides: Vec::new(),
        }
    }

    /// Load the index. A missing file is treated as empty.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::empty());
        }
        read_json(path)
    }

    /// Write the index as pretty JSON with mode 0600.
    pub fn save(&self, path: &Path) -> Result<()> {
        super::write_json(path, self)
    }

    /// Insert or replace an entry, keyed by path, layer and source.
    ///
    /// The source identifies the branch or worktree for scoped layers, so
    /// overrides for the same path on different branches coexist.
    pub fn upsert(&mut self, entry: OverrideEntry) {
        if let Some(existing) = self.overrides.iter_mut().find(|candidate| {
            candidate.path == entry.path
                && candidate.layer == entry.layer
                && candidate.source == entry.source
        }) {
            let created_at = existing.created_at.clone();
            *existing = entry;
            existing.created_at = created_at;
        } else {
            self.overrides.push(entry);
        }
    }

    /// Number of link and patch entries.
    pub fn counts(&self) -> (usize, usize) {
        let mut links = 0;
        let mut patches = 0;
        for entry in &self.overrides {
            match entry.mode {
                OverrideMode::Link => links += 1,
                OverrideMode::Patch => patches += 1,
            }
        }
        (links, patches)
    }
}

/// Remove a store file, ignoring absence.
#[allow(dead_code)] // Used by later phases.
pub fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}
