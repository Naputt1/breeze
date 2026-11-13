//! `breeze revert` — restore the flushed HEAD content and drop a patch.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::cli::RevertArgs;
use crate::commands::Ctx;
use crate::exit::{self, CommandError};
use crate::store::overlay::{self, Repo};
use crate::store::overrides::OverrideMode;
use crate::store::patches::{self, PatchMetadata};

struct StoredPatch {
    source: String,
    metadata: PatchMetadata,
    base: Vec<u8>,
}

pub fn run(ctx: &Ctx, args: &RevertArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let relative = overlay::relative_lenient_path(&repo, &args.path)?;

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

    let mut stored = Vec::with_capacity(patch_entries.len());
    for entry in &patch_entries {
        let patch_file = repo.layout.repo_dir.join(&entry.source);
        let meta_file = patches::metadata_path_for(&patch_file)?;
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

    let current_path = repo.info.root.join(&relative);
    let current_hash = patches::file_hash(&current_path);
    let applied_hash = current_stored.metadata.applied_hash.as_str();
    let base_hash = patches::hash_bytes(&current_stored.base);

    let at_applied = current_hash.as_deref() == Some(applied_hash);
    let at_base = current_hash.as_deref() == Some(base_hash.as_str());
    let missing = current_hash.is_none();
    let diverged = !missing && !at_applied && !at_base;

    if diverged && !args.force {
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
    if diverged && args.force {
        backups.push(repo.backup_file(&current_path)?);
    }

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

        let file = target.path.join(&relative);
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
    repo.log_event("revert", &relative)?;

    if !ctx.quiet {
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
    use std::os::unix::fs::PermissionsExt;

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
}
