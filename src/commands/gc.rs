//! `breeze gc` — remove store data no live worktree uses.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::GcArgs;
use crate::commands::Ctx;
use crate::exit;
use crate::fog::temp;
use crate::store::filters;
use crate::store::overlay::{self, Repo};
use crate::store::overrides::{OverrideMode, OverridesFile};
use crate::store::patches;

/// How many backup directories to keep with `--force`.
const KEEP_BACKUPS: usize = 5;

pub fn run(ctx: &Ctx, args: &GcArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let mut overrides = repo.overrides()?;
    let mut report: Vec<String> = Vec::new();
    let mut changed = false;

    let live_targets = repo.worktree_targets(false);
    let live_worktree_ids: BTreeSet<String> = live_targets.iter().map(|t| t.id.clone()).collect();
    let mut live_branch_ids: BTreeSet<String> = live_targets
        .iter()
        .filter_map(|t| t.branch_id.clone())
        .collect();
    if let Some(branch_id) = &repo.branch_id {
        live_branch_ids.insert(branch_id.clone());
    }

    // Registered worktrees git still lists but whose directory is gone.
    for worktree in &repo.info.worktrees {
        if worktree.bare || worktree.path.exists() {
            continue;
        }
        report.push(format!(
            "registered worktree is gone: {}",
            worktree.path.display()
        ));
    }

    // Worktree layers without a live worktree (deleted or pruned from git).
    for dir in subdirs(&repo.layout.worktrees_dir)? {
        let Some(name) = dir_name(&dir) else {
            continue;
        };
        if live_worktree_ids.contains(&name) {
            continue;
        }
        changed |= remove_layer(&mut overrides, &dir, "worktrees", &name, &mut report)?;
    }

    // Branch layers no live worktree uses. The current branch is never touched.
    for dir in subdirs(&repo.layout.branches_dir)? {
        let Some(name) = dir_name(&dir) else {
            continue;
        };
        if live_branch_ids.contains(&name) {
            continue;
        }
        if repo.branch_id.as_deref() == Some(name.as_str()) {
            continue;
        }
        changed |= remove_layer(&mut overrides, &dir, "branches", &name, &mut report)?;
    }

    // Stale temp fog configs for this repository.
    let cache_dir = temp::cache_root()?.join(&repo.repo_id);
    let pruned = temp::prune_stale(&cache_dir);
    if pruned > 0 {
        report.push(format!(
            "removed {pruned} stale temp fog config(s) from {}",
            cache_dir.display()
        ));
    }

    let recorded: BTreeSet<String> = overrides
        .overrides
        .iter()
        .map(|entry| entry.source.clone())
        .collect();

    changed |= clean_orphan_patches(&repo, &recorded, &mut report)?;
    report_archives(&repo, &mut report);

    // Store files that no live link override points at.
    let live_files: BTreeSet<String> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.mode == OverrideMode::Link)
        .map(|entry| entry.source.clone())
        .collect();
    for dir in file_dirs(&repo)? {
        for file in files_under(&dir)? {
            let source = overlay::relative_string(&repo.layout.repo_dir, &file);
            if live_files.contains(&source) {
                continue;
            }
            if args.force {
                fs::remove_file(&file)
                    .with_context(|| format!("failed to remove {}", file.display()))?;
                repo.prune_empty_parents(&file);
                report.push(format!("removed unused store file {source}"));
            } else {
                report.push(format!(
                    "unused store file {source} (kept; rerun with --force to remove)"
                ));
            }
        }
    }

    if args.force {
        prune_backups(&repo, &mut report)?;
    }

    remove_empty_dirs(&repo, &mut report)?;

    if changed {
        repo.save_overrides(&overrides)?;
        if filters::enabled(&repo.info.root) {
            filters::sync_attributes(&repo, &overrides)?;
        }
    }
    repo.log_event("gc", &format!("{} action(s)", report.len()))?;

    if !ctx.quiet {
        if report.is_empty() {
            println!("gc: nothing to remove");
        } else {
            for line in &report {
                println!("{line}");
            }
        }
    }

    Ok(exit::SUCCESS)
}

fn dir_name(dir: &Path) -> Option<String> {
    dir.file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string)
}

/// Remove one dead layer directory and its override entries.
fn remove_layer(
    overrides: &mut OverridesFile,
    dir: &Path,
    kind: &str,
    id: &str,
    report: &mut Vec<String>,
) -> Result<bool> {
    let prefix = format!("{kind}/{id}/");
    let before = overrides.overrides.len();
    overrides
        .overrides
        .retain(|entry| !entry.source.starts_with(&prefix));
    let dropped = before - overrides.overrides.len();

    fs::remove_dir_all(dir).with_context(|| format!("failed to remove {}", dir.display()))?;
    report.push(format!(
        "removed {kind}/{id} ({dropped} override entr{} dropped)",
        if dropped == 1 { "y" } else { "ies" }
    ));
    Ok(true)
}

/// Child directories of `dir`, ignoring files and symlinks.
fn subdirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if fs::symlink_metadata(&path)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Regular files below `dir`, recursively.
fn files_under(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            out.extend(files_under(&path)?);
        } else if metadata.is_file() {
            out.push(path);
        }
    }
    Ok(out)
}

/// Every `files/` directory that can hold link overlays.
fn file_dirs(repo: &Repo) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![repo.layout.shared_dir.join("files")];
    for base in [&repo.layout.branches_dir, &repo.layout.worktrees_dir] {
        for layer in subdirs(base)? {
            dirs.push(layer.join("files"));
        }
    }
    Ok(dirs)
}

/// Patch files with no override entry. Patches with readable metadata are
/// reported and kept; only data without metadata is removed.
fn clean_orphan_patches(
    repo: &Repo,
    recorded: &BTreeSet<String>,
    report: &mut Vec<String>,
) -> Result<bool> {
    let mut dirs = vec![repo.layout.shared_dir.join("patches")];
    for base in [&repo.layout.branches_dir, &repo.layout.worktrees_dir] {
        for layer in subdirs(base)? {
            dirs.push(layer.join("patches"));
        }
    }

    let mut changed = false;
    for dir in dirs {
        for file in files_under(&dir)? {
            let is_patch = file
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext == "patch")
                .unwrap_or(false);
            let source = overlay::relative_string(&repo.layout.repo_dir, &file);

            if !is_patch {
                let extension = file.extension().and_then(|ext| ext.to_str());
                if extension == Some("json") && !file.with_extension("patch").exists() {
                    report.push(format!("orphan patch metadata kept: {source}"));
                } else if extension == Some("content") && !file.with_extension("patch").exists() {
                    // Captured patched contents are derived data; without the
                    // patch they can never be used again.
                    fs::remove_file(&file)
                        .with_context(|| format!("failed to remove {}", file.display()))?;
                    repo.prune_empty_parents(&file);
                    report.push(format!("removed orphan patch contents: {source}"));
                    changed = true;
                }
                continue;
            }

            if recorded.contains(&source) {
                continue;
            }

            let metadata = patches::metadata_path_for(&file)?;
            if patches::read_metadata(&metadata).is_err() {
                patches::remove_patch_files(&file)?;
                repo.prune_empty_parents(&file);
                report.push(format!("removed orphan patch without metadata: {source}"));
                changed = true;
            } else {
                report.push(format!("orphan patch kept (readable metadata): {source}"));
            }
        }
    }
    Ok(changed)
}

/// Report archived patches. They are never deleted automatically.
fn report_archives(repo: &Repo, report: &mut Vec<String>) {
    for dir in repo.all_layer_dirs() {
        for record in patches::list_archived(&dir) {
            let files = patches::archive_files(&dir, &record.name);
            let source = overlay::relative_string(&repo.layout.repo_dir, &files.patch);
            report.push(format!("archived patch kept: {source}"));
        }
    }
}

/// Keep the five most recent backup directories when `--force` is set.
fn prune_backups(repo: &Repo, report: &mut Vec<String>) -> Result<()> {
    let mut dirs = subdirs(&repo.layout.backups_dir)?;
    dirs.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    for dir in dirs.into_iter().skip(KEEP_BACKUPS) {
        fs::remove_dir_all(&dir).with_context(|| format!("failed to remove {}", dir.display()))?;
        report.push(format!("pruned old backup {}", dir.display()));
    }
    Ok(())
}

/// Remove empty directories below the repository store, keeping its roots.
fn remove_empty_dirs(repo: &Repo, report: &mut Vec<String>) -> Result<()> {
    let keep = [
        repo.layout.repo_dir.clone(),
        repo.layout.state_dir.clone(),
        repo.layout.shared_dir.clone(),
        repo.layout.branches_dir.clone(),
        repo.layout.worktrees_dir.clone(),
        repo.layout.backups_dir.clone(),
    ];

    let mut dirs = Vec::new();
    collect_dirs(&repo.layout.repo_dir, &mut dirs)?;
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));

    for dir in dirs {
        if keep.iter().any(|kept| kept == &dir) {
            continue;
        }
        let empty = fs::read_dir(&dir)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if empty && fs::remove_dir(&dir).is_ok() {
            report.push(format!(
                "removed empty directory {}",
                overlay::relative_string(&repo.layout.repo_dir, &dir)
            ));
        }
    }
    Ok(())
}

fn collect_dirs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if fs::symlink_metadata(&path)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            out.push(path.clone());
            collect_dirs(&path, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use crate::store::overlay::testutil::TempRepo;

    use crate::commands::edit::tests::{commit_file, write_editor};

    #[test]
    fn gc_reports_archived_patches_and_never_removes_them() {
        let repo = TempRepo::new("gc-archives");
        commit_file(&repo, "fog.json", "base\n");

        let editor = write_editor(&repo, "gc-archive-edit", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);
        repo.breeze_ok(&["revert", "--all"]);

        let archive = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("backups/patches/api-timeout.patch");
        let sidecar = archive.with_extension("json");
        assert!(archive.is_file());
        assert!(sidecar.is_file());

        let output = repo.breeze_ok(&["gc", "--force"]);
        assert!(output.contains("archived patch kept"), "{output}");
        assert!(archive.is_file(), "gc must not delete archives");
        assert!(sidecar.is_file(), "gc must not delete archive metadata");
    }

    #[test]
    fn gc_drops_layers_for_missing_worktrees_and_keeps_the_current_branch() {
        let repo = TempRepo::new("gc-worktrees");

        // A branch-layer link on the current branch must survive gc.
        repo.write("branch.txt", "branch\n");
        repo.breeze_ok(&["link", "branch.txt", "--layer", "branch"]);
        let main_branch_dir = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main));
        assert!(main_branch_dir.join("files/branch.txt").is_file());

        // A worktree-layer link in the second worktree.
        fs::write(repo.other.join("local.txt"), "other\n").unwrap();
        repo.breeze_ok_in(&repo.other, &["link", "local.txt", "--layer", "worktree"]);
        let other_id = repo.worktree_id(&repo.other);
        let other_layer = repo.store_dir().join("worktrees").join(&other_id);
        assert!(other_layer.join("files/local.txt").is_file());

        // Delete the worktree directory; git still lists it as prunable.
        fs::remove_dir_all(&repo.other).unwrap();
        let output = repo.breeze_ok(&["gc"]);
        assert!(output.contains("registered worktree is gone"), "{output}");

        assert!(!other_layer.exists(), "worktree layer should be removed");
        let overrides = fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(!overrides.contains("local.txt"), "{overrides}");
        assert!(overrides.contains("branch.txt"), "{overrides}");
        assert!(
            main_branch_dir.join("files/branch.txt").is_file(),
            "the current branch layer must never be removed"
        );
    }

    #[test]
    fn gc_removes_stale_temp_fog_configs() {
        let repo = TempRepo::new("gc-temp");
        let info = crate::gitinfo::GitInfo::discover(Some(&repo.main)).unwrap();
        let repo_id = crate::store::identity::repo_id(
            info.remote.as_deref(),
            &info.main_worktree,
            &info.common_dir,
        );
        let dir = repo
            .base
            .join("cache")
            .join("breeze")
            .join("fog")
            .join(&repo_id);
        fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("main-12345678-999999999.json");
        fs::write(&stale, "{}").unwrap();

        let status = Command::new("touch")
            .args(["-t", "202001010000"])
            .arg(&stale)
            .status()
            .expect("run touch");
        assert!(status.success(), "touch failed");

        let output = repo.breeze_ok(&["gc"]);
        assert!(output.contains("stale temp fog config"), "{output}");
        assert!(!stale.exists());
    }

    #[test]
    fn gc_removes_orphan_patches_without_metadata_but_keeps_readable_ones() {
        let repo = TempRepo::new("gc-orphans");
        let patches_dir = repo.store_dir().join("shared/patches");
        fs::create_dir_all(&patches_dir).unwrap();

        let bare = patches_dir.join("ghost.json.patch");
        fs::write(&bare, "diff --git a/ghost b/ghost\n").unwrap();

        let described = patches_dir.join("known.json.patch");
        fs::write(&described, "diff --git a/known b/known\n").unwrap();
        fs::write(
            patches_dir.join("known.json.json"),
            r#"{
  "path": "known.json",
  "base_sha": "deadbeef",
  "base_blob": "deadbeef",
  "applied_hash": "sha256:00",
  "created_at": "2026-01-01T00:00:00Z",
  "updated_at": "2026-01-01T00:00:00Z"
}"#,
        )
        .unwrap();

        let output = repo.breeze_ok(&["gc"]);
        assert!(
            output.contains("removed orphan patch without metadata"),
            "{output}"
        );
        assert!(
            output.contains("orphan patch kept (readable metadata)"),
            "{output}"
        );
        assert!(!bare.exists());
        assert!(described.exists());
    }

    #[test]
    fn gc_reports_unused_store_files_and_force_removes_them_and_old_backups() {
        let repo = TempRepo::new("gc-force");
        let unused = repo.store_dir().join("shared/files/leftover.txt");
        fs::create_dir_all(unused.parent().unwrap()).unwrap();
        fs::write(&unused, "unused\n").unwrap();

        let backups = repo.store_dir().join("backups");
        for index in 0..7 {
            let dir = backups.join(format!("2026-01-0{index}T00-00-00Z-11"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("file.txt"), "backup\n").unwrap();
        }

        let output = repo.breeze_ok(&["gc"]);
        assert!(output.contains("unused store file"), "{output}");
        assert!(output.contains("--force"), "{output}");
        assert!(unused.exists());
        assert_eq!(
            fs::read_dir(&backups)
                .unwrap()
                .filter(|e| e.as_ref().unwrap().path().is_dir())
                .count(),
            7,
            "backups are only pruned with --force"
        );

        let output = repo.breeze_ok(&["gc", "--force"]);
        assert!(output.contains("removed unused store file"), "{output}");
        assert!(output.contains("pruned old backup"), "{output}");
        assert!(!unused.exists());
        assert_eq!(
            fs::read_dir(&backups)
                .unwrap()
                .filter(|e| e.as_ref().unwrap().path().is_dir())
                .count(),
            5
        );
    }
}
