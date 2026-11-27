//! `breeze unlink` — restore real files and drop a link overlay.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::cli::UnlinkArgs;
use crate::commands::Ctx;
use crate::exit::{self, CommandError};
use crate::store::exclude;
use crate::store::overlay::{self, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode};

pub fn run(ctx: &Ctx, args: &UnlinkArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let relative = overlay::relative_lenient_path(&repo, &args.path)?;

    let current = repo.current_target();
    let mut overrides = repo.overrides()?;
    let all_entries: Vec<OverrideEntry> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.path == relative && entry.mode == OverrideMode::Link)
        .cloned()
        .collect();
    let entries: Vec<OverrideEntry> = all_entries
        .iter()
        .filter(|entry| overlay::entry_applies(entry, &current))
        .cloned()
        .collect();

    if entries.is_empty() {
        let message = if !all_entries.is_empty() {
            format!("no link overlay for `{relative}` applies to this branch or worktree")
        } else {
            format!("no link overlay recorded for `{relative}`")
        };
        return Err(CommandError::new(exit::USAGE, message).into());
    }

    struct Restore {
        target: PathBuf,
        store: PathBuf,
        backup: bool,
    }

    let targets = repo.worktree_targets(false);
    let mut plan: Vec<Restore> = Vec::new();

    for target in &targets {
        // Decide from every recorded link, not only the ones selected here;
        // a target whose effective link is another branch's must be left alone.
        let Some(entry) = overlay::best_entry(all_entries.iter(), target) else {
            continue;
        };
        if !entries
            .iter()
            .any(|selected| selected.source == entry.source)
        {
            continue;
        }
        let store = repo.layout.repo_dir.join(&entry.source);
        let path = target.path.join(&relative);

        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if !Repo::is_regular_store_file(&store) {
                    return Err(CommandError::new(
                        exit::GENERAL,
                        format!(
                            "store copy is missing or unsafe for `{relative}` ({}); cannot restore it",
                            store.display()
                        ),
                    )
                    .into());
                }
                plan.push(Restore {
                    target: path,
                    store,
                    backup: false,
                });
            }
            Ok(metadata) if metadata.is_file() => {
                if !Repo::is_regular_store_file(&store) {
                    continue;
                }
                let current = fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let stored = fs::read(&store)
                    .with_context(|| format!("failed to read {}", store.display()))?;
                if current == stored {
                    continue;
                }
                if !args.force {
                    return Err(CommandError::new(
                        exit::UNSAFE_REFUSED,
                        format!(
                            "`{}` differs from the store copy; rerun with `--force` to back it up and restore the store copy",
                            path.display()
                        ),
                    )
                    .into());
                }
                plan.push(Restore {
                    target: path,
                    store,
                    backup: true,
                });
            }
            Ok(_) => {
                return Err(overlay::unsafe_refused(format!(
                    "`{}` is not a regular file",
                    path.display()
                )));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if target.current && Repo::is_regular_store_file(&store) {
                    plan.push(Restore {
                        target: path,
                        store,
                        backup: false,
                    });
                }
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to inspect {}", path.display()));
            }
        }
    }

    let mut backups = Vec::new();
    for item in &plan {
        if item.backup {
            let backup = repo.backup_file(&item.target)?;
            backups.push(backup);
            fs::remove_file(&item.target)
                .with_context(|| format!("failed to remove {}", item.target.display()))?;
        }
        overlay::restore_copy(&item.store, &item.target)?;
    }

    overrides.overrides.retain(|entry| {
        !(entry.path == relative
            && entry.mode == OverrideMode::Link
            && overlay::entry_applies(entry, &current))
    });
    repo.save_overrides(&overrides)?;

    if !overrides
        .overrides
        .iter()
        .any(|entry| entry.path == relative)
    {
        exclude::remove_path(&repo.exclude_path(), &relative)?;
    }

    for entry in &entries {
        let store = repo.layout.repo_dir.join(&entry.source);
        if Repo::is_regular_store_file(&store) {
            fs::remove_file(&store)
                .with_context(|| format!("failed to remove {}", store.display()))?;
            repo.prune_empty_parents(&store);
        }
    }

    repo.log_event("unlink", &relative)?;

    if !ctx.quiet {
        println!("unlinked {relative}");
        println!("restored: {} worktree file(s)", plan.len());
        for backup in &backups {
            println!("backup:   {}", backup.display());
        }
    }

    Ok(exit::SUCCESS)
}

#[cfg(test)]
mod tests {
    use crate::store::overlay::testutil::TempRepo;

    #[test]
    fn unlink_restores_real_files_and_cleans_state() {
        let repo = TempRepo::new("unlink-basic");
        repo.write("config/local.toml", "linked content\n");
        repo.breeze_ok(&["link", "config/local.toml"]);

        let store_file = repo.store_dir().join("shared/files/config/local.toml");
        assert!(store_file.is_file());

        repo.breeze_ok(&["unlink", "config/local.toml"]);

        for root in [&repo.main, &repo.other] {
            let path = root.join("config/local.toml");
            assert!(path.is_file(), "missing {}", path.display());
            assert!(!std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "linked content\n");
        }

        assert!(!store_file.exists());
        let exclude = std::fs::read_to_string(repo.main.join(".git/info/exclude")).unwrap();
        assert!(!exclude.contains("config/local.toml"));

        let overrides =
            std::fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(!overrides.contains("config/local.toml"));
    }

    #[test]
    fn unlink_refuses_conflicting_real_files_unless_forced() {
        let repo = TempRepo::new("unlink-force");
        repo.write("data.txt", "store content\n");
        repo.breeze_ok(&["link", "data.txt"]);

        let other_path = repo.other.join("data.txt");
        std::fs::remove_file(&other_path).unwrap();
        std::fs::write(&other_path, "local edits\n").unwrap();

        let (code, stderr) = repo.breeze_err(&["unlink", "data.txt"]);
        assert_eq!(code, 5, "{stderr}");
        assert!(stderr.contains("--force"), "{stderr}");

        repo.breeze_ok(&["unlink", "data.txt", "--force"]);
        assert_eq!(
            std::fs::read_to_string(&other_path).unwrap(),
            "store content\n"
        );

        let backups = repo.store_dir().join("backups");
        let mut found = false;
        for entry in std::fs::read_dir(&backups).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                for file in std::fs::read_dir(entry.path()).unwrap() {
                    if file.unwrap().file_name() == "data.txt" {
                        found = true;
                    }
                }
            }
        }
        assert!(found, "expected a backup of the conflicting file");
    }

    #[test]
    fn unlink_without_an_override_is_a_usage_error() {
        let repo = TempRepo::new("unlink-missing");
        let (code, stderr) = repo.breeze_err(&["unlink", "nope.txt"]);
        assert_eq!(code, 2);
        assert!(stderr.contains("no link overlay"), "{stderr}");
    }

    #[test]
    fn unlink_leaves_links_recorded_for_other_branches() {
        let repo = TempRepo::new("unlink-branch-scope");
        repo.write("config/local.toml", "main content\n");
        repo.breeze_ok(&["link", "config/local.toml", "--layer", "branch"]);
        let main_branch = repo.branch_id(&repo.main);
        let main_store = repo
            .store_dir()
            .join("branches")
            .join(&main_branch)
            .join("files/config/local.toml");

        repo.git(&repo.main, &["switch", "-c", "side"]);
        let side_branch = repo.branch_id(&repo.main);
        let side_store = repo
            .store_dir()
            .join("branches")
            .join(&side_branch)
            .join("files/config/local.toml");
        // Replace the carried symlink so the side branch gets its own copy.
        std::fs::remove_file(repo.main.join("config/local.toml")).unwrap();
        std::fs::write(repo.main.join("config/local.toml"), "side content\n").unwrap();
        repo.breeze_ok(&["link", "config/local.toml", "--layer", "branch"]);
        repo.git(&repo.main, &["switch", "main"]);
        repo.breeze_ok(&["sync"]);

        repo.breeze_ok(&["unlink", "config/local.toml"]);

        assert_eq!(repo.read(&repo.main, "config/local.toml"), "main content\n");
        assert!(!main_store.exists());
        assert!(side_store.is_file());
        assert_eq!(
            std::fs::read_to_string(&side_store).unwrap(),
            "side content\n"
        );
        let overrides =
            std::fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(!overrides.contains(&main_branch), "{overrides}");
        assert!(overrides.contains(&side_branch), "{overrides}");
    }

    #[test]
    fn unlink_does_not_replace_another_branchs_effective_link() {
        let repo = TempRepo::new("unlink-shared-vs-branch");
        repo.write("config/local.toml", "shared content\n");
        repo.breeze_ok(&["link", "config/local.toml", "--layer", "shared"]);

        let side = repo.base.join("side");
        repo.git(
            &repo.main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                side.to_str().unwrap(),
            ],
        );
        std::fs::create_dir_all(side.join("config")).unwrap();
        std::fs::write(side.join("config/local.toml"), "side content\n").unwrap();
        repo.breeze_ok_in(&side, &["link", "config/local.toml", "--layer", "branch"]);
        let side_branch = repo.branch_id(&side);
        let side_store = repo
            .store_dir()
            .join("branches")
            .join(&side_branch)
            .join("files/config/local.toml");
        assert!(side_store.is_file());

        repo.breeze_ok(&["unlink", "config/local.toml"]);

        assert_eq!(
            repo.read(&repo.main, "config/local.toml"),
            "shared content\n"
        );
        let side_file = side.join("config/local.toml");
        assert!(
            std::fs::symlink_metadata(&side_file)
                .unwrap()
                .file_type()
                .is_symlink(),
            "side link was replaced"
        );
        assert_eq!(repo.read(&side, "config/local.toml"), "side content\n");
        assert!(side_store.is_file());
    }
}
