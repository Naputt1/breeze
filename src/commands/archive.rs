//! Archive patches into `<layer>/backups/patches/` before removing them.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::exit::{self, CommandError};
use crate::store::now_rfc3339;
use crate::store::overlay::{self, Layer, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode};
use crate::store::patches::{self, ArchivedPatch};

/// Archive every patch file backing `entries`, then return the sidecars written.
///
/// Callers archive before deleting the live patch files and overrides. Failures
/// happen before any record is removed.
pub(crate) fn archive_entries(
    repo: &Repo,
    entries: &[OverrideEntry],
) -> Result<Vec<ArchivedPatch>> {
    let mut archived = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.mode != OverrideMode::Patch {
            continue;
        }
        let patch_file = repo.layout.repo_dir.join(&entry.source);
        if !Repo::is_regular_store_file(&patch_file) {
            return Err(overlay::unsafe_refused(format!(
                "cannot archive `{}`: patch file {} is missing or not a regular file",
                entry.path,
                patch_file.display()
            )));
        }
        let metadata_file = patches::metadata_path_for(&patch_file).map_err(|_| {
            CommandError::new(
                exit::GENERAL,
                format!(
                    "cannot archive `{}`: {} does not end in .patch",
                    entry.path,
                    patch_file.display()
                ),
            )
        })?;
        if !Repo::is_regular_store_file(&metadata_file) {
            return Err(overlay::unsafe_refused(format!(
                "cannot archive `{}`: patch metadata {} is missing or not a regular file",
                entry.path,
                metadata_file.display()
            )));
        }

        let contents = fs::read(&patch_file)
            .with_context(|| format!("failed to read {}", patch_file.display()))?;
        let metadata = patches::read_metadata(&metadata_file)?;
        let layer = Layer::from_name(&entry.layer).ok_or_else(|| {
            CommandError::new(
                exit::GENERAL,
                format!(
                    "patch `{}` records an unknown layer `{}`",
                    entry.path, entry.layer
                ),
            )
        })?;
        let layer_dir = repo.layer_dir(layer)?;
        let explicit = entry.name.is_some() || metadata.name.is_some();
        let desired = entry
            .name
            .clone()
            .or_else(|| metadata.name.clone())
            .unwrap_or_else(|| patches::fallback_name(&entry.path, &contents));
        let name = archive_name(repo, &layer_dir, &desired, &entry.path, explicit)?;

        let record = ArchivedPatch {
            name: name.clone(),
            path: entry.path.clone(),
            layer: entry.layer.clone(),
            base_sha: metadata.base_sha.clone(),
            base_blob: metadata.base_blob.clone(),
            archived_at: now_rfc3339(),
        };
        patches::write_archived(&layer_dir, &record, &contents)?;
        archived.push(record);
    }
    Ok(archived)
}

/// Choose the archive name, refusing an explicit collision and suffixing an
/// automatic fallback name until it is free.
fn archive_name(
    repo: &Repo,
    layer_dir: &Path,
    desired: &str,
    path: &str,
    explicit: bool,
) -> Result<String> {
    let existing: Vec<(std::path::PathBuf, ArchivedPatch)> = repo
        .all_layer_dirs()
        .into_iter()
        .flat_map(|dir| {
            patches::list_archived(&dir)
                .into_iter()
                .map(move |record| (dir.clone(), record))
        })
        .collect();

    let Some((existing_dir, existing_record)) =
        existing.iter().find(|(_, record)| record.name == desired)
    else {
        return Ok(desired.to_string());
    };

    if existing_dir == layer_dir && existing_record.path == path {
        return Ok(desired.to_string());
    }

    if explicit {
        return Err(CommandError::new(
            exit::USAGE,
            format!(
                "patch name `{desired}` already belongs to archived patch `{}` ({} layer)",
                existing_record.path, existing_record.layer
            ),
        )
        .into());
    }

    for suffix in 2..1000 {
        let suffix = format!("-{suffix}");
        let keep = 64usize.saturating_sub(suffix.len());
        let mut base = &desired[..desired.len().min(keep)];
        while base.ends_with('-') {
            base = &base[..base.len() - 1];
        }
        let candidate = format!("{base}{suffix}");
        if !existing.iter().any(|(_, record)| record.name == candidate) {
            return Ok(candidate);
        }
    }
    Err(CommandError::new(
        exit::GENERAL,
        format!("could not find a free archive name for `{desired}`"),
    )
    .into())
}

/// Refuse a name that a live or archived patch on another path already uses.
pub(crate) fn ensure_name_available(
    repo: &Repo,
    overrides: &crate::store::overrides::OverridesFile,
    name: &str,
    path: &str,
    layer: Layer,
) -> Result<()> {
    for entry in overrides
        .overrides
        .iter()
        .filter(|entry| entry.mode == OverrideMode::Patch)
    {
        if entry.name.as_deref() != Some(name) {
            continue;
        }
        if entry.path == path && entry.layer == layer.name() {
            continue;
        }
        return Err(CommandError::new(
            exit::USAGE,
            format!(
                "patch name `{name}` is already used by `{}` ({} layer)",
                entry.path, entry.layer
            ),
        )
        .into());
    }

    for dir in repo.all_layer_dirs() {
        for record in patches::list_archived(&dir) {
            if record.name == name && record.path != path {
                return Err(CommandError::new(
                    exit::USAGE,
                    format!(
                        "patch name `{name}` already belongs to archived patch `{}` ({} layer)",
                        record.path, record.layer
                    ),
                )
                .into());
            }
        }
    }

    Ok(())
}
