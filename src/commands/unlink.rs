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

    let mut overrides = repo.overrides()?;
    let entries: Vec<OverrideEntry> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.path == relative && entry.mode == OverrideMode::Link)
        .cloned()
        .collect();

    if entries.is_empty() {
        return Err(CommandError::new(
            exit::USAGE,
            format!("no link overlay recorded for `{relative}`"),
        )
        .into());
    }

    struct Restore {
        target: PathBuf,
        store: PathBuf,
        backup: bool,
    }

    let targets = repo.worktree_targets(false);
    let mut plan: Vec<Restore> = Vec::new();

    for target in &targets {
        let Some(entry) = overlay::best_entry(entries.iter(), target) else {
            continue;
        };
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

    overrides
        .overrides
        .retain(|entry| !(entry.path == relative && entry.mode == OverrideMode::Link));
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
}
