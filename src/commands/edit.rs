//! `breeze edit` — edit a tracked file and store the change as a patch.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::EditArgs;
use crate::commands::{archive, Ctx};
use crate::exit::{self, CommandError};
use crate::store::now_rfc3339;
use crate::store::overlay::{self, Layer, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode};
use crate::store::patches::{self, PatchFiles, PatchMetadata};

pub fn run(ctx: &Ctx, args: &EditArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let relative = overlay::relative_lenient_path(&repo, &args.path)?;
    let worktree_path = repo.info.root.join(&relative);

    require_editable(&repo, &relative, &worktree_path)?;
    let filter_mode = crate::store::filters::enabled(&repo.info.root);

    let layer = match args.layer.as_deref() {
        Some(value) => {
            let layer = Layer::parse(Some(value))?;
            if filter_mode && layer != Layer::Shared {
                return Err(CommandError::new(
                    exit::UNSAFE_REFUSED,
                    "editing the branch or worktree layer is not supported in filter mode yet; \
                     use the shared layer or run `breeze filter disable` first",
                )
                .into());
            }
            layer
        }
        None if filter_mode => Layer::Shared,
        None => Layer::Branch,
    };
    // Validate the layer early: the branch layer needs a checked-out branch.
    let layer_dir = repo.layer_dir(layer)?;
    let files = patches::patch_files(&layer_dir, &relative);
    let source = overlay::relative_string(&repo.layout.repo_dir, &files.patch);

    let mut overrides = repo.overrides()?;
    if let Some(name) = &args.name {
        patches::validate_name(name)?;
        archive::ensure_name_available(&repo, &overrides, name, &relative, layer, &source)?;
    }

    let base_blob = patches::head_blob(&repo.info.root, &relative)?.ok_or_else(|| {
        CommandError::new(
            exit::GENERAL,
            format!("`{relative}` has no version at HEAD; commit it before calling `breeze edit`"),
        )
    })?;
    let base_sha = patches::short_head(&repo.info.root)?.unwrap_or_default();

    let editor = resolve_editor(args)?;
    run_editor(&repo.info.root, &editor, &worktree_path)?;

    let contents = fs::read(&worktree_path).map_err(|_| {
        CommandError::new(
            exit::GENERAL,
            format!("the editor removed `{relative}`; restore it or use `breeze revert`"),
        )
    })?;

    // Filter mode cannot ask Git for the diff: the clean filter strips the
    // patch. Build it from the base blob and the edited file instead.
    let (diff, base_contents) = if filter_mode {
        let base_contents = patches::blob_content(&repo.info.root, &base_blob)?;
        let diff = patches::generate_diff_from_contents(&relative, &base_contents, &contents)?;
        (diff, Some(base_contents))
    } else {
        (patches::generate_diff(&repo.info.root, &relative)?, None)
    };

    let unchanged = match &base_contents {
        Some(base) => contents == *base,
        None => diff.trim().is_empty(),
    };
    if unchanged {
        return undo(&repo, &mut overrides, &relative, &source, ctx);
    }

    warn_other_layers(&overrides, &relative, layer);

    let now = now_rfc3339();
    let applied_hash = patches::hash_bytes(&contents);
    let created_at = patches::read_metadata(&files.metadata)
        .map(|metadata| metadata.created_at)
        .unwrap_or_else(|_| now.clone());
    let previous = patches::read_metadata(&files.metadata).ok();
    let existing_name = overrides
        .overrides
        .iter()
        .find(|entry| {
            entry.path == relative
                && entry.layer == layer.name()
                && entry.source == source
                && entry.mode == OverrideMode::Patch
        })
        .and_then(|entry| entry.name.clone());
    let name = args
        .name
        .clone()
        .or(existing_name)
        .or_else(|| previous.and_then(|metadata| metadata.name));

    let base_display = patches::short(&base_blob).to_string();
    repo.write_store_file(&files.patch, diff.as_bytes())?;
    if filter_mode {
        repo.write_store_file(&files.content, &contents)?;
    }
    patches::write_metadata(
        &files.metadata,
        &PatchMetadata {
            path: relative.clone(),
            base_sha: base_sha.clone(),
            base_blob,
            applied_hash,
            name: name.clone(),
            created_at: created_at.clone(),
            updated_at: now.clone(),
        },
    )?;

    overrides.upsert(OverrideEntry {
        path: relative.clone(),
        mode: OverrideMode::Patch,
        layer: layer.name().to_string(),
        source,
        tracked: true,
        base_sha: Some(base_sha),
        name: name.clone(),
        created_at,
        updated_at: now,
    });
    repo.save_overrides(&overrides)?;

    if filter_mode {
        crate::store::filters::sync_attributes(&repo, &overrides)?;
        // Force one checkout so smudge materialises the merged file.
        crate::store::filters::checkout(&repo.info.root, &relative)?;
    }
    repo.log_event("edit", &relative)?;

    if !ctx.quiet {
        println!("edited {relative} ({} layer)", layer.name());
        if let Some(name) = &name {
            println!("name:   {name}");
        }
        println!("patch:  {}", files.patch.display());
        println!("base:   {base_display}");
        if filter_mode {
            println!(
                "file:   {} (filter mode, merged by git)",
                worktree_path.display()
            );
        } else {
            println!("file:   {} (modified, not staged)", worktree_path.display());
        }
    }

    Ok(exit::SUCCESS)
}

/// Reject paths `edit` cannot safely patch.
fn require_editable(repo: &Repo, relative: &str, worktree_path: &Path) -> Result<()> {
    if !overlay::is_tracked(&repo.info.root, relative)? {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!("`{relative}` is not tracked by git; use `breeze link` for untracked files"),
        )
        .into());
    }

    if patches::is_submodule(&repo.info.root, relative)? {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!("`{relative}` is a submodule; `breeze edit` only handles regular files"),
        )
        .into());
    }

    let contents = fs::read(worktree_path)
        .with_context(|| format!("failed to read {}", worktree_path.display()))?;
    if patches::looks_binary(&contents) {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!("`{relative}` looks binary; `breeze edit` only handles text files"),
        )
        .into());
    }

    Ok(())
}

/// `--editor`, then `$EDITOR`.
///
/// `$EDITOR` is spawned exactly as given, so non-interactive editors such as
/// `sed -i` work without a terminal. Only the absence of both is refused.
fn resolve_editor(args: &EditArgs) -> Result<Vec<String>> {
    if let Some(command) = &args.editor {
        let parts = split_command(command);
        if parts.is_empty() {
            return Err(CommandError::new(exit::USAGE, "`--editor` must not be empty").into());
        }
        return Ok(parts);
    }

    match std::env::var("EDITOR") {
        Ok(value) if !value.trim().is_empty() => {
            let parts = split_command(&value);
            if parts.is_empty() {
                return Err(CommandError::new(exit::USAGE, "$EDITOR is empty").into());
            }
            Ok(parts)
        }
        _ => Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            "no editor configured; pass `--editor CMD` or set $EDITOR",
        )
        .into()),
    }
}

fn split_command(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .map(ToString::to_string)
        .collect()
}

fn run_editor(root: &Path, command: &[String], path: &Path) -> Result<()> {
    let mut process = Command::new(&command[0]);
    process.current_dir(root).args(&command[1..]).arg(path);
    let status = process
        .status()
        .with_context(|| format!("failed to launch editor `{}`", command.join(" ")))?;
    if !status.success() {
        return Err(CommandError::new(
            exit::GENERAL,
            format!(
                "editor `{}` exited with {}; the patch was not updated",
                command.join(" "),
                status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "a signal".to_string())
            ),
        )
        .into());
    }
    Ok(())
}

/// The file now matches HEAD: drop the patch stored for the current slot.
fn undo(
    repo: &Repo,
    overrides: &mut crate::store::overrides::OverridesFile,
    relative: &str,
    source: &str,
    ctx: &Ctx,
) -> Result<i32> {
    let removed = remove_patch_source(repo, overrides, relative, source)?;
    repo.save_overrides(overrides)?;

    if crate::store::filters::enabled(&repo.info.root) {
        crate::store::filters::sync_attributes(repo, overrides)?;
        crate::store::filters::checkout(&repo.info.root, relative)?;
    }
    repo.log_event("edit-undo", relative)?;

    if !ctx.quiet {
        if removed == 0 {
            println!("{relative} matches HEAD; nothing to record");
        } else {
            println!("{relative} matches HEAD; removed {removed} patch override(s)");
        }
    }
    Ok(exit::SUCCESS)
}

/// Remove the patch record and store files for one path and one layer slot.
pub(crate) fn remove_patch_source(
    repo: &Repo,
    overrides: &mut crate::store::overrides::OverridesFile,
    relative: &str,
    source: &str,
) -> Result<usize> {
    let matches = |entry: &OverrideEntry| {
        entry.path == relative && entry.mode == OverrideMode::Patch && entry.source == source
    };
    let sources: Vec<String> = overrides
        .overrides
        .iter()
        .filter(|entry| matches(entry))
        .map(|entry| entry.source.clone())
        .collect();

    overrides.overrides.retain(|entry| !matches(entry));

    for source in &sources {
        let patch_file = repo.layout.repo_dir.join(source);
        patches::remove_patch_files(&patch_file)?;
        repo.prune_empty_parents(&patch_file);
    }

    Ok(sources.len())
}

fn warn_other_layers(
    overrides: &crate::store::overrides::OverridesFile,
    relative: &str,
    layer: Layer,
) {
    let others: Vec<&str> = overrides
        .overrides
        .iter()
        .filter(|entry| {
            entry.path == relative
                && entry.mode == OverrideMode::Patch
                && entry.layer != layer.name()
        })
        .map(|entry| entry.layer.as_str())
        .collect();

    if !others.is_empty() {
        eprintln!(
            "warning: `{relative}` already has a patch override in the {} layer; recording this edit in the {} layer too (run `breeze doctor` to review duplicates)",
            others.join(", "),
            layer.name()
        );
    }
}

/// Path of the stored patch for one layer, for tests and doctor.
#[allow(dead_code)]
pub fn patch_files_for(repo: &Repo, layer: Layer, relative: &str) -> Result<PatchFiles> {
    let dir = repo.layer_dir(layer)?;
    Ok(patches::patch_files(&dir, relative))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::store::overlay::testutil::TempRepo;

    pub(crate) fn write_editor(repo: &TempRepo, label: &str, body: &str) -> String {
        let path = repo.base.join(format!("{label}-editor.sh"));
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write editor");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod editor");
        path.to_str().expect("editor path").to_string()
    }

    pub fn commit_file(repo: &TempRepo, relative: &str, contents: &str) {
        repo.write(relative, contents);
        repo.git(&repo.main, &["add", relative]);
        repo.git(&repo.main, &["commit", "-m", &format!("add {relative}")]);
    }

    #[test]
    fn edit_records_a_patch_and_leaves_the_file_modified() {
        let repo = TempRepo::new("edit-basic");
        commit_file(&repo, "fog.json", "{\"ports\": {}}\n");

        let editor = write_editor(&repo, "append", "printf '\\n// local change\\n' >> \"$1\"");
        let output =
            repo.breeze_ok(&["edit", "fog.json", "--layer", "branch", "--editor", &editor]);
        assert!(output.contains("edited fog.json"), "{output}");

        let local = repo.read(&repo.main, "fog.json");
        assert!(local.contains("// local change"), "{local}");

        let patch = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("patches/fog.json.patch");
        assert!(patch.is_file(), "missing {}", patch.display());
        let patch_text = fs::read_to_string(&patch).unwrap();
        assert!(patch_text.contains("// local change"), "{patch_text}");

        let metadata = fs::read_to_string(patch.with_extension("json")).unwrap();
        assert!(metadata.contains("\"base_blob\""), "{metadata}");
        assert!(metadata.contains("\"base_sha\""), "{metadata}");
        assert!(
            metadata.contains("\"applied_hash\": \"sha256:"),
            "{metadata}"
        );

        let overrides = fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(overrides.contains("\"mode\": \"patch\""), "{overrides}");
        assert!(overrides.contains("\"tracked\": true"), "{overrides}");

        let status = repo.git(&repo.main, &["status", "--porcelain"]);
        assert!(status.contains("fog.json"), "{status}");
    }

    #[test]
    fn edit_refuses_untracked_binary_and_missing_editor() {
        let repo = TempRepo::new("edit-refuse");
        repo.write("untracked.txt", "x\n");
        let (code, stderr) = repo.breeze_err(&["edit", "untracked.txt"]);
        assert_eq!(code, 5);
        assert!(stderr.contains("breeze link"), "{stderr}");

        repo.write("blob.bin", "a\0b");
        repo.git(&repo.main, &["add", "blob.bin"]);
        repo.git(&repo.main, &["commit", "-m", "binary"]);
        let editor = write_editor(&repo, "noop", "exit 0");
        let (code, stderr) = repo.breeze_err(&["edit", "blob.bin", "--editor", &editor]);
        assert_eq!(code, 5);
        assert!(stderr.contains("binary"), "{stderr}");
    }

    #[test]
    fn edit_uses_editor_env_without_a_tty_and_refuses_without_an_editor() {
        let repo = TempRepo::new("edit-editor-env");
        commit_file(&repo, "fog.json", "base\n");

        let editor = write_editor(&repo, "env-editor", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok_in_env(&repo.main, &["edit", "fog.json"], &[("EDITOR", &editor)]);
        let patch = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("patches/fog.json.patch");
        assert!(patch.is_file(), "missing {}", patch.display());
        assert!(fs::read_to_string(&patch).unwrap().contains("local"));

        // No `--editor` and no `$EDITOR` is the only refused case.
        let (code, stderr) = repo.breeze_err(&["edit", "fog.json"]);
        assert_eq!(code, 5, "{stderr}");
        assert!(stderr.contains("$EDITOR"), "{stderr}");
    }

    #[test]
    fn edit_that_restores_head_removes_the_override() {
        let repo = TempRepo::new("edit-undo");
        commit_file(&repo, "fog.json", "base\n");

        let add = write_editor(&repo, "add", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--editor", &add]);
        assert!(repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("patches/fog.json.patch")
            .is_file());

        let undo = write_editor(&repo, "undo", "git checkout -- \"$1\"");
        let output = repo.breeze_ok(&["edit", "fog.json", "--editor", &undo]);
        assert!(output.contains("removed 1 patch override"), "{output}");

        let overrides = fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(!overrides.contains("fog.json"), "{overrides}");
    }

    #[test]
    fn edit_name_is_stored_in_both_files_and_displayed() {
        let repo = TempRepo::new("edit-name");
        commit_file(&repo, "fog.json", "base\n");

        let editor = write_editor(&repo, "name-add", "printf 'local\\n' >> \"$1\"");
        let output = repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);
        assert!(output.contains("name:   api-timeout"), "{output}");

        let patch_dir = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("patches");
        let overrides = fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(
            overrides.contains("\"name\": \"api-timeout\""),
            "{overrides}"
        );
        let metadata = fs::read_to_string(patch_dir.join("fog.json.json")).unwrap();
        assert!(metadata.contains("\"name\": \"api-timeout\""), "{metadata}");

        let status = repo.breeze_ok(&["status"]);
        assert!(status.contains("api-timeout (fog.json)"), "{status}");
        let status_json = repo.breeze_ok(&["status", "--json"]);
        assert!(
            status_json.contains("\"name\": \"api-timeout\""),
            "{status_json}"
        );
        let diff = repo.breeze_ok(&["diff"]);
        assert!(
            diff.contains("# patch: api-timeout (branch, fog.json)"),
            "{diff}"
        );

        // Updating the name in place is allowed.
        let rename = write_editor(&repo, "rename-add", "printf 'more\\n' >> \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-later",
            "--editor",
            &rename,
        ]);
        let metadata = fs::read_to_string(patch_dir.join("fog.json.json")).unwrap();
        assert!(metadata.contains("\"name\": \"api-later\""), "{metadata}");
    }

    #[test]
    fn edit_rejects_invalid_and_duplicate_patch_names() {
        let repo = TempRepo::new("edit-name-dup");
        commit_file(&repo, "fog.json", "base\n");
        commit_file(&repo, "other.json", "other\n");

        let editor = write_editor(&repo, "dup-add", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);

        let (code, stderr) = repo.breeze_err(&[
            "edit", "fog.json", "--name", "Bad Name", "--editor", &editor,
        ]);
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains("invalid patch name"), "{stderr}");

        let (code, stderr) = repo.breeze_err(&[
            "edit",
            "other.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains("api-timeout"), "{stderr}");
        assert!(stderr.contains("fog.json"), "{stderr}");
    }
}
