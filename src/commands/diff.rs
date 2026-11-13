//! `breeze diff` — show the stored patches and links for this worktree.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::cli::DiffArgs;
use crate::commands::Ctx;
use crate::exit;
use crate::store::overlay::{self, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode};

pub fn run(ctx: &Ctx, args: &DiffArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let overrides = repo.overrides()?;
    let current = repo.current_target();

    let filter = match &args.path {
        Some(path) => Some(overlay::relative_lenient_path(&repo, path)?),
        None => None,
    };

    let mut grouped: BTreeMap<String, Vec<&OverrideEntry>> = BTreeMap::new();
    for entry in &overrides.overrides {
        if overlay::entry_applies(entry, &current) {
            grouped.entry(entry.path.clone()).or_default().push(entry);
        }
    }

    let mut shown = 0usize;
    for (path, entries) in &grouped {
        if let Some(filter) = &filter {
            if filter != path {
                continue;
            }
        }
        let Some(entry) = overlay::best_entry(entries.iter().copied(), &current) else {
            continue;
        };
        shown += 1;
        match entry.mode {
            OverrideMode::Patch => show_patch(&repo, path, entry, args.stat)?,
            OverrideMode::Link => {
                let store = repo.layout.repo_dir.join(&entry.source);
                println!("# {path} (link, {} layer)", entry.layer);
                println!("linked → {} (live link)", store.display());
            }
        }
    }

    if shown == 0 {
        match &filter {
            Some(path) => println!("no overlay recorded for {path}"),
            None => println!("no overlays recorded"),
        }
    }

    Ok(exit::SUCCESS)
}

fn show_patch(repo: &Repo, path: &str, entry: &OverrideEntry, stat: bool) -> Result<()> {
    let patch_file = repo.layout.repo_dir.join(&entry.source);
    if !Repo::is_regular_store_file(&patch_file) {
        println!(
            "MISSING {path} patch file (or it is not a regular file): {}",
            patch_file.display()
        );
        return Ok(());
    }

    let text = fs::read_to_string(&patch_file)
        .with_context(|| format!("failed to read {}", patch_file.display()))?;

    if stat {
        let summary = patch_stat(repo, &text)?;
        println!("{path} (patch, {} layer)", entry.layer);
        print!("{summary}");
        if !summary.ends_with('\n') {
            println!();
        }
    } else {
        println!("# {path} (patch, {} layer)", entry.layer);
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

/// Run `git apply --stat` over the stored patch text.
fn patch_stat(repo: &Repo, text: &str) -> Result<String> {
    let mut child = Command::new("git")
        .current_dir(&repo.info.root)
        .args(["apply", "--stat", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run git apply --stat")?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(text.as_bytes())
            .context("failed to write patch to git apply")?;
    }

    let output = child
        .wait_with_output()
        .context("failed to read git apply --stat output")?;
    if !output.status.success() {
        bail!(
            "git apply --stat failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::store::overlay::testutil::TempRepo;

    #[test]
    fn diff_prints_the_stored_patch_and_a_stat() {
        let repo = TempRepo::new("diff-patch");
        repo.write("fog.json", "one\ntwo\n");
        repo.git(&repo.main, &["add", "fog.json"]);
        repo.git(&repo.main, &["commit", "-m", "add fog"]);

        let editor = repo.base.join("diff-editor.sh");
        fs::write(&editor, "#!/bin/sh\nprintf 'one\\ntwo-local\\n' > \"$1\"\n").unwrap();
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
        repo.breeze_ok(&["edit", "fog.json", "--editor", editor.to_str().unwrap()]);

        let output = repo.breeze_ok(&["diff"]);
        assert!(
            output.contains("fog.json (patch, branch layer)"),
            "{output}"
        );
        assert!(output.contains("+two-local"), "{output}");

        let stat = repo.breeze_ok(&["diff", "--stat"]);
        assert!(stat.contains("fog.json (patch, branch layer)"), "{stat}");
        assert!(stat.contains("1 file changed"), "{stat}");
    }

    #[test]
    fn diff_counts_a_live_link() {
        let repo = TempRepo::new("diff-link");
        repo.write("config/local.toml", "x\n");
        repo.breeze_ok(&["link", "config/local.toml"]);

        let output = repo.breeze_ok(&["diff", "config/local.toml"]);
        assert!(output.contains("link, shared layer"), "{output}");
        assert!(output.contains("live link"), "{output}");
    }
}
