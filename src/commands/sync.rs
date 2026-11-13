//! `breeze sync` — repair overlay symlinks.

use std::collections::BTreeSet;
use std::fs;

use anyhow::Result;

use crate::cli::SyncArgs;
use crate::commands::Ctx;
use crate::exit;
use crate::store::overlay::{self, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode};

pub fn run(ctx: &Ctx, args: &SyncArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let overrides = repo.overrides()?;
    let mut entries: Vec<OverrideEntry> = overrides
        .overrides
        .into_iter()
        .filter(|entry| entry.mode == OverrideMode::Link)
        .collect();

    if let Some(path) = &args.path {
        let relative = overlay::relative_lenient_path(&repo, path)?;
        entries.retain(|entry| entry.path == relative);
        if entries.is_empty() {
            if !ctx.quiet {
                println!("no link overlays recorded for {relative}");
            }
            return Ok(exit::SUCCESS);
        }
    }

    if entries.is_empty() {
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

    let paths: BTreeSet<String> = entries.iter().map(|entry| entry.path.clone()).collect();
    let mut failures = 0;
    let mut changes = 0;

    for path in &paths {
        let path_entries: Vec<&OverrideEntry> =
            entries.iter().filter(|entry| &entry.path == path).collect();

        for target in &targets {
            let Some(entry) = overlay::best_entry(path_entries.iter().copied(), target) else {
                continue;
            };
            let store_file = repo.layout.repo_dir.join(&entry.source);
            let link = target.path.join(path);

            if !store_file.is_file() {
                eprintln!(
                    "error: store copy for `{path}` is missing ({})",
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

    if changes == 0 && failures == 0 && !ctx.quiet {
        println!("all links ok");
    }

    Ok(if failures > 0 {
        exit::GENERAL
    } else {
        exit::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use crate::store::overlay::testutil::TempRepo;

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
}
