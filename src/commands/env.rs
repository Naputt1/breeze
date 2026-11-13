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
        write_env_local(&repo, &merged, ctx)?;
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

fn write_env_local(repo: &Repo, merged: &BTreeMap<String, String>, ctx: &Ctx) -> Result<()> {
    let path = repo.info.root.join(".env.local");

    // `EnvArgs` exposes no `--force`; refuse rather than silently replacing a
    // file the user may have edited. (See the phase report for this deviation.)
    if overlay::exists(&path) {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!(
                "{} already exists; remove it before writing the merged environment",
                path.display()
            ),
        )
        .into());
    }

    let contents = envfile::format_dotenv(merged);
    envfile::write_atomic(&path, &contents)?;
    exclude::add_path(&repo.exclude_path(), ".env.local")?;
    repo.log_event("env-write", ".env.local")?;

    if !ctx.quiet {
        eprintln!("wrote {} ({}) variables", path.display(), merged.len());
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
        assert_eq!(
            fs::read_to_string(&path).unwrap().contains("A=shared"),
            true
        );
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
