//! `breeze sync` — apply stored patches and repair overlay symlinks.
//! `--links-only` repairs links without touching tracked patches; the
//! post-checkout hook uses it so internal Git checkouts cannot dirty a worktree.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::SyncArgs;
use crate::commands::Ctx;
use crate::exit;
use crate::store::now_rfc3339;
use crate::store::overlay::{self, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode};
use crate::store::patches;

pub fn run(ctx: &Ctx, args: &SyncArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let overrides = repo.overrides()?;
    let (mut links, mut patches): (Vec<OverrideEntry>, Vec<OverrideEntry>) = overrides
        .overrides
        .into_iter()
        .partition(|entry| entry.mode == OverrideMode::Link);

    let filter = match &args.path {
        Some(path) => Some(overlay::relative_lenient_path(&repo, path)?),
        None => None,
    };
    if let Some(path) = &filter {
        links.retain(|entry| &entry.path == path);
        patches.retain(|entry| &entry.path == path);
        if links.is_empty() && patches.is_empty() {
            if !ctx.quiet {
                println!("no overlays recorded for {path}");
            }
            return Ok(exit::SUCCESS);
        }
    }

    if links.is_empty() && patches.is_empty() {
        if !ctx.quiet {
            println!("no overlays recorded");
        }
        return Ok(exit::SUCCESS);
    }

    if args.links_only && links.is_empty() {
        if !ctx.quiet {
            println!("no link overlays recorded");
        }
        return Ok(exit::SUCCESS);
    }

    let targets = if args.all {
        repo.worktree_targets(false)
    } else {
        vec![repo.current_target()]
    };

    let mut failures = 0usize;
    let mut conflicts = 0usize;
    let mut changes = 0usize;
    let filter_mode = crate::store::filters::enabled(&repo.info.root);

    if !args.links_only {
        if filter_mode {
            // Filters do the applying. Sync only dry-runs the smudge merge so
            // deferred patches are visible after a pull or merge.
            for path in &entry_paths(&patches) {
                let path_entries: Vec<&OverrideEntry> =
                    patches.iter().filter(|entry| &entry.path == path).collect();
                for target in &targets {
                    let Some(entry) = overlay::best_entry(path_entries.iter().copied(), target)
                    else {
                        continue;
                    };
                    match crate::store::filters::forward_merge(&repo, &target.path, path, entry) {
                        crate::store::filters::MergeOutcome::Clean => {}
                        crate::store::filters::MergeOutcome::Deferred => {
                            println!(
                                "DEFERRED {path} in {} (upstream content conflicts; patch not applied)",
                                target.path.display()
                            );
                            conflicts += 1;
                        }
                        crate::store::filters::MergeOutcome::Unavailable => {
                            eprintln!("error: patch data for `{path}` is unavailable; run `breeze doctor`");
                            failures += 1;
                        }
                    }
                }
            }
        } else {
            for path in &entry_paths(&patches) {
                let path_entries: Vec<&OverrideEntry> =
                    patches.iter().filter(|entry| &entry.path == path).collect();
                for target in &targets {
                    match sync_patch(
                        &repo,
                        path,
                        &path_entries,
                        target,
                        args.dry_run,
                        args.prune_inactive,
                    ) {
                        Ok(PatchChange::Unchanged) => {}
                        Ok(PatchChange::Applied) | Ok(PatchChange::Restored) => changes += 1,
                        Ok(PatchChange::Conflict) => conflicts += 1,
                        Err(err) => {
                            eprintln!("error: failed to sync `{path}`: {err:#}");
                            failures += 1;
                        }
                    }
                }
            }
        }
    }

    for path in &entry_paths(&links) {
        let path_entries: Vec<&OverrideEntry> =
            links.iter().filter(|entry| &entry.path == path).collect();

        for target in &targets {
            let Some(entry) = overlay::best_entry(path_entries.iter().copied(), target) else {
                continue;
            };
            let store_file = repo.layout.repo_dir.join(&entry.source);
            let link = target.path.join(path);

            if !Repo::is_regular_store_file(&store_file) {
                eprintln!(
                    "error: store copy for `{path}` is missing or is not a regular file ({})",
                    store_file.display()
                );
                failures += 1;
                continue;
            }

            match fs::symlink_metadata(&link) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    if overlay::link_points_to(&link, &store_file) {
                        continue;
                    }
                    if args.dry_run {
                        println!("would repair link {}", link.display());
                        changes += 1;
                        continue;
                    }
                    match fs::remove_file(&link)
                        .map_err(anyhow::Error::from)
                        .and_then(|_| overlay::create_relative_symlink(&link, &store_file))
                    {
                        Ok(()) => {
                            println!("repaired link {}", link.display());
                            changes += 1;
                        }
                        Err(err) => {
                            eprintln!("error: failed to repair {}: {err:#}", link.display());
                            failures += 1;
                        }
                    }
                }
                Ok(metadata) if metadata.is_file() => {
                    println!(
                        "WARN {}: real file is not a link; left untouched",
                        link.display()
                    );
                }
                Ok(_) => {
                    println!(
                        "WARN {}: not a regular file; left untouched",
                        link.display()
                    );
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if args.dry_run {
                        println!("would link {}", link.display());
                        changes += 1;
                        continue;
                    }
                    match overlay::ensure_link_parent(&link)
                        .and_then(|_| overlay::create_relative_symlink(&link, &store_file))
                    {
                        Ok(()) => {
                            println!("linked {}", link.display());
                            changes += 1;
                        }
                        Err(err) => {
                            eprintln!("error: failed to link {}: {err:#}", link.display());
                            failures += 1;
                        }
                    }
                }
                Err(err) => {
                    eprintln!("error: failed to inspect {}: {err}", link.display());
                    failures += 1;
                }
            }
        }
    }

    if changes == 0 && failures == 0 && conflicts == 0 && !ctx.quiet {
        println!("all overlays ok");
    }

    Ok(if conflicts > 0 {
        exit::PATCH_CONFLICT
    } else if failures > 0 {
        exit::GENERAL
    } else {
        exit::SUCCESS
    })
}

enum PatchChange {
    Unchanged,
    Applied,
    Restored,
    Conflict,
}

#[allow(clippy::too_many_arguments)]
fn sync_patch(
    repo: &Repo,
    path: &str,
    entries: &[&OverrideEntry],
    target: &overlay::WorktreeTarget,
    dry_run: bool,
    prune_inactive: bool,
) -> Result<PatchChange> {
    let file = target.path.join(path);
    let current_hash = patches::file_hash(&file);
    let best = overlay::best_entry(entries.iter().copied(), target);

    let Some(entry) = best else {
        if !prune_inactive {
            return Ok(PatchChange::Unchanged);
        }
        return if restore_inactive(
            repo,
            path,
            entries,
            None,
            current_hash.as_deref(),
            target,
            dry_run,
        )? {
            Ok(PatchChange::Restored)
        } else {
            Ok(PatchChange::Unchanged)
        };
    };

    let patch_file = repo.layout.repo_dir.join(&entry.source);
    if !Repo::is_regular_store_file(&patch_file) {
        anyhow::bail!(
            "store patch is missing or is not a regular file ({})",
            patch_file.display()
        );
    }
    let meta_file = patches::metadata_path_for(&patch_file)?;
    let metadata = patches::read_metadata(&meta_file)?;

    if current_hash.as_deref() == Some(metadata.applied_hash.as_str()) {
        return Ok(PatchChange::Unchanged);
    }

    // A branch switch can leave another layer's patch content in the file.
    // Restore the target's HEAD content before applying this patch so the
    // three-way merge starts from the branch's real base.
    restore_inactive(
        repo,
        path,
        entries,
        Some(entry),
        current_hash.as_deref(),
        target,
        dry_run,
    )?;

    if dry_run {
        println!("would apply patch {path} in {}", target.path.display());
        return Ok(PatchChange::Applied);
    }

    match patches::apply_patch(&target.path, &patch_file, path)? {
        patches::ApplyOutcome::Clean => {
            let applied = patches::file_hash(&file).unwrap_or(metadata.applied_hash.clone());
            let updated = patches::PatchMetadata {
                applied_hash: applied,
                updated_at: now_rfc3339(),
                ..metadata
            };
            patches::write_metadata(&meta_file, &updated)?;
            println!("applied patch {path} in {}", target.path.display());
            Ok(PatchChange::Applied)
        }
        patches::ApplyOutcome::Conflicted => {
            println!(
                "CONFLICT {path} in {} (conflict markers written)",
                target.path.display()
            );
            Ok(PatchChange::Conflict)
        }
    }
}

/// Restore `path` to the target's HEAD content when it still carries the
/// output of a patch other than `best`.
///
/// The hash comparison is deliberately exact: content that a user changed is
/// left alone. Returns whether the file was (or, in a dry run, would be)
/// restored.
fn restore_inactive(
    repo: &Repo,
    path: &str,
    entries: &[&OverrideEntry],
    best: Option<&OverrideEntry>,
    current_hash: Option<&str>,
    target: &overlay::WorktreeTarget,
    dry_run: bool,
) -> Result<bool> {
    let Some(current_hash) = current_hash else {
        return Ok(false);
    };
    if !carries_other_patch(repo, entries, best, current_hash) {
        return Ok(false);
    }

    let file = target.path.join(path);
    let metadata = match fs::symlink_metadata(&file) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(false);
    }
    // Never rewrite a worktree file that has staged changes.
    if has_staged_changes(&target.path, path)? {
        return Ok(false);
    }

    let Some(head_blob) = patches::head_blob(&target.path, path)? else {
        return Ok(false);
    };
    let head = patches::blob_content(&target.path, &head_blob)?;
    if patches::hash_bytes(&head) == current_hash {
        return Ok(false);
    }

    if dry_run {
        println!(
            "would restore {path} in {} (patch is not active here)",
            target.path.display()
        );
        return Ok(true);
    }

    patches::write_worktree_file(&file, &head)?;
    println!(
        "restored {path} in {} (patch is not active here)",
        target.path.display()
    );
    Ok(true)
}

/// Whether `current_hash` matches the recorded applied hash of a patch other
/// than `best`. With no `best`, every record counts.
fn carries_other_patch(
    repo: &Repo,
    entries: &[&OverrideEntry],
    best: Option<&OverrideEntry>,
    current_hash: &str,
) -> bool {
    entries.iter().any(|entry| {
        if best.is_some_and(|best| std::ptr::eq(*entry, best)) {
            return false;
        }
        let patch_file = repo.layout.repo_dir.join(&entry.source);
        if !Repo::is_regular_store_file(&patch_file) {
            return false;
        }
        let Ok(meta_file) = patches::metadata_path_for(&patch_file) else {
            return false;
        };
        let Ok(metadata) = patches::read_metadata(&meta_file) else {
            return false;
        };
        metadata.applied_hash == current_hash
    })
}

fn has_staged_changes(worktree: &Path, relative: &str) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["diff", "--cached", "--quiet", "--", relative])
        .output()
        .context("failed to run git diff --cached")?;
    Ok(!output.status.success())
}

fn entry_paths(entries: &[OverrideEntry]) -> BTreeSet<String> {
    entries.iter().map(|entry| entry.path.clone()).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::store::overlay::testutil::TempRepo;

    use crate::commands::edit::tests::{commit_file, write_editor};

    #[test]
    fn sync_repairs_missing_and_broken_links_and_dry_run_changes_nothing() {
        let repo = TempRepo::new("sync-repair");
        repo.write("config/local.toml", "content\n");
        repo.breeze_ok(&["link", "config/local.toml"]);

        let other_link = repo.other.join("config/local.toml");
        let store_file = repo.store_dir().join("shared/files/config/local.toml");
        std::fs::remove_file(&other_link).unwrap();

        let dry = repo.breeze_ok(&["sync", "--all", "--dry-run"]);
        assert!(dry.contains("would link"), "{dry}");
        assert!(!other_link.exists());

        repo.breeze_ok(&["sync", "--all"]);
        assert!(std::fs::symlink_metadata(&other_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_link(&other_link).unwrap().is_relative());
        assert_eq!(
            other_link.canonicalize().unwrap(),
            store_file.canonicalize().unwrap()
        );

        // Replace the link with a dangling one.
        std::fs::remove_file(&other_link).unwrap();
        std::os::unix::fs::symlink("/nonexistent/breeze", &other_link).unwrap();
        repo.breeze_ok(&["sync", "--all"]);
        assert_eq!(
            other_link.canonicalize().unwrap(),
            store_file.canonicalize().unwrap()
        );
    }

    #[test]
    fn sync_defaults_to_the_current_worktree_and_leaves_real_files_alone() {
        let repo = TempRepo::new("sync-current");
        repo.write("data.txt", "content\n");
        repo.breeze_ok(&["link", "data.txt"]);

        // A real file in the other worktree must be reported, not overwritten.
        let other_path = repo.other.join("data.txt");
        std::fs::remove_file(&other_path).unwrap();
        std::fs::write(&other_path, "local file\n").unwrap();

        let output = repo.breeze_ok(&["sync", "--all"]);
        assert!(output.contains("real file"), "{output}");
        assert_eq!(
            std::fs::read_to_string(&other_path).unwrap(),
            "local file\n"
        );

        // Default scope is the current worktree only: no error there.
        repo.breeze_ok(&["sync", "--path", "data.txt"]);
    }

    #[test]
    fn sync_applies_a_shared_patch_to_a_second_worktree_and_skips_when_current() {
        let repo = TempRepo::new("sync-patch-clean");
        commit_file(&repo, "fog.json", "one\ntwo\n");
        repo.git(&repo.other, &["reset", "--hard", "main"]);

        let editor = write_editor(
            &repo,
            "sync-patch-add",
            "printf 'one\\ntwo\\nlocal\\n' > \"$1\"",
        );
        repo.breeze_ok(&["edit", "fog.json", "--layer", "shared", "--editor", &editor]);

        let dry = repo.breeze_ok(&["sync", "--all", "--dry-run"]);
        assert!(dry.contains("would apply patch fog.json"), "{dry}");
        assert_eq!(repo.read(&repo.other, "fog.json"), "one\ntwo\n");

        let output = repo.breeze_ok(&["sync", "--all"]);
        assert!(output.contains("applied patch fog.json"), "{output}");
        assert_eq!(repo.read(&repo.other, "fog.json"), "one\ntwo\nlocal\n");

        // A second run is a no-op: the applied hash matches.
        let again = repo.breeze_ok(&["sync", "--all"]);
        assert!(again.contains("all overlays ok"), "{again}");
    }

    #[test]
    fn sync_writes_conflict_markers_and_exits_three_on_diverged_targets() {
        let repo = TempRepo::new("sync-patch-conflict");
        commit_file(&repo, "fog.json", "one\ntwo\nthree\nfour\n");
        repo.git(&repo.other, &["reset", "--hard", "main"]);

        let editor = write_editor(
            &repo,
            "sync-conflict-edit",
            "printf 'one\\ntwo-local\\nthree\\nfour\\n' > \"$1\"",
        );
        repo.breeze_ok(&["edit", "fog.json", "--layer", "shared", "--editor", &editor]);

        // Unrelated edit at the same location in the second worktree.
        fs::write(repo.other.join("fog.json"), "one\ntwo-other\nthree\nfour\n").unwrap();

        let (code, _stderr) = repo.breeze_err(&["sync", "--all"]);
        assert_eq!(code, 3);

        let contents = repo.read(&repo.other, "fog.json");
        assert!(contents.contains("<<<<<<<"), "{contents}");
        assert!(contents.contains(">>>>>>>"), "{contents}");
    }

    #[test]
    fn sync_links_only_repairs_links_without_applying_patches() {
        let repo = TempRepo::new("sync-links-only");
        commit_file(&repo, "fog.json", "one\ntwo\n");
        repo.git(&repo.other, &["reset", "--hard", "main"]);

        let editor = write_editor(
            &repo,
            "sync-links-only-add",
            "printf 'one\\ntwo\\nlocal\\n' > \"$1\"",
        );
        repo.breeze_ok(&["edit", "fog.json", "--layer", "shared", "--editor", &editor]);
        repo.write("config/local.toml", "local config\n");
        repo.breeze_ok(&["link", "config/local.toml"]);

        let other_link = repo.other.join("config/local.toml");
        std::fs::remove_file(&other_link).unwrap();
        assert_eq!(repo.read(&repo.other, "fog.json"), "one\ntwo\n");

        let output = repo.breeze_ok(&["sync", "--all", "--links-only"]);
        assert!(
            output.contains("linked") || output.contains("repaired"),
            "{output}"
        );
        assert!(std::fs::symlink_metadata(&other_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            repo.read(&repo.other, "fog.json"),
            "one\ntwo\n",
            "--links-only must not apply tracked patches"
        );

        // A full sync still applies the patch afterwards.
        repo.breeze_ok(&["sync", "--all"]);
        assert_eq!(repo.read(&repo.other, "fog.json"), "one\ntwo\nlocal\n");
    }

    #[test]
    fn sync_prune_inactive_restores_a_previous_branch_patch() {
        let repo = TempRepo::new("sync-prune-branch");
        commit_file(&repo, "fog.json", "base\n");
        let editor = write_editor(&repo, "sync-prune-main", "printf 'main-local\\n' > \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--layer", "branch", "--editor", &editor]);

        repo.git(&repo.main, &["branch", "side"]);
        repo.git(&repo.main, &["switch", "side"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "main-local\n");

        let dry = repo.breeze_ok(&["sync", "--dry-run", "--prune-inactive"]);
        assert!(dry.contains("would restore fog.json"), "{dry}");
        assert_eq!(repo.read(&repo.main, "fog.json"), "main-local\n");

        let output = repo.breeze_ok(&["sync", "--prune-inactive"]);
        assert!(output.contains("restored fog.json"), "{output}");
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\n");

        repo.git(&repo.main, &["switch", "main"]);
        repo.breeze_ok(&["sync"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "main-local\n");
    }

    #[test]
    fn sync_replaces_a_previous_branch_patch_with_the_target_patch() {
        let repo = TempRepo::new("sync-swap-branch");
        commit_file(&repo, "fog.json", "base\n");
        let main_editor = write_editor(&repo, "sync-swap-main", "printf 'main-local\\n' > \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--layer",
            "branch",
            "--editor",
            &main_editor,
        ]);

        repo.git(&repo.main, &["switch", "-c", "side"]);
        let side_editor = write_editor(&repo, "sync-swap-side", "printf 'side-local\\n' > \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--layer",
            "branch",
            "--editor",
            &side_editor,
        ]);
        repo.git(&repo.main, &["checkout", "--", "fog.json"]);
        repo.git(&repo.main, &["switch", "main"]);

        repo.breeze_ok(&["sync"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "main-local\n");

        repo.git(&repo.main, &["switch", "side"]);
        let output = repo.breeze_ok(&["sync"]);
        assert!(output.contains("applied patch fog.json"), "{output}");
        assert_eq!(repo.read(&repo.main, "fog.json"), "side-local\n");

        repo.git(&repo.main, &["switch", "main"]);
        repo.breeze_ok(&["sync"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "main-local\n");
    }

    #[test]
    fn sync_prune_inactive_leaves_diverged_and_staged_content_alone() {
        let repo = TempRepo::new("sync-prune-safe");
        commit_file(&repo, "fog.json", "base\n");
        let editor = write_editor(
            &repo,
            "sync-prune-safe-main",
            "printf 'main-local\\n' > \"$1\"",
        );
        repo.breeze_ok(&["edit", "fog.json", "--layer", "branch", "--editor", &editor]);
        repo.git(&repo.main, &["branch", "side"]);
        repo.git(&repo.main, &["switch", "side"]);

        repo.write("fog.json", "user-edit\n");
        repo.breeze_ok(&["sync", "--prune-inactive"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "user-edit\n");

        repo.write("fog.json", "main-local\n");
        repo.git(&repo.main, &["add", "fog.json"]);
        repo.breeze_ok(&["sync", "--prune-inactive"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "main-local\n");

        repo.git(&repo.main, &["reset", "HEAD", "--", "fog.json"]);
        repo.breeze_ok(&["sync", "--prune-inactive"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\n");
    }
}
