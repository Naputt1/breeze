//! Git clean/smudge filter mode.
//!
//! In filter mode Breeze stops writing patches into the working tree. Instead
//! each patched path is registered in `info/attributes` and Git runs
//! `breeze filter clean`/`smudge` around every checkout. The working tree then
//! shows patched contents while the index, commits and clones only ever see the
//! unpatched upstream blob.
//!
//! Both drivers always exit successfully and always write something to stdout:
//! a conflict is not an error, it only means the patch is *deferred* and the
//! upstream content passes through untouched.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::store::attributes;
use crate::store::overlay::{Layer, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode, OverridesFile};
use crate::store::patches;

/// Result of dry-running the smudge merge for one patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The patch merged cleanly onto the current upstream content.
    Clean,
    /// The patch conflicts with upstream and smudge would defer it.
    Deferred,
    /// Patch metadata or captured contents are missing.
    Unavailable,
}

/// Whether filter mode is configured in this repository.
pub fn enabled(root: &Path) -> bool {
    config_value(root, "filter.breeze.smudge")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

/// Whether the driver is configured as required.
pub fn required(root: &Path) -> bool {
    config_value(root, "filter.breeze.required")
        .map(|value| value.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Read one repo-local config value.
pub fn config_value(root: &Path, key: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--local", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Path to the executable named by the configured smudge command.
pub fn configured_binary(root: &Path) -> Option<PathBuf> {
    let value = config_value(root, "filter.breeze.smudge")?;
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('\'') {
        let end = rest.find('\'')?;
        return Some(PathBuf::from(&rest[..end]));
    }
    Some(PathBuf::from(value.split_whitespace().next()?))
}

/// `.git/info/attributes` for this repository (shared by all worktrees).
pub fn attributes_file(repo: &Repo) -> PathBuf {
    repo.info.common_dir.join("info").join("attributes")
}

/// Paths currently registered in the managed attributes block.
pub fn registered_paths(repo: &Repo) -> Result<Vec<String>> {
    let text = attributes::read(&attributes_file(repo))?;
    Ok(attributes::managed_paths(&text))
}

/// Paths that should be registered: every shared-layer patch.
pub fn expected_paths(overrides: &OverridesFile) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for entry in &overrides.overrides {
        if entry.mode == OverrideMode::Patch && entry.layer == Layer::Shared.name() {
            paths.insert(entry.path.clone());
        }
    }
    paths.into_iter().collect()
}

/// Rewrite the managed attributes block to match the active shared patches.
pub fn sync_attributes(repo: &Repo, overrides: &OverridesFile) -> Result<()> {
    attributes::write_paths(&attributes_file(repo), &expected_paths(overrides))
}

/// Point `filter.breeze` at this executable.
pub fn enable_config(repo: &Repo) -> Result<()> {
    let binary = std::env::current_exe().context("failed to locate the breeze executable")?;
    let binary = binary.to_string_lossy();
    let quoted = shell_quote(&binary);

    set_config(
        &repo.info.root,
        "filter.breeze.smudge",
        &format!("{quoted} filter smudge %f"),
    )?;
    set_config(
        &repo.info.root,
        "filter.breeze.clean",
        &format!("{quoted} filter clean %f"),
    )?;
    set_config(&repo.info.root, "filter.breeze.required", "true")?;
    Ok(())
}

/// Remove the `filter.breeze` section.
pub fn disable_config(repo: &Repo) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo.info.root)
        .args(["config", "--local", "--remove-section", "filter.breeze"])
        .output()
        .context("failed to run git config")?;
    // A missing section is not an error.
    if !output.status.success() && config_value(&repo.info.root, "filter.breeze.smudge").is_some() {
        bail!(
            "failed to remove filter.breeze config: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn set_config(root: &Path, key: &str, value: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--local", "--replace-all", key, value])
        .output()
        .context("failed to run git config")?;
    if !output.status.success() {
        bail!(
            "failed to set {key}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./+@:".contains(ch))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Rewrite `relative` in `root` through the smudge filter.
///
/// Removing the file first makes Git rewrite it even when the index entry did
/// not change, which is what makes `edit`/`revert`/`enable` deterministic.
pub fn checkout(root: &Path, relative: &str) -> Result<()> {
    let path = root.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        _ => {}
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["checkout", "--", relative])
        .output()
        .context("failed to run git checkout")?;
    if !output.status.success() {
        bail!(
            "failed to check out {relative}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The shared-layer patch entry for `relative`, if any.
pub fn shared_entry<'a>(overrides: &'a OverridesFile, relative: &str) -> Option<&'a OverrideEntry> {
    overrides.overrides.iter().find(|entry| {
        entry.mode == OverrideMode::Patch
            && entry.layer == Layer::Shared.name()
            && entry.path == relative
    })
}

/// Full patched contents captured for a patch, when present and safe to read.
pub fn read_patch_content(patch_file: &Path) -> Option<Vec<u8>> {
    let path = patches::content_path_for(patch_file).ok()?;
    if !Repo::is_regular_store_file(&path) {
        return None;
    }
    fs::read(&path).ok()
}

/// Capture the full patched contents for `relative` if they are missing.
pub fn ensure_content(repo: &Repo, relative: &str) -> Result<()> {
    let overrides = repo.overrides()?;
    let Some(entry) = shared_entry(&overrides, relative) else {
        return Ok(());
    };
    let patch_file = repo.layout.repo_dir.join(&entry.source);
    let content_path = patches::content_path_for(&patch_file)?;
    if Repo::is_regular_store_file(&content_path) {
        return Ok(());
    }

    let metadata = patches::read_metadata(&patches::metadata_path_for(&patch_file)?)?;
    let contents = patches::derive_patched_content(
        &repo.info.root,
        relative,
        &metadata.base_blob,
        &patch_file,
    )?;
    repo.write_store_file(&content_path, &contents)
}

/// Smudge: merge the captured patched contents onto `upstream`.
///
/// Anything that goes wrong (uninitialized store, missing data, conflict)
/// passes `upstream` through unchanged. The driver must never fail a checkout.
pub fn smudge(repo: &Repo, relative: &str, upstream: &[u8]) -> Vec<u8> {
    let Ok(overrides) = repo.overrides() else {
        return upstream.to_vec();
    };
    let Some(entry) = shared_entry(&overrides, relative) else {
        return upstream.to_vec();
    };
    let patch_file = repo.layout.repo_dir.join(&entry.source);
    let Some(patched) = read_patch_content(&patch_file) else {
        return upstream.to_vec();
    };
    let Ok(metadata_path) = patches::metadata_path_for(&patch_file) else {
        return upstream.to_vec();
    };
    let Ok(metadata) = patches::read_metadata(&metadata_path) else {
        return upstream.to_vec();
    };
    let Ok(base) = patches::blob_content(&repo.info.root, &metadata.base_blob) else {
        return upstream.to_vec();
    };

    if patches::looks_binary(&base)
        || patches::looks_binary(&patched)
        || patches::looks_binary(upstream)
    {
        return upstream.to_vec();
    }

    three_way_merge(&base, upstream, &patched).unwrap_or_else(|| upstream.to_vec())
}

/// Clean: reverse the stored patch out of `content`.
///
/// When the content already equals the index entry there is nothing to strip;
/// passing it through keeps deferred patches (`smudge` returned upstream)
/// clean instead of dirty.
pub fn clean(repo: &Repo, relative: &str, content: &[u8]) -> Vec<u8> {
    if let Ok(Some(index)) = patches::index_content(&repo.info.root, relative) {
        if index == content {
            return content.to_vec();
        }
    }

    let Ok(overrides) = repo.overrides() else {
        return content.to_vec();
    };
    let Some(entry) = shared_entry(&overrides, relative) else {
        return content.to_vec();
    };

    // base = patched, ours = worktree, theirs = upstream base.
    reverse(repo, entry, content).unwrap_or_else(|| content.to_vec())
}

/// Reverse the stored patch out of `content`, returning `None` on conflict.
pub fn reverse(repo: &Repo, entry: &OverrideEntry, content: &[u8]) -> Option<Vec<u8>> {
    let patch_file = repo.layout.repo_dir.join(&entry.source);
    let patched = read_patch_content(&patch_file)?;
    let metadata = patches::read_metadata(&patches::metadata_path_for(&patch_file).ok()?).ok()?;
    let base = patches::blob_content(&repo.info.root, &metadata.base_blob).ok()?;

    if patches::looks_binary(content) {
        return None;
    }

    three_way_merge(&patched, content, &base)
}

/// Three-way merge in a scratch directory, returning `None` on conflicts.
fn three_way_merge(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    let dir = temp_dir("merge")?;
    let result = (|| {
        let base_path = dir.join("base");
        let ours_path = dir.join("ours");
        let theirs_path = dir.join("theirs");
        fs::write(&base_path, base).ok()?;
        fs::write(&ours_path, ours).ok()?;
        fs::write(&theirs_path, theirs).ok()?;

        let output = Command::new("git")
            .args([
                "merge-file",
                "-p",
                ours_path.to_str()?,
                base_path.to_str()?,
                theirs_path.to_str()?,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if output.status.success() {
            Some(output.stdout)
        } else {
            None
        }
    })();

    let _ = fs::remove_dir_all(&dir);
    result
}

fn temp_dir(label: &str) -> Option<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "breeze-filter-{label}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Dry-run the smudge merge for one patch against the index of `dir`.
pub fn forward_merge(
    repo: &Repo,
    dir: &Path,
    relative: &str,
    entry: &OverrideEntry,
) -> MergeOutcome {
    let patch_file = repo.layout.repo_dir.join(&entry.source);
    let Some(patched) = read_patch_content(&patch_file) else {
        return MergeOutcome::Unavailable;
    };
    let Ok(metadata_path) = patches::metadata_path_for(&patch_file) else {
        return MergeOutcome::Unavailable;
    };
    let Ok(metadata) = patches::read_metadata(&metadata_path) else {
        return MergeOutcome::Unavailable;
    };
    let Ok(base) = patches::blob_content(&repo.info.root, &metadata.base_blob) else {
        return MergeOutcome::Unavailable;
    };
    let upstream = match patches::index_content(dir, relative) {
        Ok(Some(content)) => content,
        // The path is not present in this worktree's index; there is nothing
        // for the filter to materialize here.
        Ok(None) => return MergeOutcome::Clean,
        Err(_) => return MergeOutcome::Unavailable,
    };

    if patches::looks_binary(&base)
        || patches::looks_binary(&patched)
        || patches::looks_binary(&upstream)
    {
        return MergeOutcome::Unavailable;
    }

    match three_way_merge(&base, &upstream, &patched) {
        Some(_) => MergeOutcome::Clean,
        None => MergeOutcome::Deferred,
    }
}

/// Patches whose smudge merge currently conflicts with upstream.
pub fn deferred_paths(repo: &Repo) -> Result<Vec<String>> {
    let overrides = repo.overrides()?;
    let mut deferred = Vec::new();
    for relative in expected_paths(&overrides) {
        let Some(entry) = shared_entry(&overrides, &relative) else {
            continue;
        };
        if forward_merge(repo, &repo.info.root, &relative, entry) == MergeOutcome::Deferred {
            deferred.push(relative);
        }
    }
    Ok(deferred)
}

/// Whether the working tree in `dir` holds changes the patch did not put there.
///
/// Reversing the patch out of the current content and comparing with the index
/// separates a genuine manual edit from the patch itself. Missing data is
/// treated as diverged so destructive commands require `--force`.
pub fn manual_diverged(
    repo: &Repo,
    dir: &Path,
    relative: &str,
    entry: &OverrideEntry,
) -> Result<bool> {
    let Ok(content) = fs::read(dir.join(relative)) else {
        return Ok(false);
    };
    let Some(index) = patches::index_content(dir, relative)? else {
        return Ok(true);
    };
    if content == index {
        return Ok(false);
    }

    let patch_file = repo.layout.repo_dir.join(&entry.source);
    let Some(patched) = read_patch_content(&patch_file) else {
        return Ok(true);
    };
    let Ok(metadata) = patches::read_metadata(&patches::metadata_path_for(&patch_file)?) else {
        return Ok(true);
    };
    let Ok(base) = patches::blob_content(&repo.info.root, &metadata.base_blob) else {
        return Ok(true);
    };
    if patches::looks_binary(&content) {
        return Ok(true);
    }

    match three_way_merge(&patched, &content, &base) {
        Some(reversed) => Ok(reversed != index),
        None => Ok(true),
    }
}

/// Remove the filter config and attributes block.
///
/// Returns the paths that were registered so callers can re-check them out.
pub fn disable(repo: &Repo) -> Result<Vec<String>> {
    let paths = registered_paths(repo)?;
    disable_config(repo)?;
    attributes::write_paths(&attributes_file(repo), &[])?;
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quotes_paths_with_spaces() {
        assert_eq!(
            shell_quote("/usr/local/bin/breeze"),
            "/usr/local/bin/breeze"
        );
        assert_eq!(
            shell_quote("/Applications/Breeze Tools/breeze"),
            "'/Applications/Breeze Tools/breeze'"
        );
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn three_way_merge_outputs_clean_result_and_rejects_conflicts() {
        let merged = three_way_merge(
            b"one\ntwo\nthree\n",
            b"one\nTWO\nthree\n",
            b"one\ntwo\nthree\nlocal\n",
        );
        assert_eq!(merged, Some(b"one\nTWO\nthree\nlocal\n".to_vec()));

        let conflict = three_way_merge(
            b"one\nbase\nthree\n",
            b"one\nours\nthree\n",
            b"one\ntheirs\nthree\n",
        );
        assert_eq!(conflict, None);
    }
}
