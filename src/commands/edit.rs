//! `breeze edit` — edit a tracked file and store the change as a patch.

use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::EditArgs;
use crate::commands::Ctx;
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

    let layer = match args.layer.as_deref() {
        Some(value) => Layer::parse(Some(value))?,
        None => Layer::Branch,
    };
    // Validate the layer early: the branch layer needs a checked-out branch.
    let layer_dir = repo.layer_dir(layer)?;

    let base_blob = patches::head_blob(&repo.info.root, &relative)?.ok_or_else(|| {
        CommandError::new(
            exit::GENERAL,
            format!("`{relative}` has no version at HEAD; commit it before calling `breeze edit`"),
        )
    })?;
    let base_sha = patches::short_head(&repo.info.root)?.unwrap_or_default();

    let editor = resolve_editor(args)?;
    run_editor(&repo.info.root, &editor, &worktree_path)?;

    let diff = patches::generate_diff(&repo.info.root, &relative)?;
    let mut overrides = repo.overrides()?;

    if diff.trim().is_empty() {
        return undo(&repo, &mut overrides, &relative, ctx);
    }

    let files = patches::patch_files(&layer_dir, &relative);
    warn_other_layers(&overrides, &relative, layer);

    let now = now_rfc3339();
    let applied_hash = patches::file_hash(&worktree_path).ok_or_else(|| {
        CommandError::new(
            exit::GENERAL,
            format!("the editor removed `{relative}`; restore it or use `breeze revert`"),
        )
    })?;
    let created_at = patches::read_metadata(&files.metadata)
        .map(|metadata| metadata.created_at)
        .unwrap_or_else(|_| now.clone());

    let base_display = patches::short(&base_blob).to_string();
    repo.write_store_file(&files.patch, diff.as_bytes())?;
    patches::write_metadata(
        &files.metadata,
        &PatchMetadata {
            path: relative.clone(),
            base_sha: base_sha.clone(),
            base_blob,
            applied_hash,
            created_at: created_at.clone(),
            updated_at: now.clone(),
        },
    )?;

    let source = overlay::relative_string(&repo.layout.repo_dir, &files.patch);
    overrides.upsert(OverrideEntry {
        path: relative.clone(),
        mode: OverrideMode::Patch,
        layer: layer.name().to_string(),
        source,
        tracked: true,
        base_sha: Some(base_sha),
        created_at,
        updated_at: now,
    });
    repo.save_overrides(&overrides)?;
    repo.log_event("edit", &relative)?;

    if !ctx.quiet {
        println!("edited {relative} ({} layer)", layer.name());
        println!("patch:  {}", files.patch.display());
        println!("base:   {base_display}");
        println!("file:   {} (modified, not staged)", worktree_path.display());
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

/// `--editor`, then `$EDITOR` (only with a terminal), then `vi`.
fn resolve_editor(args: &EditArgs) -> Result<Vec<String>> {
    if let Some(command) = &args.editor {
        let parts = split_command(command);
        if parts.is_empty() {
            return Err(CommandError::new(exit::USAGE, "`--editor` must not be empty").into());
        }
        return Ok(parts);
    }

    if !std::io::stdin().is_terminal() {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            "no terminal is attached; pass `--editor CMD` or set an editor that works without a TTY",
        )
        .into());
    }

    match std::env::var("EDITOR") {
        Ok(value) if !value.trim().is_empty() => {
            let parts = split_command(&value);
            if parts.is_empty() {
                return Err(CommandError::new(exit::USAGE, "$EDITOR is empty").into());
            }
            Ok(parts)
        }
        _ => Ok(vec!["vi".to_string()]),
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

/// The file now matches HEAD: drop every stored patch for the path.
fn undo(
    repo: &Repo,
    overrides: &mut crate::store::overrides::OverridesFile,
    relative: &str,
    ctx: &Ctx,
) -> Result<i32> {
    let removed = remove_patch_entries(repo, overrides, relative)?;
    repo.save_overrides(overrides)?;
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

/// Remove stored patch data and index entries for one path.
pub fn remove_patch_entries(
    repo: &Repo,
    overrides: &mut crate::store::overrides::OverridesFile,
    relative: &str,
) -> Result<usize> {
    let sources: Vec<String> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.path == relative && entry.mode == OverrideMode::Patch)
        .map(|entry| entry.source.clone())
        .collect();

    overrides
        .overrides
        .retain(|entry| !(entry.path == relative && entry.mode == OverrideMode::Patch));

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
}
