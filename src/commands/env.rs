//! `breeze env` — print or write the merged overlay environment.
//!
//! Output is masked by default. Values are only printed with `--reveal`,
//! which needs an interactive terminal and an explicit confirmation. `--show`
//! is accepted as a synonym for printing and never unmasks anything.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::cli::{EnvArgs, EnvFormat};
use crate::commands::Ctx;
use crate::envfile;
use crate::exit::{self, CommandError};
use crate::store::exclude;
use crate::store::overlay::{self, Repo};

pub fn run(ctx: &Ctx, args: &EnvArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let merged = envfile::load_merged(&repo.env_files())?;

    if args.reveal {
        confirm_reveal()?;
    }

    let values: BTreeMap<String, String> = if args.reveal {
        merged.clone()
    } else {
        merged
            .keys()
            .map(|key| (key.clone(), "***".to_string()))
            .collect()
    };

    let output = match args.format {
        EnvFormat::Dotenv => envfile::format_dotenv(&values),
        EnvFormat::Shell => envfile::format_shell(&values),
        EnvFormat::Json => envfile::format_json(&values)?,
    };

    if args.write {
        write_env_local(&repo, &merged, args.force, ctx)?;
    }

    if !ctx.quiet {
        print!("{output}");
    }

    Ok(exit::SUCCESS)
}

fn confirm_reveal() -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            "--reveal needs an interactive terminal; write the merged environment to a file instead, such as `breeze env --write`",
        )
        .into());
    }

    eprint!("type `reveal` to print secret values: ");
    std::io::stderr().flush().ok();

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if line.trim() != "reveal" {
        return Err(CommandError::new(exit::GENERAL, "reveal cancelled").into());
    }
    Ok(())
}

fn write_env_local(
    repo: &Repo,
    merged: &BTreeMap<String, String>,
    force: bool,
    ctx: &Ctx,
) -> Result<()> {
    let path = repo.info.root.join(".env.local");

    let backup = if overlay::exists(&path) {
        if !force {
            return Err(CommandError::new(
                exit::UNSAFE_REFUSED,
                format!(
                    "{} already exists; rerun with `--force` to back it up and overwrite it",
                    path.display()
                ),
            )
            .into());
        }
        Some(repo.backup_file(&path)?)
    } else {
        None
    };

    let contents = envfile::format_dotenv(merged);
    envfile::write_atomic(&path, &contents)?;
    exclude::add_path(&repo.exclude_path(), ".env.local")?;
    repo.log_event("env-write", ".env.local")?;

    if !ctx.quiet {
        eprintln!("wrote {} ({}) variables", path.display(), merged.len());
        if let Some(backup) = backup {
            eprintln!("backup: {}", backup.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::envfile;
    use crate::store::overlay::testutil::TempRepo;

    fn write_layers(repo: &TempRepo) {
        let store = repo.store_dir();
        fs::write(store.join("shared/env"), "A=shared\nB=shared\nC=shared\n").unwrap();

        let branch_dir = store.join("branches").join(repo.branch_id(&repo.main));
        fs::create_dir_all(&branch_dir).unwrap();
        fs::write(branch_dir.join("env"), "B=branch\nC=branch\n").unwrap();

        let worktree_dir = store.join("worktrees").join(repo.worktree_id(&repo.main));
        fs::create_dir_all(&worktree_dir).unwrap();
        fs::write(worktree_dir.join("env"), "C=worktree\n").unwrap();
    }

    #[test]
    fn env_masks_values_by_default_and_respects_format() {
        let repo = TempRepo::new("env-mask");
        write_layers(&repo);

        let output = repo.breeze_ok(&["env"]);
        assert!(output.contains("A=***"), "{output}");
        assert!(output.contains("B=***"), "{output}");
        assert!(output.contains("C=***"), "{output}");
        assert!(!output.contains("shared"), "{output}");
        assert!(!output.contains("branch"), "{output}");
        assert!(!output.contains("worktree"), "{output}");

        let shell = repo.breeze_ok(&["env", "--format", "shell"]);
        assert!(shell.contains("export A='***'"), "{shell}");

        let json = repo.breeze_ok(&["env", "--format", "json"]);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(parsed["A"], "***");
        assert_eq!(parsed["C"], "***");

        let show = repo.breeze_ok(&["env", "--show"]);
        assert!(show.contains("A=***"), "{show}");
    }

    #[test]
    fn env_write_force_backs_up_and_replaces_an_existing_file() {
        let repo = TempRepo::new("env-force");
        write_layers(&repo);
        repo.breeze_ok(&["env", "--write"]);

        let path = repo.main.join(".env.local");
        fs::write(&path, "LOCAL=keep\n").unwrap();

        let (code, stderr) = repo.breeze_err(&["env", "--write"]);
        assert_eq!(code, 5);
        assert!(stderr.contains("--force"), "{stderr}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "LOCAL=keep\n");

        repo.breeze_ok(&["env", "--write", "--force"]);
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("A=shared"), "{contents}");
        assert!(!contents.contains("LOCAL=keep"), "{contents}");

        let backups = repo.store_dir().join("backups");
        let mut backup = None;
        for entry in fs::read_dir(&backups).unwrap() {
            let dir = entry.unwrap().path();
            for file in fs::read_dir(&dir).unwrap() {
                let file = file.unwrap().path();
                if file.file_name().and_then(|name| name.to_str()) == Some(".env.local") {
                    backup = Some((dir.clone(), file));
                }
            }
        }
        let (dir, backup) = backup.expect("expected a backup of the previous .env.local");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "LOCAL=keep\n");

        if cfg!(unix) {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn env_merges_shared_then_branch_then_worktree() {
        let repo = TempRepo::new("env-merge");
        write_layers(&repo);

        repo.breeze_ok(&["env", "--write"]);
        let path = repo.main.join(".env.local");
        let parsed: std::collections::BTreeMap<String, String> =
            envfile::parse(&fs::read_to_string(&path).unwrap())
                .expect("parse")
                .into_iter()
                .map(|entry| (entry.key, entry.value))
                .collect();

        assert_eq!(parsed["A"], "shared");
        assert_eq!(parsed["B"], "branch");
        assert_eq!(parsed["C"], "worktree");

        let exclude = fs::read_to_string(repo.main.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains(".env.local"), "{exclude}");

        // A second write refuses rather than clobbering the file.
        let (code, _) = repo.breeze_err(&["env", "--write"]);
        assert_eq!(code, 5);
        assert!(fs::read_to_string(&path).unwrap().contains("A=shared"));
    }

    #[test]
    fn reveal_requires_a_terminal() {
        let repo = TempRepo::new("env-reveal");
        write_layers(&repo);
        let (code, stderr) = repo.breeze_err(&["env", "--reveal"]);
        assert_eq!(code, 5);
        assert!(stderr.contains("interactive terminal"), "{stderr}");
    }
}
