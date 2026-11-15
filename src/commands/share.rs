//! `breeze share` — move a branch or worktree override up to the shared layer.

use std::fs;

use anyhow::{Context, Result};

use crate::cli::ShareArgs;
use crate::commands::Ctx;
use crate::exit::{self, CommandError};
use crate::store::now_rfc3339;
use crate::store::overlay::{self, Layer, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode};
use crate::store::patches;

pub fn run(ctx: &Ctx, args: &ShareArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let relative = overlay::relative_lenient_path(&repo, &args.path)?;
    let mut overrides = repo.overrides()?;
    let current = repo.current_target();

    let entries: Vec<OverrideEntry> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.path == relative)
        .cloned()
        .collect();

    if entries.is_empty() {
        return Err(CommandError::new(
            exit::USAGE,
            format!("no overlay recorded for `{relative}`; nothing to share"),
        )
        .into());
    }

    // Prefer the override that actually applies here; fall back to the highest
    // layer recorded for the path when none matches the current worktree.
    let chosen = match overlay::best_entry(entries.iter(), &current) {
        Some(entry) => entry.clone(),
        None => entries
            .iter()
            .max_by_key(|entry| Layer::from_name(&entry.layer))
            .cloned()
            .ok_or_else(|| {
                CommandError::new(
                    exit::GENERAL,
                    format!("could not resolve an overlay for `{relative}`"),
                )
            })?,
    };

    if chosen.layer == Layer::Shared.name() {
        if !ctx.quiet {
            println!("{relative} is already shared");
        }
        return Ok(exit::SUCCESS);
    }

    let now = now_rfc3339();
    let (source, moved_links) = match chosen.mode {
        OverrideMode::Link => (share_link(&repo, &relative, &chosen)?, true),
        OverrideMode::Patch => {
            let source = share_patch(&repo, &relative, &chosen)?;
            eprintln!(
                "warning: a shared patch applies to every branch and worktree on the next `breeze sync --all`"
            );
            (source, false)
        }
    };

    overrides.overrides.retain(|entry| {
        !(entry.path == relative && entry.mode == chosen.mode && entry.source == chosen.source)
    });

    let mut shared = chosen.clone();
    shared.layer = Layer::Shared.name().to_string();
    shared.source = source.clone();
    shared.updated_at = now;
    overrides.upsert(shared);
    repo.save_overrides(&overrides)?;

    let repaired = if moved_links {
        repair_links(&repo, &overrides, &relative, &source)?
    } else {
        0
    };

    repo.log_event("share", &relative)?;

    let shadowing: Vec<&str> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.path == relative && entry.layer != Layer::Shared.name())
        .map(|entry| entry.layer.as_str())
        .collect();
    if !shadowing.is_empty() {
        eprintln!(
            "warning: `{relative}` still has a {} layer override that shadows the shared copy",
            shadowing.join(", ")
        );
    }

    if !ctx.quiet {
        println!("shared {relative} (moved to the shared layer)");
        println!("store:  {}", repo.layout.repo_dir.join(&source).display());
        if chosen.mode == OverrideMode::Link {
            println!("links:  {repaired} worktree link(s) re-pointed");
        }
    }

    Ok(exit::SUCCESS)
}

/// Move the backing store file to `shared/files/`.
fn share_link(repo: &Repo, relative: &str, entry: &OverrideEntry) -> Result<String> {
    let old_file = repo.layout.repo_dir.join(&entry.source);
    if !Repo::is_regular_store_file(&old_file) {
        return Err(CommandError::new(
            exit::GENERAL,
            format!(
                "store copy is missing for `{relative}` ({}); cannot share it",
                old_file.display()
            ),
        )
        .into());
    }

    let shared_file = repo.store_file(Layer::Shared, relative)?;
    let contents =
        fs::read(&old_file).with_context(|| format!("failed to read {}", old_file.display()))?;

    if fs::symlink_metadata(&shared_file).is_ok() {
        if !Repo::is_regular_store_file(&shared_file) {
            return Err(overlay::unsafe_refused(format!(
                "shared store path {} exists but is not a regular file",
                shared_file.display()
            )));
        }
        let existing = fs::read(&shared_file)
            .with_context(|| format!("failed to read {}", shared_file.display()))?;
        if existing != contents {
            return Err(overlay::unsafe_refused(format!(
                "`{relative}` already has a different copy in the shared layer; reconcile {} and {} manually, then rerun `breeze share {relative}`",
                old_file.display(),
                shared_file.display()
            )));
        }
        fs::remove_file(&old_file)
            .with_context(|| format!("failed to remove {}", old_file.display()))?;
        repo.prune_empty_parents(&old_file);
    } else {
        if let Some(parent) = shared_file.parent() {
            crate::store::create_private_dir(parent)?;
        }
        fs::rename(&old_file, &shared_file).with_context(|| {
            format!(
                "failed to move {} to {}",
                old_file.display(),
                shared_file.display()
            )
        })?;
        repo.prune_empty_parents(&old_file);
    }

    Ok(overlay::relative_string(
        &repo.layout.repo_dir,
        &shared_file,
    ))
}

/// Point links at the shared copy only where it is the effective override.
fn repair_links(
    repo: &Repo,
    overrides: &crate::store::overrides::OverridesFile,
    relative: &str,
    shared_source: &str,
) -> Result<usize> {
    let shared_file = repo.layout.repo_dir.join(shared_source);
    let contents = fs::read(&shared_file)
        .with_context(|| format!("failed to read {}", shared_file.display()))?;
    let entries: Vec<&OverrideEntry> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.path == relative)
        .collect();

    let mut repaired = 0usize;
    for target in repo.worktree_targets(false) {
        let Some(best) = overlay::best_entry(entries.iter().copied(), &target) else {
            continue;
        };
        if best.source != shared_source {
            continue;
        }

        let link = target.path.join(relative);
        match fs::symlink_metadata(&link) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if overlay::link_points_to(&link, &shared_file) {
                    continue;
                }
                fs::remove_file(&link)
                    .with_context(|| format!("failed to remove {}", link.display()))?;
                overlay::create_relative_symlink(&link, &shared_file)?;
                repaired += 1;
            }
            Ok(metadata) if metadata.is_file() => {
                let current = fs::read(&link)
                    .with_context(|| format!("failed to read {}", link.display()))?;
                if current == contents {
                    fs::remove_file(&link)
                        .with_context(|| format!("failed to remove {}", link.display()))?;
                    overlay::create_relative_symlink(&link, &shared_file)?;
                    repaired += 1;
                } else {
                    eprintln!(
                        "warning: {} has unrelated content; left untouched (run `breeze doctor`)",
                        link.display()
                    );
                }
            }
            Ok(_) => eprintln!(
                "warning: {} is not a regular file; left untouched",
                link.display()
            ),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                overlay::ensure_link_parent(&link)?;
                overlay::create_relative_symlink(&link, &shared_file)?;
                repaired += 1;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to inspect {}", link.display()));
            }
        }
    }

    Ok(repaired)
}

/// Move the stored patch and metadata to `shared/patches/`.
fn share_patch(repo: &Repo, relative: &str, entry: &OverrideEntry) -> Result<String> {
    let old_patch = repo.layout.repo_dir.join(&entry.source);
    if !Repo::is_regular_store_file(&old_patch) {
        return Err(CommandError::new(
            exit::GENERAL,
            format!(
                "stored patch is missing for `{relative}` ({}); cannot share it",
                old_patch.display()
            ),
        )
        .into());
    }
    let old_meta = patches::metadata_path_for(&old_patch)?;
    if !Repo::is_regular_store_file(&old_meta) {
        return Err(overlay::unsafe_refused(format!(
            "patch metadata for `{relative}` is missing or is not a regular file ({})",
            old_meta.display()
        )));
    }
    let patch_bytes =
        fs::read(&old_patch).with_context(|| format!("failed to read {}", old_patch.display()))?;
    let meta_bytes = fs::read(&old_meta).with_context(|| {
        format!(
            "patch metadata is missing for `{relative}` ({}); run `breeze doctor`",
            old_meta.display()
        )
    })?;

    let patch_files = patches::patch_files(&repo.layout.shared_dir, relative);
    let shared_patch = patch_files.patch;
    let shared_meta = patch_files.metadata;
    let mut metadata: patches::PatchMetadata = serde_json::from_slice(&meta_bytes)
        .with_context(|| format!("failed to parse patch metadata for `{relative}`"))?;
    metadata.name = entry.name.clone();

    if fs::symlink_metadata(&shared_patch).is_ok() {
        if !Repo::is_regular_store_file(&shared_patch) {
            return Err(overlay::unsafe_refused(format!(
                "shared patch path {} exists but is not a regular file",
                shared_patch.display()
            )));
        }
        let existing = fs::read(&shared_patch)
            .with_context(|| format!("failed to read {}", shared_patch.display()))?;
        if existing != patch_bytes {
            return Err(overlay::unsafe_refused(format!(
                "`{relative}` already has a different patch in the shared layer; reconcile {} and {} manually, then rerun `breeze share {relative}`",
                old_patch.display(),
                shared_patch.display()
            )));
        }
    }

    repo.write_store_file(&shared_patch, &patch_bytes)?;
    patches::write_metadata(&shared_meta, &metadata)?;
    fs::remove_file(&old_patch)
        .with_context(|| format!("failed to remove {}", old_patch.display()))?;
    if overlay::exists(&old_meta) {
        fs::remove_file(&old_meta)
            .with_context(|| format!("failed to remove {}", old_meta.display()))?;
    }
    repo.prune_empty_parents(&old_patch);

    if let Err(err) = move_archive(repo, relative, entry) {
        eprintln!(
            "warning: could not move the archived copy of `{relative}` to the shared layer: {err:#}"
        );
    }

    Ok(overlay::relative_string(
        &repo.layout.repo_dir,
        &shared_patch,
    ))
}

/// Move a matching archive from the old layer to `shared/backups/patches/`.
fn move_archive(repo: &Repo, relative: &str, entry: &OverrideEntry) -> Result<()> {
    let Some(layer) = Layer::from_name(&entry.layer) else {
        return Ok(());
    };
    if layer == Layer::Shared {
        return Ok(());
    }
    let old_layer_dir = repo.layer_dir(layer)?;
    let Some(record) = patches::list_archived(&old_layer_dir)
        .into_iter()
        .find(|record| {
            record.path == relative
                && entry
                    .name
                    .as_ref()
                    .map(|name| &record.name == name)
                    .unwrap_or(true)
        })
    else {
        return Ok(());
    };

    let old_files = patches::archive_files(&old_layer_dir, &record.name);
    let new_files = patches::archive_files(&repo.layout.shared_dir, &record.name);
    if fs::symlink_metadata(&new_files.patch).is_ok() {
        let existing = fs::read(&new_files.patch)
            .with_context(|| format!("failed to read {}", new_files.patch.display()))?;
        let source = fs::read(&old_files.patch)
            .with_context(|| format!("failed to read {}", old_files.patch.display()))?;
        if existing != source {
            eprintln!(
                "warning: archived patch `{}` already exists in the shared layer with different content; left {} in place",
                record.name,
                old_files.patch.display()
            );
            return Ok(());
        }
        fs::remove_file(&old_files.patch).ok();
        fs::remove_file(&old_files.metadata).ok();
        repo.prune_empty_parents(&old_files.patch);
        return Ok(());
    }

    if let Some(parent) = new_files.patch.parent() {
        crate::store::create_private_dir(parent)?;
    }
    fs::rename(&old_files.patch, &new_files.patch).with_context(|| {
        format!(
            "failed to move {} to {}",
            old_files.patch.display(),
            new_files.patch.display()
        )
    })?;
    fs::rename(&old_files.metadata, &new_files.metadata).with_context(|| {
        format!(
            "failed to move {} to {}",
            old_files.metadata.display(),
            new_files.metadata.display()
        )
    })?;
    repo.prune_empty_parents(&old_files.patch);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::store::overlay::testutil::TempRepo;

    use crate::commands::edit::tests::{commit_file, write_editor};

    #[test]
    fn shares_a_branch_link_with_every_worktree() {
        let repo = TempRepo::new("share-link");
        repo.write("branch.txt", "branch value\n");
        repo.breeze_ok(&["link", "branch.txt", "--layer", "branch"]);

        // Before sharing, only the current worktree has the link.
        assert!(std::fs::symlink_metadata(repo.other.join("branch.txt")).is_err());

        let output = repo.breeze_ok(&["share", "branch.txt"]);
        assert!(output.contains("shared branch.txt"), "{output}");

        let store = repo.store_dir();
        assert!(store.join("shared/files/branch.txt").is_file());
        assert!(!store
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("files/branch.txt")
            .exists());

        for root in [&repo.main, &repo.other] {
            let path = root.join("branch.txt");
            assert!(std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(fs::read_to_string(&path).unwrap(), "branch value\n");
            assert_eq!(
                path.canonicalize().unwrap(),
                store
                    .join("shared/files/branch.txt")
                    .canonicalize()
                    .unwrap()
            );
        }

        let overrides = fs::read_to_string(store.join("state/overrides.json")).unwrap();
        assert!(overrides.contains("\"layer\": \"shared\""), "{overrides}");
        assert!(
            overrides.contains("\"source\": \"shared/files/branch.txt\""),
            "{overrides}"
        );
        assert!(!overrides.contains("\"layer\": \"branch\""), "{overrides}");

        let events = fs::read_to_string(store.join("state/events.log")).unwrap();
        assert!(events.contains("share"), "{events}");
    }

    #[test]
    fn sharing_does_not_clobber_a_more_specific_layer() {
        let repo = TempRepo::new("share-shadow");
        repo.write("same.txt", "main value\n");
        repo.breeze_ok(&["link", "same.txt", "--layer", "branch"]);

        fs::write(repo.other.join("same.txt"), "feature value\n").unwrap();
        repo.breeze_ok_in(&repo.other, &["link", "same.txt", "--layer", "worktree"]);

        repo.breeze_ok(&["share", "same.txt"]);

        let store = repo.store_dir();
        assert_eq!(
            fs::read_to_string(store.join("shared/files/same.txt")).unwrap(),
            "main value\n"
        );
        assert_eq!(
            fs::read_to_string(repo.main.join("same.txt")).unwrap(),
            "main value\n"
        );
        // The worktree layer is more specific, so its link keeps its own copy.
        assert_eq!(
            fs::read_to_string(repo.other.join("same.txt")).unwrap(),
            "feature value\n"
        );
        let overrides = fs::read_to_string(store.join("state/overrides.json")).unwrap();
        assert!(overrides.contains("\"layer\": \"worktree\""), "{overrides}");
        assert!(
            overrides.contains("worktrees/") && overrides.contains("files/same.txt"),
            "{overrides}"
        );
    }

    #[test]
    fn shares_a_patch_and_warns_about_global_scope() {
        let repo = TempRepo::new("share-patch");
        commit_file(&repo, "fog.json", "base\n");
        let editor = write_editor(&repo, "share-patch-edit", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--layer", "branch", "--editor", &editor]);

        let output = repo.breeze(&["share", "fog.json"]);
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("shared fog.json"), "{stdout}");
        assert!(stderr.contains("applies to every branch"), "{stderr}");

        let store = repo.store_dir();
        assert!(store.join("shared/patches/fog.json.patch").is_file());
        assert!(store.join("shared/patches/fog.json.json").is_file());
        assert!(!store
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("patches/fog.json.patch")
            .exists());

        let overrides = fs::read_to_string(store.join("state/overrides.json")).unwrap();
        assert!(overrides.contains("\"mode\": \"patch\""), "{overrides}");
        assert!(overrides.contains("\"layer\": \"shared\""), "{overrides}");
        assert!(
            overrides.contains("\"source\": \"shared/patches/fog.json.patch\""),
            "{overrides}"
        );
    }

    #[test]
    fn share_detects_already_shared_missing_and_conflicting_overrides() {
        let repo = TempRepo::new("share-conflict");

        let (code, _) = repo.breeze_err(&["share", "absent.txt"]);
        assert_eq!(code, 2);

        repo.write("same.txt", "same\n");
        repo.breeze_ok(&["link", "same.txt"]);
        let output = repo.breeze_ok(&["share", "same.txt"]);
        assert!(output.contains("already shared"), "{output}");

        repo.write("branch.txt", "branch copy\n");
        repo.breeze_ok(&["link", "branch.txt", "--layer", "branch"]);

        let shared = repo.store_dir().join("shared/files/branch.txt");
        fs::create_dir_all(shared.parent().unwrap()).unwrap();
        fs::write(&shared, "different copy\n").unwrap();

        let (code, stderr) = repo.breeze_err(&["share", "branch.txt"]);
        assert_eq!(code, 5);
        assert!(
            stderr.contains("different copy") || stderr.contains("different"),
            "{stderr}"
        );
        assert!(stderr.contains("manually"), "{stderr}");

        // The original branch copy is untouched after the refusal.
        assert!(repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("files/branch.txt")
            .is_file());
    }
}
