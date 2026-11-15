//! `breeze revert` — restore the flushed HEAD content and drop a patch.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::cli::RevertArgs;
use crate::commands::{archive, Ctx};
use crate::exit::{self, CommandError};
use crate::store::overlay::{self, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode, OverridesFile};
use crate::store::patches::{self, PatchMetadata};

struct StoredPatch {
    source: String,
    metadata: PatchMetadata,
    base: Vec<u8>,
}

pub fn run(ctx: &Ctx, args: &RevertArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    if args.all {
        return revert_all(&repo, ctx, args.force);
    }
    let relative = if let Some(name) = &args.name {
        let overrides = repo.overrides()?;
        let entry = overrides
            .overrides
            .iter()
            .find(|entry| entry.mode == OverrideMode::Patch && entry.name.as_deref() == Some(name))
            .ok_or_else(|| {
                CommandError::new(exit::USAGE, format!("no patch overlay named `{name}`"))
            })?;
        entry.path.clone()
    } else {
        let path = args.path.as_ref().ok_or_else(|| {
            CommandError::new(
                exit::USAGE,
                "specify a path, --name or --all to revert patch overlays",
            )
        })?;
        overlay::relative_lenient_path(&repo, path)?
    };
    revert_relative(&repo, ctx, &relative, args.force)
}

fn revert_all(repo: &Repo, ctx: &Ctx, force: bool) -> Result<i32> {
    let overrides = repo.overrides()?;
    let mut paths: Vec<String> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.mode == OverrideMode::Patch)
        .map(|entry| entry.path.clone())
        .collect();
    paths.sort();
    paths.dedup();

    if paths.is_empty() {
        if !ctx.quiet {
            println!("no patch overlays to revert");
        }
        return Ok(exit::SUCCESS);
    }

    for path in paths {
        revert_relative(repo, ctx, &path, force)?;
    }
    Ok(exit::SUCCESS)
}

pub(crate) fn revert_relative(repo: &Repo, ctx: &Ctx, relative: &str, force: bool) -> Result<i32> {
    let mut overrides = repo.overrides()?;
    let patch_entries: Vec<_> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.path == relative && entry.mode == OverrideMode::Patch)
        .cloned()
        .collect();

    if patch_entries.is_empty() {
        return Err(CommandError::new(
            exit::USAGE,
            format!("no patch overlay recorded for `{relative}`"),
        )
        .into());
    }

    if crate::store::filters::enabled(&repo.info.root) {
        return revert_filter(repo, ctx, relative, force, &patch_entries, &mut overrides);
    }

    let mut stored = Vec::with_capacity(patch_entries.len());
    for entry in &patch_entries {
        let patch_file = repo.layout.repo_dir.join(&entry.source);
        let meta_file = patches::metadata_path_for(&patch_file)?;
        if !Repo::is_regular_store_file(&patch_file) || !Repo::is_regular_store_file(&meta_file) {
            return Err(overlay::unsafe_refused(format!(
                "patch files for `{relative}` are missing or are not regular files; run `breeze doctor`"
            )));
        }
        let metadata = patches::read_metadata(&meta_file).with_context(|| {
            format!(
                "failed to read patch metadata for `{relative}` ({})",
                meta_file.display()
            )
        })?;
        let base = patches::blob_content(&repo.info.root, &metadata.base_blob)
            .with_context(|| format!("failed to read the stored base for `{relative}`"))?;
        stored.push(StoredPatch {
            source: entry.source.clone(),
            metadata,
            base,
        });
    }

    let current = repo.current_target();
    let Some(current_entry) = overlay::best_entry(patch_entries.iter(), &current) else {
        return Err(CommandError::new(
            exit::USAGE,
            format!("no patch overlay for `{relative}` applies to this worktree"),
        )
        .into());
    };
    let current_stored = stored
        .iter()
        .find(|item| item.source == current_entry.source)
        .ok_or_else(|| {
            CommandError::new(
                exit::GENERAL,
                "patch metadata is inconsistent; run `breeze doctor`",
            )
        })?;

    let current_path = repo.info.root.join(relative);
    let current_hash = patches::file_hash(&current_path);
    let applied_hash = current_stored.metadata.applied_hash.as_str();
    let base_hash = patches::hash_bytes(&current_stored.base);

    let at_applied = current_hash.as_deref() == Some(applied_hash);
    let at_base = current_hash.as_deref() == Some(base_hash.as_str());
    let missing = current_hash.is_none();
    let diverged = !missing && !at_applied && !at_base;

    if diverged && !force {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!(
                "`{}` changed since the patch was applied; rerun with `--force` to back it up and restore the HEAD content",
                current_path.display()
            ),
        )
        .into());
    }

    let mut backups = Vec::new();
    if diverged && force {
        backups.push(repo.backup_file(&current_path)?);
    }

    let archived = archive::archive_entries(repo, &patch_entries)?;

    let mut restored = 0usize;
    if !at_base {
        write_base(&current_path, &current_stored.base)?;
        println!("restored {relative} in {}", repo.info.root.display());
        restored += 1;
    }

    let mut warnings = Vec::new();
    for target in repo.worktree_targets(false) {
        if target.path == repo.info.root {
            continue;
        }
        let Some(entry) = overlay::best_entry(patch_entries.iter(), &target) else {
            continue;
        };
        let Some(item) = stored.iter().find(|stored| stored.source == entry.source) else {
            continue;
        };

        let file = target.path.join(relative);
        let Some(hash) = patches::file_hash(&file) else {
            continue;
        };
        let target_base = patches::hash_bytes(&item.base);
        if hash == target_base {
            continue;
        }
        if hash == item.metadata.applied_hash {
            write_base(&file, &item.base)?;
            println!("restored {relative} in {}", target.path.display());
            restored += 1;
        } else {
            warnings.push(format!(
                "{} has unrelated edits; left untouched",
                file.display()
            ));
        }
    }

    for item in &stored {
        let patch_file = repo.layout.repo_dir.join(&item.source);
        patches::remove_patch_files(&patch_file)?;
        repo.prune_empty_parents(&patch_file);
    }
    overrides
        .overrides
        .retain(|entry| !(entry.path == relative && entry.mode == OverrideMode::Patch));
    repo.save_overrides(&overrides)?;
    repo.log_event("revert", relative)?;

    if !ctx.quiet {
        for record in &archived {
            println!(
                "archived patch '{}' → backups/patches/{}.patch",
                record.name, record.name
            );
        }
        println!(
            "reverted {relative}; removed {} patch override(s)",
            stored.len()
        );
        println!("restored: {restored} worktree file(s)");
        for backup in &backups {
            println!("backup:   {}", backup.display());
        }
        for warning in &warnings {
            eprintln!("WARN {warning}");
        }
    }

    Ok(exit::SUCCESS)
}

/// Filter-mode revert: drop the record and attributes line, then let Git
/// check the plain index content back out.
fn revert_filter(
    repo: &Repo,
    ctx: &Ctx,
    relative: &str,
    force: bool,
    patch_entries: &[OverrideEntry],
    overrides: &mut OverridesFile,
) -> Result<i32> {
    use crate::store::filters;

    for entry in patch_entries {
        let patch_file = repo.layout.repo_dir.join(&entry.source);
        let meta_file = patches::metadata_path_for(&patch_file)?;
        if !Repo::is_regular_store_file(&patch_file) || !Repo::is_regular_store_file(&meta_file) {
            return Err(overlay::unsafe_refused(format!(
                "patch files for `{relative}` are missing or are not regular files; run `breeze doctor`"
            )));
        }
    }

    let current = repo.current_target();
    let current_entry = overlay::best_entry(patch_entries.iter(), &current).ok_or_else(|| {
        CommandError::new(
            exit::USAGE,
            format!("no patch overlay for `{relative}` applies to this worktree"),
        )
    })?;

    let mut diverged_targets = std::collections::BTreeSet::new();
    for target in repo.worktree_targets(false) {
        if target.path == repo.info.root {
            continue;
        }
        let Some(entry) = overlay::best_entry(patch_entries.iter(), &target) else {
            continue;
        };
        if filters::manual_diverged(repo, &target.path, relative, entry)? {
            diverged_targets.insert(target.path.clone());
        }
    }

    let diverged = filters::manual_diverged(repo, &repo.info.root, relative, current_entry)?;
    if diverged && !force {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!(
                "`{}` changed since the patch was applied; rerun with `--force` to back it up and restore the HEAD content",
                repo.info.root.join(relative).display()
            ),
        )
        .into());
    }

    let mut backups = Vec::new();
    if diverged && force {
        backups.push(repo.backup_file(&repo.info.root.join(relative))?);
    }

    for entry in patch_entries {
        let patch_file = repo.layout.repo_dir.join(&entry.source);
        patches::remove_patch_files(&patch_file)?;
        repo.prune_empty_parents(&patch_file);
    }
    overrides
        .overrides
        .retain(|entry| !(entry.path == relative && entry.mode == OverrideMode::Patch));
    repo.save_overrides(overrides)?;
    filters::sync_attributes(repo, overrides)?;

    let mut restored = 0usize;
    let mut warnings = Vec::new();
    for target in repo.worktree_targets(false) {
        let file = target.path.join(relative);
        if target.path != repo.info.root && !file.exists() {
            continue;
        }
        if diverged_targets.contains(&target.path) {
            warnings.push(format!(
                "{} has unrelated edits; left untouched",
                file.display()
            ));
            continue;
        }
        filters::checkout(&target.path, relative)?;
        println!("restored {relative} in {}", target.path.display());
        restored += 1;
    }

    repo.log_event("revert", relative)?;

    if !ctx.quiet {
        println!(
            "reverted {relative}; removed {} patch override(s)",
            patch_entries.len()
        );
        println!("restored: {restored} worktree file(s)");
        for backup in &backups {
            println!("backup:   {}", backup.display());
        }
        for warning in &warnings {
            eprintln!("WARN {warning}");
        }
    }

    Ok(exit::SUCCESS)
}

fn write_base(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::store::overlay::testutil::TempRepo;

    use crate::commands::edit::tests::{commit_file, write_editor};

    #[test]
    fn revert_restores_head_and_cleans_every_applied_worktree() {
        let repo = TempRepo::new("revert-basic");
        commit_file(&repo, "fog.json", "base\n");
        // The feature worktree needs the tracked file too.
        repo.git(&repo.other, &["reset", "--hard", "main"]);

        let editor = write_editor(&repo, "revert-add", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--layer", "shared", "--editor", &editor]);
        repo.breeze_ok(&["sync", "--all"]);

        assert_eq!(repo.read(&repo.main, "fog.json"), "base\nlocal\n");
        assert_eq!(repo.read(&repo.other, "fog.json"), "base\nlocal\n");

        let output = repo.breeze_ok(&["revert", "fog.json"]);
        assert!(output.contains("restored"), "{output}");

        assert_eq!(repo.read(&repo.main, "fog.json"), "base\n");
        assert_eq!(repo.read(&repo.other, "fog.json"), "base\n");
        assert!(repo.status_clean(&repo.main));
        assert!(repo.status_clean(&repo.other));

        let overrides = fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(!overrides.contains("fog.json"), "{overrides}");
        assert!(!repo
            .store_dir()
            .join("shared/patches/fog.json.patch")
            .exists());
    }

    #[test]
    fn revert_refuses_diverged_files_unless_forced_and_backs_up() {
        let repo = TempRepo::new("revert-force");
        commit_file(&repo, "fog.json", "base\n");
        // Keep the secondary worktree out of this test's way.
        repo.git(&repo.other, &["reset", "--hard", "main"]);

        let editor = write_editor(&repo, "revert-force-add", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--layer", "branch", "--editor", &editor]);

        fs::write(repo.main.join("fog.json"), "someone else\n").unwrap();

        let (code, stderr) = repo.breeze_err(&["revert", "fog.json"]);
        assert_eq!(code, 5, "{stderr}");
        assert!(stderr.contains("--force"), "{stderr}");
        assert_eq!(repo.read(&repo.main, "fog.json"), "someone else\n");

        repo.breeze_ok(&["revert", "fog.json", "--force"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\n");

        let backups = repo.store_dir().join("backups");
        let mut found = false;
        for entry in fs::read_dir(&backups).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                for file in fs::read_dir(entry.path()).unwrap() {
                    if file.unwrap().file_name() == "fog.json" {
                        found = true;
                    }
                }
            }
        }
        assert!(found, "expected a backup of the diverged file");
    }

    #[test]
    fn revert_archives_named_and_unnamed_patches_before_removing_them() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("revert-archive");
        commit_file(&repo, "fog.json", "base\n");
        repo.git(&repo.other, &["reset", "--hard", "main"]);

        let editor = write_editor(&repo, "archive-add", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);

        let output = repo.breeze_ok(&["revert", "--name", "api-timeout"]);
        assert!(
            output.contains("archived patch 'api-timeout' → backups/patches/api-timeout.patch"),
            "{output}"
        );

        let layer_dir = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main));
        let archive = layer_dir.join("backups/patches/api-timeout.patch");
        let archive_meta = archive.with_extension("json");
        assert!(archive.is_file(), "missing {}", archive.display());
        assert!(archive_meta.is_file(), "missing {}", archive_meta.display());
        assert_eq!(
            fs::metadata(&archive).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(archive.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let sidecar = fs::read_to_string(&archive_meta).unwrap();
        for key in [
            "\"name\": \"api-timeout\"",
            "\"path\": \"fog.json\"",
            "\"layer\": \"branch\"",
            "\"base_sha\"",
            "\"base_blob\"",
            "\"archived_at\"",
        ] {
            assert!(sidecar.contains(key), "missing {key}: {sidecar}");
        }
        assert!(!layer_dir.join("patches/fog.json.patch").exists());
        let overrides = fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(!overrides.contains("fog.json"), "{overrides}");

        // An unnamed patch gets a path + content-hash fallback name.
        repo.breeze_ok(&["edit", "fog.json", "--editor", &editor]);
        repo.breeze_ok(&["revert", "fog.json"]);
        let mut fallback = None;
        for entry in fs::read_dir(layer_dir.join("backups/patches")).unwrap() {
            let path = entry.unwrap().path();
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            if stem.starts_with("fog-json-") {
                fallback = Some(stem);
            }
        }
        let fallback = fallback.expect("fallback archive name");
        assert_eq!(fallback.len(), "fog-json-".len() + 8, "{fallback}");
    }
}
