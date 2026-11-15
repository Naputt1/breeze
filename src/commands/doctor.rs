//! `breeze doctor` — check the store and overlays for problems.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::DoctorArgs;
use crate::commands::Ctx;
use crate::exit;
use crate::store::exclude;
use crate::store::overlay::{self, Layer, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode, OverridesFile};
use crate::store::patches;

pub fn run(ctx: &Ctx, args: &DoctorArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let overrides = repo.overrides()?;

    let links: Vec<&OverrideEntry> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.mode == OverrideMode::Link)
        .collect();

    let targets = repo.worktree_targets(false);
    let mut paths: Vec<&str> = links.iter().map(|entry| entry.path.as_str()).collect();
    paths.sort_unstable();
    paths.dedup();

    let mut problems = 0usize;
    let mut fixed = 0usize;

    for path in paths {
        let path_entries: Vec<&OverrideEntry> = links
            .iter()
            .copied()
            .filter(|entry| entry.path == path)
            .collect();

        for target in &targets {
            let Some(entry) = overlay::best_entry(path_entries.iter().copied(), target) else {
                continue;
            };
            let store_file = repo.layout.repo_dir.join(&entry.source);
            let link = target.path.join(path);

            if !Repo::is_regular_store_file(&store_file) {
                println!(
                    "BROKEN {path} store copy is missing or is not a regular file: {}",
                    store_file.display()
                );
                problems += 1;
                continue;
            }

            match fs::symlink_metadata(&link) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    if overlay::link_points_to(&link, &store_file) {
                        println!("OK {path} linked in {}", target.path.display());
                    } else if args.fix {
                        fs::remove_file(&link)
                            .with_context(|| format!("failed to remove {}", link.display()))?;
                        overlay::create_relative_symlink(&link, &store_file)?;
                        println!("FIXED {path} repaired link in {}", target.path.display());
                        fixed += 1;
                    } else {
                        println!(
                            "BROKEN {path} link does not point at the store copy in {}",
                            target.path.display()
                        );
                        problems += 1;
                    }
                }
                Ok(metadata) if metadata.is_file() => {
                    let contents = fs::read(&link)
                        .with_context(|| format!("failed to read {}", link.display()))?;
                    let stored = fs::read(&store_file)
                        .with_context(|| format!("failed to read {}", store_file.display()))?;
                    if contents == stored && args.fix {
                        fs::remove_file(&link)
                            .with_context(|| format!("failed to remove {}", link.display()))?;
                        overlay::create_relative_symlink(&link, &store_file)?;
                        println!(
                            "FIXED {path} converted matching real file to a link in {}",
                            target.path.display()
                        );
                        fixed += 1;
                    } else if contents == stored {
                        println!(
                            "WARN {path} real file matches the store copy in {}; run `breeze doctor --fix` to link it",
                            target.path.display()
                        );
                        problems += 1;
                    } else {
                        println!(
                            "CONFLICT {path} real file differs from the store copy in {}",
                            target.path.display()
                        );
                        problems += 1;
                    }
                }
                Ok(_) => {
                    println!(
                        "WARN {path} is not a regular file in {}",
                        target.path.display()
                    );
                    problems += 1;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if args.fix {
                        overlay::ensure_link_parent(&link)?;
                        overlay::create_relative_symlink(&link, &store_file)?;
                        println!(
                            "FIXED {path} created missing link in {}",
                            target.path.display()
                        );
                        fixed += 1;
                    } else {
                        println!("BROKEN {path} link is missing in {}", target.path.display());
                        problems += 1;
                    }
                }
                Err(err) => {
                    println!("WARN {path} cannot be inspected: {err}");
                    problems += 1;
                }
            }
        }
    }

    problems += check_patches(&repo, &overrides)?;

    let known: HashSet<String> = overrides
        .overrides
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    for stale in exclude::stale_paths(&repo.exclude_path(), &known)? {
        if args.fix {
            exclude::remove_path(&repo.exclude_path(), &stale)?;
            println!("FIXED stale exclude entry removed: {stale}");
            fixed += 1;
        } else {
            println!("WARN stale exclude entry: {stale}");
            problems += 1;
        }
    }

    #[cfg(unix)]
    {
        let (perm_problems, perm_fixed) = check_permissions(&repo, args.fix)?;
        problems += perm_problems;
        fixed += perm_fixed;
    }

    for worktree in &repo.info.worktrees {
        if worktree.bare {
            continue;
        }
        if !worktree.path.exists() {
            println!(
                "WARN registered worktree directory is missing: {} (remove it with `breeze gc`)",
                worktree.path.display()
            );
            problems += 1;
        }
    }

    if links.is_empty() && overrides.overrides.is_empty() && problems == 0 {
        println!("OK no overlays recorded");
    }

    if fixed > 0 && !ctx.quiet {
        println!("repaired: {fixed} item(s)");
    }

    Ok(if problems > 0 {
        exit::GENERAL
    } else {
        exit::SUCCESS
    })
}

/// Check stored patches for missing data, duplicate layers and a stale base.
fn check_patches(repo: &Repo, overrides: &OverridesFile) -> Result<usize> {
    let mut grouped: BTreeMap<String, Vec<&OverrideEntry>> = BTreeMap::new();
    for entry in overrides
        .overrides
        .iter()
        .filter(|entry| entry.mode == OverrideMode::Patch)
    {
        grouped.entry(entry.path.clone()).or_default().push(entry);
    }

    let mut problems = 0usize;
    for (path, entries) in &grouped {
        let best = entries
            .iter()
            .max_by_key(|entry| Layer::from_name(&entry.layer));
        let label = best
            .map(|entry| entry.label())
            .unwrap_or_else(|| path.clone());

        if entries.len() > 1 {
            let layers: Vec<&str> = entries.iter().map(|entry| entry.layer.as_str()).collect();
            println!(
                "WARN {label} has duplicate patch overrides in layers {}; keep one layer or run `breeze promote {path}`",
                layers.join(", ")
            );
            problems += 1;
        }

        let Some(entry) = best else {
            continue;
        };
        let patch_file = repo.layout.repo_dir.join(&entry.source);
        if !Repo::is_regular_store_file(&patch_file) {
            println!(
                "BROKEN {label} patch file is missing or is not a regular file: {}",
                patch_file.display()
            );
            problems += 1;
            continue;
        }

        let meta_path = match patches::metadata_path_for(&patch_file) {
            Ok(path) => path,
            Err(err) => {
                println!("BROKEN {label} patch metadata path is invalid: {err}");
                problems += 1;
                continue;
            }
        };
        let metadata = match patches::read_metadata(&meta_path) {
            Ok(metadata) => metadata,
            Err(_) => {
                println!(
                    "BROKEN {label} patch metadata is missing or invalid: {}",
                    meta_path.display()
                );
                problems += 1;
                continue;
            }
        };

        let object_ok = patches::blob_exists(&repo.info.root, &metadata.base_blob).unwrap_or(false);
        let head_blob = patches::head_blob(&repo.info.root, path)?;
        let head_matches = head_blob.as_deref() == Some(metadata.base_blob.as_str());
        if !object_ok || !head_matches {
            let base = patches::short(&metadata.base_blob);
            let current = head_blob
                .as_deref()
                .map(patches::short)
                .unwrap_or("(missing at HEAD)");
            println!(
                "STALE {label} patch base {base} no longer matches HEAD {current}; re-run `breeze edit {path}`"
            );
            problems += 1;
        }
    }

    Ok(problems)
}

#[cfg(unix)]
fn check_permissions(repo: &Repo, fix: bool) -> Result<(usize, usize)> {
    use std::os::unix::fs::PermissionsExt;

    let mut directories = vec![repo.layout.repo_dir.clone()];
    let mut files: Vec<PathBuf> = Vec::new();
    collect(&repo.layout.repo_dir, &mut directories, &mut files);

    let mut problems = 0;
    let mut fixed = 0;

    for dir in directories {
        let Ok(metadata) = fs::symlink_metadata(&dir) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            if fix {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
                println!("FIXED {} permissions {mode:04o} -> 0700", dir.display());
                fixed += 1;
            } else {
                println!(
                    "WARN {} permissions {mode:04o} (expected 0700)",
                    dir.display()
                );
                problems += 1;
            }
        }
    }

    for file in files {
        let Ok(metadata) = fs::symlink_metadata(&file) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o600 {
            if fix {
                fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
                println!("FIXED {} permissions {mode:04o} -> 0600", file.display());
                fixed += 1;
            } else {
                println!(
                    "WARN {} permissions {mode:04o} (expected 0600)",
                    file.display()
                );
                problems += 1;
            }
        }
    }

    Ok((problems, fixed))
}

fn collect(dir: &Path, directories: &mut Vec<PathBuf>, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
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
            directories.push(path.clone());
            collect(&path, directories, files);
        } else if metadata.is_file() {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::store::overlay::testutil::TempRepo;

    #[test]
    fn doctor_is_healthy_after_link_and_fixes_a_broken_link() {
        let repo = TempRepo::new("doctor-link");
        repo.write("config/local.toml", "content\n");
        repo.breeze_ok(&["link", "config/local.toml"]);

        repo.breeze_ok(&["doctor"]);

        let other_link = repo.other.join("config/local.toml");
        fs::remove_file(&other_link).unwrap();
        std::os::unix::fs::symlink("/nonexistent/breeze", &other_link).unwrap();

        let (code, _) = repo.breeze_err(&["doctor"]);
        assert_eq!(code, 1);

        let output = repo.breeze_ok(&["doctor", "--fix"]);
        assert!(output.contains("FIXED"), "{output}");
        assert!(std::fs::symlink_metadata(&other_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            other_link.canonicalize().unwrap(),
            repo.store_dir()
                .join("shared/files/config/local.toml")
                .canonicalize()
                .unwrap()
        );

        repo.breeze_ok(&["doctor"]);
    }

    #[test]
    fn doctor_flags_a_stale_patch_base_after_amend() {
        let repo = TempRepo::new("doctor-stale");
        crate::commands::edit::tests::commit_file(&repo, "fog.json", "base\none\n");
        let editor = crate::commands::edit::tests::write_editor(
            &repo,
            "doctor-stale-edit",
            "printf 'base\\nlocal\\n' > \"$1\"",
        );
        repo.breeze_ok(&["edit", "fog.json", "--editor", &editor]);
        repo.breeze_ok(&["doctor"]);

        // Rewrite the commit under the patch, so the recorded base blob no
        // longer matches the version at HEAD.
        repo.write("fog.json", "base\namended\n");
        repo.git(&repo.main, &["add", "fog.json"]);
        repo.git(&repo.main, &["commit", "--amend", "--no-edit"]);

        let output = repo.breeze(&["doctor"]);
        assert_eq!(output.status.code(), Some(1));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("STALE fog.json"), "{stdout}");
        assert!(stdout.contains("re-run `breeze edit fog.json`"), "{stdout}");
    }

    #[test]
    fn doctor_reports_conflicts_and_repairs_permissions() {
        let repo = TempRepo::new("doctor-perms");
        repo.write("data.txt", "content\n");
        repo.breeze_ok(&["link", "data.txt"]);

        let store_file = repo.store_dir().join("shared/files/data.txt");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&store_file, fs::Permissions::from_mode(0o644)).unwrap();
        }

        let output = repo.breeze_ok(&["doctor", "--fix"]);
        assert!(output.contains("permissions"), "{output}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&store_file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let other_path = repo.other.join("data.txt");
        fs::remove_file(&other_path).unwrap();
        fs::write(&other_path, "different\n").unwrap();

        let (code, _) = repo.breeze_err(&["doctor"]);
        assert_eq!(code, 1);
        let output = repo.breeze(&["doctor", "--fix"]);
        assert_eq!(output.status.code(), Some(1));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("CONFLICT"), "{stdout}");
        assert_eq!(fs::read_to_string(&other_path).unwrap(), "different\n");
    }
}
