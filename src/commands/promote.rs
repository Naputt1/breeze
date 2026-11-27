//! `breeze promote` — drop the patch and leave the edit as a normal Git change.

use anyhow::Result;

use crate::cli::PromoteArgs;
use crate::commands::Ctx;
use crate::exit::{self, CommandError};
use crate::store::overlay::{self, Repo};
use crate::store::overrides::OverrideMode;

pub fn run(ctx: &Ctx, args: &PromoteArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let relative = overlay::relative_lenient_path(&repo, &args.path)?;

    let target = repo.current_target();
    let mut overrides = repo.overrides()?;
    let entries: Vec<_> = overrides
        .overrides
        .iter()
        .filter(|entry| {
            entry.path == relative
                && entry.mode == OverrideMode::Patch
                && overlay::entry_applies(entry, &target)
        })
        .cloned()
        .collect();

    if entries.is_empty() {
        return Err(CommandError::new(
            exit::USAGE,
            format!("no patch overlay applies to `{relative}` in this branch or worktree"),
        )
        .into());
    }

    let filter_mode = crate::store::filters::enabled(&repo.info.root);
    let mut diverged_targets = std::collections::BTreeSet::new();
    if filter_mode {
        for target in repo.worktree_targets(false) {
            if target.path == repo.info.root {
                continue;
            }
            let Some(entry) = overlay::best_entry(entries.iter(), &target) else {
                continue;
            };
            if crate::store::filters::manual_diverged(&repo, &target.path, &relative, entry)? {
                diverged_targets.insert(target.path.clone());
            }
        }
    }
    let archived = crate::commands::archive::archive_entries(&repo, &entries)?;

    for entry in &entries {
        crate::commands::edit::remove_patch_source(
            &repo,
            &mut overrides,
            &relative,
            &entry.source,
        )?;
    }
    repo.save_overrides(&overrides)?;
    if filter_mode {
        // The patched file stays on disk in this worktree; only its
        // registration goes away. Other worktrees fall back to plain content.
        crate::store::filters::sync_attributes(&repo, &overrides)?;
        for target in repo.worktree_targets(false) {
            if target.path == repo.info.root {
                continue;
            }
            let file = target.path.join(&relative);
            if !file.exists() {
                continue;
            }
            if diverged_targets.contains(&target.path) {
                eprintln!(
                    "WARN {} has unrelated edits; left untouched",
                    file.display()
                );
                continue;
            }
            crate::store::filters::checkout(&target.path, &relative)?;
        }
    }
    repo.log_event("promote", &relative)?;

    if !ctx.quiet {
        for record in &archived {
            println!(
                "archived patch '{}' → backups/patches/{}.patch",
                record.name, record.name
            );
        }
        println!("promoted {relative}: the local modification stays in the working tree");
        println!("next:");
        println!("  git add {relative}");
        println!("  git commit -m \"...\"");
    }

    Ok(exit::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::store::overlay::testutil::TempRepo;

    #[test]
    fn promote_keeps_the_edit_and_removes_the_override() {
        let repo = TempRepo::new("promote-basic");
        repo.write("fog.json", "base\n");
        repo.git(&repo.main, &["add", "fog.json"]);
        repo.git(&repo.main, &["commit", "-m", "add fog"]);

        let editor = repo.base.join("promote-editor.sh");
        fs::write(&editor, "#!/bin/sh\nprintf 'local\\n' >> \"$1\"\n").unwrap();
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
        repo.breeze_ok(&["edit", "fog.json", "--editor", editor.to_str().unwrap()]);

        let patch = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("patches/fog.json.patch");
        assert!(patch.is_file());

        let output = repo.breeze_ok(&["promote", "fog.json"]);
        assert!(output.contains("git add fog.json"), "{output}");

        assert_eq!(repo.read(&repo.main, "fog.json"), "base\nlocal\n");
        assert!(!patch.exists());
        let overrides = fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(!overrides.contains("fog.json"), "{overrides}");

        let status = repo.git(&repo.main, &["status", "--porcelain"]);
        assert!(status.contains("fog.json"), "{status}");
    }

    #[test]
    fn promote_leaves_patches_recorded_for_other_branches() {
        let repo = TempRepo::new("promote-branch-scope");
        repo.write("fog.json", "base\n");
        repo.git(&repo.main, &["add", "fog.json"]);
        repo.git(&repo.main, &["commit", "-m", "add fog"]);
        let main_branch = repo.branch_id(&repo.main);

        let main_editor = repo.base.join("promote-scope-main.sh");
        fs::write(&main_editor, "#!/bin/sh\nprintf 'main-local\\n' > \"$1\"\n").unwrap();
        fs::set_permissions(&main_editor, fs::Permissions::from_mode(0o755)).unwrap();
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--layer",
            "branch",
            "--editor",
            main_editor.to_str().unwrap(),
        ]);

        repo.git(&repo.main, &["switch", "-c", "side"]);
        let side_branch = repo.branch_id(&repo.main);
        let side_editor = repo.base.join("promote-scope-side.sh");
        fs::write(&side_editor, "#!/bin/sh\nprintf 'side-local\\n' > \"$1\"\n").unwrap();
        fs::set_permissions(&side_editor, fs::Permissions::from_mode(0o755)).unwrap();
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--layer",
            "branch",
            "--editor",
            side_editor.to_str().unwrap(),
        ]);
        repo.git(&repo.main, &["checkout", "--", "fog.json"]);
        repo.git(&repo.main, &["switch", "main"]);

        repo.breeze_ok(&["promote", "fog.json"]);

        let store = repo.store_dir();
        assert!(!store
            .join("branches")
            .join(&main_branch)
            .join("patches/fog.json.patch")
            .exists());
        assert!(store
            .join("branches")
            .join(&side_branch)
            .join("patches/fog.json.patch")
            .is_file());
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\n");

        repo.git(&repo.main, &["switch", "side"]);
        repo.breeze_ok(&["sync"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "side-local\n");
    }

    #[test]
    fn promote_archives_the_patch_before_removing_it() {
        let repo = TempRepo::new("promote-archive");
        repo.write("fog.json", "base\n");
        repo.git(&repo.main, &["add", "fog.json"]);
        repo.git(&repo.main, &["commit", "-m", "add fog"]);

        let editor = repo.base.join("promote-archive-editor.sh");
        fs::write(&editor, "#!/bin/sh\nprintf 'local\\n' >> \"$1\"\n").unwrap();
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "promote-me",
            "--editor",
            editor.to_str().unwrap(),
        ]);

        let output = repo.breeze_ok(&["promote", "fog.json"]);
        assert!(
            output.contains("archived patch 'promote-me' → backups/patches/promote-me.patch"),
            "{output}"
        );
        let archive = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("backups/patches/promote-me.patch");
        assert!(archive.is_file(), "missing {}", archive.display());
        assert!(archive.with_extension("json").is_file());
    }

    #[test]
    fn promote_removes_shared_and_branch_patches_for_the_same_path() {
        let repo = TempRepo::new("promote-stacked-layers");
        repo.write("fog.json", "base\n");
        repo.git(&repo.main, &["add", "fog.json"]);
        repo.git(&repo.main, &["commit", "-m", "add fog"]);
        let branch = repo.branch_id(&repo.main);

        let shared_editor = repo.base.join("promote-stacked-shared.sh");
        fs::write(
            &shared_editor,
            "#!/bin/sh\nprintf 'base\\nshared\\n' > \"$1\"\n",
        )
        .unwrap();
        fs::set_permissions(&shared_editor, fs::Permissions::from_mode(0o755)).unwrap();
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--layer",
            "shared",
            "--editor",
            shared_editor.to_str().unwrap(),
        ]);

        let branch_editor = repo.base.join("promote-stacked-branch.sh");
        fs::write(
            &branch_editor,
            "#!/bin/sh\nprintf 'base\\nshared\\nbranch\\n' > \"$1\"\n",
        )
        .unwrap();
        fs::set_permissions(&branch_editor, fs::Permissions::from_mode(0o755)).unwrap();
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--layer",
            "branch",
            "--editor",
            branch_editor.to_str().unwrap(),
        ]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\nshared\nbranch\n");

        repo.breeze_ok(&["promote", "fog.json"]);

        assert_eq!(repo.read(&repo.main, "fog.json"), "base\nshared\nbranch\n");
        let store = repo.store_dir();
        assert!(!store.join("shared/patches/fog.json.patch").exists());
        assert!(!store
            .join("branches")
            .join(&branch)
            .join("patches/fog.json.patch")
            .exists());
        let overrides = fs::read_to_string(store.join("state/overrides.json")).unwrap();
        assert!(!overrides.contains("fog.json"), "{overrides}");

        repo.git(&repo.main, &["add", "fog.json"]);
        repo.git(&repo.main, &["commit", "-m", "promote both"]);
        repo.breeze_ok(&["sync"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\nshared\nbranch\n");
    }
}
