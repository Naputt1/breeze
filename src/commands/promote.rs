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

    let mut overrides = repo.overrides()?;
    let entries: Vec<_> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.path == relative && entry.mode == OverrideMode::Patch)
        .cloned()
        .collect();

    if entries.is_empty() {
        return Err(CommandError::new(
            exit::USAGE,
            format!("no patch overlay recorded for `{relative}`"),
        )
        .into());
    }

    crate::commands::edit::remove_patch_entries(&repo, &mut overrides, &relative)?;
    repo.save_overrides(&overrides)?;
    repo.log_event("promote", &relative)?;

    if !ctx.quiet {
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
}
