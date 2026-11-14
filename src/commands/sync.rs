//! `breeze sync` — apply stored patches and repair overlay symlinks.
//! `--links-only` repairs links without touching tracked patches; the
//! post-checkout hook uses it so internal Git checkouts cannot dirty a worktree.

use std::collections::BTreeSet;
use std::fs;

use anyhow::Result;

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

    if !args.links_only {
        for path in &entry_paths(&patches) {
            let path_entries: Vec<&OverrideEntry> =
                patches.iter().filter(|entry| &entry.path == path).collect();
            for target in &targets {
                match sync_patch(&repo, path, &path_entries, target, args.dry_run) {
                    Ok(PatchChange::Unchanged) => {}
                    Ok(PatchChange::Applied) => changes += 1,
                    Ok(PatchChange::Conflict) => conflicts += 1,
                    Err(err) => {
                        eprintln!("error: failed to sync `{path}`: {err:#}");
                        failures += 1;
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
    Conflict,
}

fn sync_patch(
    repo: &Repo,
    path: &str,
    entries: &[&OverrideEntry],
    target: &overlay::WorktreeTarget,
    dry_run: bool,
) -> Result<PatchChange> {
    let Some(entry) = overlay::best_entry(entries.iter().copied(), target) else {
        return Ok(PatchChange::Unchanged);
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

    let file = target.path.join(path);
    let current_hash = patches::file_hash(&file);
    if current_hash.as_deref() == Some(metadata.applied_hash.as_str()) {
        return Ok(PatchChange::Unchanged);
    }

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
}
