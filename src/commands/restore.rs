//! `breeze restore` — re-apply an archived patch and record it again.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::RestoreArgs;
use crate::commands::Ctx;
use crate::exit::{self, CommandError};
use crate::store::now_rfc3339;
use crate::store::overlay::{self, Layer, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode};
use crate::store::patches::{self, ApplyOutcome, ArchivedPatch};

struct Candidate {
    layer: Layer,
    record: ArchivedPatch,
    patch_file: PathBuf,
}

pub fn run(ctx: &Ctx, args: &RestoreArgs) -> Result<i32> {
    patches::validate_name(&args.name)?;
    let repo = Repo::open(ctx)?;
    let wanted = match &args.layer {
        Some(value) => Some(Layer::parse(Some(value))?),
        None => None,
    };

    let mut candidates = Vec::new();
    for (layer, dir) in repo.current_layer_dirs() {
        if let Some(wanted) = wanted {
            if layer != wanted {
                continue;
            }
        }
        let files = patches::archive_files(&dir, &args.name);
        let patch_exists = Repo::is_regular_store_file(&files.patch);
        let meta_exists = Repo::is_regular_store_file(&files.metadata);
        if !patch_exists && !meta_exists {
            continue;
        }
        if !patch_exists || !meta_exists {
            return Err(CommandError::new(
                exit::GENERAL,
                format!(
                    "archived patch `{}` is incomplete in the {} layer ({}); keep both the .patch and .json files",
                    args.name,
                    layer.name(),
                    files.patch.display()
                ),
            )
            .into());
        }
        let record = patches::read_archived(&files.metadata).with_context(|| {
            format!(
                "failed to read archive metadata for `{}` ({})",
                args.name,
                files.metadata.display()
            )
        })?;
        candidates.push(Candidate {
            layer,
            record,
            patch_file: files.patch,
        });
    }

    if candidates.is_empty() {
        let layer = wanted
            .map(|layer| format!(" in the {} layer", layer.name()))
            .unwrap_or_default();
        return Err(CommandError::new(
            exit::USAGE,
            format!(
                "no archived patch named `{}`{layer}; run `breeze patches` to list archived patches",
                args.name
            ),
        )
        .into());
    }
    if candidates.len() > 1 {
        let list: Vec<String> = candidates
            .iter()
            .map(|candidate| format!("{} ({})", candidate.record.name, candidate.layer.name()))
            .collect();
        return Err(CommandError::new(
            exit::USAGE,
            format!(
                "patch `{}` is archived in several layers: {}; rerun with `--layer <layer>`",
                args.name,
                list.join(", ")
            ),
        )
        .into());
    }

    let mut candidate = candidates.pop().expect("one candidate");
    candidate.record.name = args.name.clone();
    let relative = candidate.record.path.clone();
    let relative_path = Path::new(&relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CommandError::new(
            exit::GENERAL,
            format!(
                "archived patch `{}` has an unsafe path `{relative}`",
                args.name
            ),
        )
        .into());
    }

    let overrides = repo.overrides()?;
    if let Some(active) = overrides.overrides.iter().find(|entry| {
        entry.mode == OverrideMode::Patch && entry.name.as_deref() == Some(args.name.as_str())
    }) {
        return Err(CommandError::new(
            exit::USAGE,
            format!(
                "patch `{}` is already active on `{}`; revert it first",
                args.name, active.path
            ),
        )
        .into());
    }

    let archive_display = overlay::relative_string(&repo.layout.repo_dir, &candidate.patch_file);
    let Some(base_blob) = patches::head_blob(&repo.info.root, &relative)? else {
        return Err(CommandError::new(
            exit::PATCH_CONFLICT,
            format!(
                "cannot restore `{}`: `{relative}` is missing from HEAD; the archive is kept at {archive_display}",
                args.name
            ),
        )
        .into());
    };
    if !overlay::is_tracked(&repo.info.root, &relative)? {
        return Err(CommandError::new(
            exit::PATCH_CONFLICT,
            format!(
                "cannot restore `{}`: `{relative}` is no longer tracked by git; the archive is kept at {archive_display}",
                args.name
            ),
        )
        .into());
    }
    if patches::is_submodule(&repo.info.root, &relative)? {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!("`{relative}` is a submodule; `breeze restore` only handles regular files"),
        )
        .into());
    }

    let worktree_path = repo.info.root.join(&relative);
    if !overlay::exists(&worktree_path) {
        let contents = patches::blob_content(&repo.info.root, &base_blob)
            .with_context(|| format!("failed to read the HEAD version of `{relative}`"))?;
        if let Some(parent) = worktree_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&worktree_path, contents)
            .with_context(|| format!("failed to write {}", worktree_path.display()))?;
    }
    let contents = fs::read(&worktree_path)
        .with_context(|| format!("failed to read {}", worktree_path.display()))?;
    if patches::looks_binary(&contents) {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!("`{relative}` looks binary; `breeze restore` only handles text files"),
        )
        .into());
    }

    match patches::apply_patch(&repo.info.root, &candidate.patch_file, &relative) {
        Ok(ApplyOutcome::Clean) => {}
        Ok(ApplyOutcome::Conflicted) => {
            return Err(CommandError::new(
                exit::PATCH_CONFLICT,
                format!(
                    "restoring `{}` produced conflicts in {}; resolve, then run `breeze edit {relative} --name {}`",
                    args.name,
                    worktree_path.display(),
                    args.name
                ),
            )
            .into());
        }
        Err(err) => {
            return Err(CommandError::new(
                exit::PATCH_CONFLICT,
                format!(
                    "failed to apply archived patch `{}`: {err:#}; the archive is kept at {archive_display}",
                    args.name
                ),
            )
            .into());
        }
    }

    let layer_dir = repo.layer_dir(candidate.layer)?;
    let files = patches::patch_files(&layer_dir, &relative);
    let patch_bytes = fs::read(&candidate.patch_file)
        .with_context(|| format!("failed to read {}", candidate.patch_file.display()))?;
    repo.write_store_file(&files.patch, &patch_bytes)?;

    let now = now_rfc3339();
    let applied_hash = patches::file_hash(&worktree_path).ok_or_else(|| {
        CommandError::new(
            exit::GENERAL,
            format!("the restored patch removed `{relative}`; restore it and retry"),
        )
    })?;
    let created_at = patches::read_metadata(&files.metadata)
        .map(|metadata| metadata.created_at)
        .unwrap_or_else(|_| now.clone());
    let base_sha = patches::short_head(&repo.info.root)?.unwrap_or_default();
    patches::write_metadata(
        &files.metadata,
        &patches::PatchMetadata {
            path: relative.clone(),
            base_sha: base_sha.clone(),
            base_blob,
            applied_hash,
            name: Some(candidate.record.name.clone()),
            created_at,
            updated_at: now.clone(),
        },
    )?;

    let source = overlay::relative_string(&repo.layout.repo_dir, &files.patch);
    let mut overrides = repo.overrides()?;
    overrides.upsert(OverrideEntry {
        path: relative.clone(),
        mode: OverrideMode::Patch,
        layer: candidate.layer.name().to_string(),
        source,
        tracked: true,
        base_sha: Some(base_sha),
        name: Some(candidate.record.name.clone()),
        created_at: now.clone(),
        updated_at: now,
    });
    repo.save_overrides(&overrides)?;
    repo.log_event("restore", &relative)?;

    if !ctx.quiet {
        println!(
            "restored patch '{}' ({relative}) in the {} layer",
            candidate.record.name,
            candidate.layer.name()
        );
        println!("patch:  {}", files.patch.display());
        println!("file:   {} (modified, not staged)", worktree_path.display());
        println!("archive kept: {archive_display}");
    }

    Ok(exit::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::store::overlay::testutil::TempRepo;

    use crate::commands::edit::tests::{commit_file, write_editor};

    #[test]
    fn restore_applies_a_clean_archive_and_records_the_name_again() {
        let repo = TempRepo::new("restore-clean");
        commit_file(&repo, "fog.json", "base\n");

        let editor = write_editor(&repo, "restore-add", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);
        repo.breeze_ok(&["revert", "--all"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\n");

        let output = repo.breeze_ok(&["restore", "api-timeout"]);
        assert!(output.contains("restored patch 'api-timeout'"), "{output}");
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\nlocal\n");

        let layer_dir = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main));
        assert!(layer_dir
            .join("backups/patches/api-timeout.patch")
            .is_file());
        let metadata = fs::read_to_string(layer_dir.join("patches/fog.json.json")).unwrap();
        assert!(metadata.contains("\"name\": \"api-timeout\""), "{metadata}");
        assert!(metadata.contains("\"base_blob\""), "{metadata}");
        let overrides = fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(
            overrides.contains("\"name\": \"api-timeout\""),
            "{overrides}"
        );
        assert!(
            repo.breeze_ok(&["status"])
                .contains("api-timeout (fog.json)"),
            "status should show the restored patch"
        );
    }

    #[test]
    fn restore_writes_conflict_markers_and_exits_three() {
        let repo = TempRepo::new("restore-conflict");
        commit_file(&repo, "fog.json", "one\ntwo\nthree\nfour\n");

        let editor = write_editor(
            &repo,
            "restore-conflict-edit",
            "printf 'one\\ntwo-local\\nthree\\nfour\\n' > \"$1\"",
        );
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);
        repo.breeze_ok(&["revert", "--all"]);
        commit_file(&repo, "fog.json", "one\ntwo-committed\nthree\nfour\n");

        let (code, stderr) = repo.breeze_err(&["restore", "api-timeout"]);
        assert_eq!(code, 3, "{stderr}");
        assert!(
            stderr.contains("breeze edit fog.json --name api-timeout"),
            "{stderr}"
        );
        let contents = repo.read(&repo.main, "fog.json");
        assert!(contents.contains("<<<<<<<"), "{contents}");
        let overrides = fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(!overrides.contains("fog.json"), "{overrides}");
        assert!(repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("backups/patches/api-timeout.patch")
            .is_file());
    }

    #[test]
    fn restore_refuses_a_file_missing_from_head_and_keeps_the_archive() {
        let repo = TempRepo::new("restore-missing");
        commit_file(&repo, "fog.json", "base\n");

        let editor = write_editor(&repo, "restore-missing-edit", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);
        repo.breeze_ok(&["revert", "--all"]);
        repo.git(&repo.main, &["rm", "fog.json"]);
        repo.git(&repo.main, &["commit", "-m", "remove fog"]);

        let (code, stderr) = repo.breeze_err(&["restore", "api-timeout"]);
        assert_eq!(code, 3, "{stderr}");
        assert!(stderr.contains("missing from HEAD"), "{stderr}");
        let archive = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("backups/patches/api-timeout.patch");
        assert!(archive.is_file(), "archive must be kept");
        assert!(!repo.main.join("fog.json").exists());
    }

    #[test]
    fn restore_requires_layer_when_the_name_is_archived_twice() {
        let repo = TempRepo::new("restore-ambiguous");
        commit_file(&repo, "fog.json", "base\n");

        let editor = write_editor(
            &repo,
            "restore-ambiguous-edit",
            "printf 'local\\n' >> \"$1\"",
        );
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--layer",
            "shared",
            "--editor",
            &editor,
        ]);
        repo.breeze_ok(&["revert", "--all"]);

        let shared_archive = repo.store_dir().join("shared/backups/patches");
        let branch_archive = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("backups/patches");
        fs::create_dir_all(&branch_archive).unwrap();
        for extension in ["patch", "json"] {
            fs::copy(
                shared_archive.join(format!("api-timeout.{extension}")),
                branch_archive.join(format!("api-timeout.{extension}")),
            )
            .unwrap();
        }

        let (code, stderr) = repo.breeze_err(&["restore", "api-timeout"]);
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains("several layers"), "{stderr}");
        assert!(stderr.contains("shared"), "{stderr}");
        assert!(stderr.contains("branch"), "{stderr}");
        assert!(stderr.contains("--layer"), "{stderr}");

        repo.breeze_ok(&["restore", "api-timeout", "--layer", "shared"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\nlocal\n");
    }

    #[test]
    fn restore_refuses_a_name_that_is_already_active() {
        let repo = TempRepo::new("restore-active");
        commit_file(&repo, "fog.json", "base\n");

        let editor = write_editor(&repo, "restore-active-edit", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);
        repo.breeze_ok(&["revert", "--all"]);
        repo.breeze_ok(&["restore", "api-timeout"]);

        let (code, stderr) = repo.breeze_err(&["restore", "api-timeout"]);
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains("already active"), "{stderr}");
    }

    #[test]
    fn restore_works_from_a_second_worktree() {
        let repo = TempRepo::new("restore-worktree");
        commit_file(&repo, "fog.json", "base\n");
        repo.git(&repo.other, &["reset", "--hard", "main"]);

        let editor = write_editor(
            &repo,
            "restore-worktree-edit",
            "printf 'local\\n' >> \"$1\"",
        );
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--layer",
            "shared",
            "--editor",
            &editor,
        ]);
        repo.breeze_ok(&["revert", "--all"]);

        repo.breeze_ok_in(&repo.other, &["restore", "api-timeout"]);
        assert_eq!(repo.read(&repo.other, "fog.json"), "base\nlocal\n");
        assert!(
            repo.breeze_ok_in(&repo.other, &["status"])
                .contains("api-timeout (fog.json)"),
            "the shared override should be visible from the second worktree"
        );
    }

    #[test]
    fn end_to_end_edit_name_revert_rebase_restore() {
        let repo = TempRepo::new("restore-e2e");
        commit_file(&repo, "fog.json", "base\n");
        repo.git(&repo.main, &["checkout", "-b", "topic"]);

        let editor = write_editor(&repo, "restore-e2e-edit", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);
        repo.breeze_ok(&["revert", "--all"]);
        assert!(repo.status_clean(&repo.main));

        repo.git(&repo.main, &["checkout", "main"]);
        repo.git(
            &repo.main,
            &["commit", "--allow-empty", "-m", "advance main"],
        );
        repo.git(&repo.main, &["checkout", "topic"]);
        repo.git(&repo.main, &["rebase", "main"]);

        repo.breeze_ok(&["restore", "api-timeout"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\nlocal\n");
        assert!(
            repo.breeze_ok(&["status"])
                .contains("api-timeout (fog.json)"),
            "status should show the restored patch"
        );
    }

    #[test]
    fn share_moves_the_matching_archive_to_the_shared_layer() {
        let repo = TempRepo::new("restore-share");
        commit_file(&repo, "fog.json", "base\n");

        let editor = write_editor(&repo, "restore-share-edit", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);
        repo.breeze_ok(&["revert", "--all"]);
        repo.breeze_ok(&["restore", "api-timeout"]);

        let branch_archive = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("backups/patches/api-timeout.patch");
        assert!(branch_archive.is_file());

        repo.breeze_ok(&["share", "fog.json"]);
        let shared_archive = repo
            .store_dir()
            .join("shared/backups/patches/api-timeout.patch");
        assert!(shared_archive.is_file(), "{}", shared_archive.display());
        assert!(shared_archive.with_extension("json").is_file());
        assert!(!branch_archive.exists(), "the old archive should move");
        assert!(repo
            .store_dir()
            .join("shared/patches/fog.json.patch")
            .is_file());
    }
}
