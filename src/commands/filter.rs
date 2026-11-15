//! `breeze filter` — Git clean/smudge mode plus its internal drivers.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::Result;

use crate::cli::{FilterArgs, FilterCommand};
use crate::commands::Ctx;
use crate::exit::{self, CommandError};
use crate::store::filters;
use crate::store::overlay::{self, Layer, Repo};
use crate::store::overrides::OverrideMode;

pub fn run(ctx: &Ctx, args: &FilterArgs) -> Result<i32> {
    match &args.command {
        FilterCommand::Enable => enable(ctx),
        FilterCommand::Disable => disable(ctx),
        FilterCommand::Status => status(ctx),
        FilterCommand::Smudge(path) => Ok(driver(ctx, &path.path, true)),
        FilterCommand::Clean(path) => Ok(driver(ctx, &path.path, false)),
    }
}

/// Internal smudge/clean entry point.
///
/// The driver contract is absolute: read stdin, write stdout, exit 0. Any
/// problem (not initialized, missing store data, bad path, merge conflict)
/// degrades to passing the input through unchanged.
fn driver(ctx: &Ctx, path: &Path, smudge: bool) -> i32 {
    let mut input = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut input);

    let output = apply_driver(ctx, path, &input, smudge).unwrap_or(input);
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&output);
    let _ = stdout.flush();
    exit::SUCCESS
}

fn apply_driver(ctx: &Ctx, path: &Path, input: &[u8], smudge: bool) -> Option<Vec<u8>> {
    let repo = Repo::open(ctx).ok()?;
    // Git passes `%f`, which is already repository-relative; accept absolute
    // paths too so the driver also works when invoked by hand.
    let relative = overlay::relative_lenient_path(&repo, path).ok()?;
    Some(if smudge {
        filters::smudge(&repo, &relative, input)
    } else {
        filters::clean(&repo, &relative, input)
    })
}

fn enable(ctx: &Ctx) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let overrides = repo.overrides()?;

    let unsupported: Vec<String> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.mode == OverrideMode::Patch && entry.layer != Layer::Shared.name())
        .map(|entry| format!("{} ({} layer)", entry.path, entry.layer))
        .collect();
    if !unsupported.is_empty() {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!(
                "filter mode only supports shared-layer patches for now; move or revert: {}",
                unsupported.join(", ")
            ),
        )
        .into());
    }

    // Patches recorded before filter mode need their full patched contents
    // captured once.
    let paths = filters::expected_paths(&overrides);
    for relative in &paths {
        filters::ensure_content(&repo, relative)?;
    }

    filters::enable_config(&repo)?;
    filters::sync_attributes(&repo, &overrides)?;

    let mut rechecked = 0usize;
    for target in repo.worktree_targets(false) {
        for relative in &paths {
            if !target.path.join(relative).is_file() {
                continue;
            }
            let Some(entry) = filters::shared_entry(&overrides, relative) else {
                continue;
            };
            if filters::manual_diverged(&repo, &target.path, relative, entry)? {
                if !ctx.quiet {
                    println!(
                        "warning: left {} in {} untouched (manual changes present)",
                        relative,
                        target.path.display()
                    );
                }
                continue;
            }
            filters::checkout(&target.path, relative)?;
            rechecked += 1;
        }
    }

    if !ctx.quiet {
        println!(
            "filter mode enabled ({} patch path{} registered, {rechecked} checkout{})",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" },
            if rechecked == 1 { "" } else { "s" }
        );
        for path in &paths {
            println!("  {path}");
        }
    }
    Ok(exit::SUCCESS)
}

fn disable(ctx: &Ctx) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let paths = filters::disable(&repo)?;

    for target in repo.worktree_targets(false) {
        for relative in &paths {
            if !target.path.join(relative).is_file() {
                continue;
            }
            filters::checkout(&target.path, relative)?;
        }
    }

    if !ctx.quiet {
        println!(
            "filter mode disabled ({} path{} unregistered)",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" }
        );
        if !paths.is_empty() {
            println!("patch records are kept; `breeze sync` applies them in patch mode");
        }
    }
    Ok(exit::SUCCESS)
}

fn status(ctx: &Ctx) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let enabled = filters::enabled(&repo.info.root);

    if ctx.json {
        let paths = filters::registered_paths(&repo)?;
        let deferred = if enabled {
            filters::deferred_paths(&repo)?
        } else {
            Vec::new()
        };
        let binary = filters::configured_binary(&repo.info.root);
        let value = serde_json::json!({
            "enabled": enabled,
            "required": filters::required(&repo.info.root),
            "binary": binary.as_ref().map(|path| path.display().to_string()),
            "binary_present": binary.as_ref().map(|path| path.is_file()).unwrap_or(false),
            "paths": paths,
            "deferred": deferred,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(exit::SUCCESS);
    }

    println!(
        "filter mode: {}",
        if enabled { "enabled" } else { "disabled" }
    );
    if !enabled {
        println!("run `breeze filter enable` to register patches with Git");
        return Ok(exit::SUCCESS);
    }

    for key in [
        "filter.breeze.smudge",
        "filter.breeze.clean",
        "filter.breeze.required",
    ] {
        if let Some(value) = filters::config_value(&repo.info.root, key) {
            println!("{key} = {value}");
        }
    }
    match filters::configured_binary(&repo.info.root) {
        Some(binary) => {
            println!(
                "binary: {} ({})",
                binary.display(),
                if binary.is_file() {
                    "present"
                } else {
                    "MISSING"
                }
            );
        }
        None => println!("binary: not configured"),
    }

    let paths = filters::registered_paths(&repo)?;
    println!("registered paths: {}", paths.len());
    for path in &paths {
        println!("  {path}");
    }

    let deferred = filters::deferred_paths(&repo)?;
    if deferred.is_empty() {
        println!("deferred patches: none");
    } else {
        println!("deferred patches: {}", deferred.len());
        for path in deferred {
            println!("  {path} (upstream content conflicts; run `breeze doctor`)");
        }
    }
    Ok(exit::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};

    use crate::commands::edit::tests::{commit_file, write_editor};
    use crate::store::overlay::testutil::{breeze_bin, TempRepo};

    const BASE: &str = "one\ntwo\nthree\n";
    const PATCHED: &str = "one\ntwo\nthree\nlocal\n";

    fn git(repo: &TempRepo, dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("XDG_DATA_HOME", &repo.data)
            .env("XDG_CACHE_HOME", repo.base.join("cache"))
            .stdin(Stdio::null())
            .output()
            .expect("run git")
    }

    fn git_ok(repo: &TempRepo, dir: &Path, args: &[&str]) -> String {
        let output = git(repo, dir, args);
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn git_err(repo: &TempRepo, dir: &Path, args: &[&str]) -> Output {
        let output = git(repo, dir, args);
        assert!(
            !output.status.success(),
            "git {:?} unexpectedly succeeded\nstdout: {}",
            args,
            String::from_utf8_lossy(&output.stdout)
        );
        output
    }

    fn status(repo: &TempRepo, dir: &Path) -> String {
        git_ok(repo, dir, &["status", "--porcelain"])
    }

    fn filter_stdin(repo: &TempRepo, command: &str, path: &str, input: &[u8]) -> (i32, Vec<u8>) {
        let mut child = Command::new(breeze_bin())
            .current_dir(&repo.main)
            .args(["filter", command, path])
            .env("XDG_DATA_HOME", &repo.data)
            .env("XDG_CACHE_HOME", repo.base.join("cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn breeze filter");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("write stdin");
        let output = child.wait_with_output().expect("wait for breeze filter");
        (output.status.code().unwrap_or(-1), output.stdout)
    }

    fn prepare(label: &str) -> (TempRepo, String) {
        let repo = TempRepo::new(label);
        commit_file(&repo, "fog.json", BASE);
        let editor = write_editor(&repo, label, "printf 'local\\n' >> \"$1\"");
        (repo, editor)
    }

    fn enable(repo: &TempRepo) {
        repo.breeze_ok(&["filter", "enable"]);
    }

    fn edit(repo: &TempRepo, editor: &str) {
        repo.breeze_ok(&["edit", "fog.json", "--editor", editor]);
    }

    fn enable_and_edit(repo: &TempRepo, editor: &str) {
        enable(repo);
        edit(repo, editor);
    }

    fn add_remote(repo: &TempRepo) -> PathBuf {
        let bare = repo.base.join("remote.git");
        let bare_str = bare.to_str().expect("utf-8 bare path");
        git_ok(repo, &repo.base, &["init", "--bare", bare_str]);
        git_ok(
            repo,
            &repo.base,
            &[
                "--git-dir",
                bare_str,
                "symbolic-ref",
                "HEAD",
                "refs/heads/main",
            ],
        );
        git_ok(repo, &repo.main, &["remote", "add", "origin", bare_str]);
        git_ok(repo, &repo.main, &["push", "-u", "origin", "main"]);

        let upstream = repo.base.join("upstream");
        let upstream_str = upstream.to_str().expect("utf-8 upstream path");
        git_ok(repo, &repo.base, &["clone", bare_str, upstream_str]);
        git_ok(
            repo,
            &upstream,
            &["config", "user.email", "test@example.com"],
        );
        git_ok(repo, &upstream, &["config", "user.name", "Breeze Test"]);

        // The remote changes the repository identity; initialize the new store
        // Breeze will use for the rest of this test.
        repo.breeze_ok(&["init"]);
        upstream
    }

    fn upstream_commit(repo: &TempRepo, upstream: &Path, relative: &str, contents: &str) {
        fs::write(upstream.join(relative), contents).expect("write upstream file");
        git_ok(repo, upstream, &["add", relative]);
        git_ok(repo, upstream, &["commit", "-m", "upstream change"]);
        git_ok(repo, upstream, &["push", "origin", "main"]);
    }

    fn pull(repo: &TempRepo) {
        git_ok(repo, &repo.main, &["pull", "--no-edit", "origin", "main"]);
    }

    fn assert_clean(repo: &TempRepo, dir: &Path) {
        let output = status(repo, dir);
        assert!(output.trim().is_empty(), "worktree is dirty:\n{output}");
    }

    fn output_text(output: &Output) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    #[test]
    fn edit_writes_patched_content_and_keeps_the_tree_clean() {
        let (repo, editor) = prepare("filter-edit");
        enable_and_edit(&repo, &editor);

        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);
        assert_eq!(git_ok(&repo, &repo.main, &["diff"]), "");
        assert_eq!(git_ok(&repo, &repo.main, &["show", ":fog.json"]), BASE);
        assert_eq!(git_ok(&repo, &repo.main, &["show", "HEAD:fog.json"]), BASE);

        let attributes =
            fs::read_to_string(repo.main.join(".git/info/attributes")).expect("attributes");
        assert!(
            attributes.contains("fog.json filter=breeze"),
            "{attributes}"
        );
        let smudge = git_ok(
            &repo,
            &repo.main,
            &["config", "--local", "--get", "filter.breeze.smudge"],
        );
        assert!(smudge.contains("filter smudge %f"), "{smudge}");
        assert_eq!(
            git_ok(
                &repo,
                &repo.main,
                &["config", "--local", "--get", "filter.breeze.required"]
            )
            .trim(),
            "true"
        );

        let store = repo.store_dir();
        assert!(store.join("shared/patches/fog.json.content").is_file());
        assert!(store.join("shared/patches/fog.json.patch").is_file());

        // Pairing: clean strips exactly the stored patch, smudge restores it.
        let (code, cleaned) = filter_stdin(&repo, "clean", "fog.json", PATCHED.as_bytes());
        assert_eq!(code, 0);
        assert_eq!(cleaned, BASE.as_bytes());
        let (code, smudged) = filter_stdin(&repo, "smudge", "fog.json", BASE.as_bytes());
        assert_eq!(code, 0);
        assert_eq!(smudged, PATCHED.as_bytes());

        // Unknown paths pass through untouched.
        let (code, passthrough) = filter_stdin(&repo, "smudge", "other.txt", b"raw\n");
        assert_eq!(code, 0);
        assert_eq!(passthrough, b"raw\n");

        let human = repo.breeze_ok(&["status"]);
        assert!(human.contains("Patches (shared): fog.json"), "{human}");
        assert!(
            human.contains("overlays:  0 link(s), 1 patch(es)"),
            "{human}"
        );
        assert!(!human.contains("[deferred]"), "{human}");

        // sync must not write the patch into the worktree in filter mode.
        let sync = repo.breeze(&["sync", "--all"]);
        assert_eq!(sync.status.code(), Some(0), "{}", output_text(&sync));
        assert!(!output_text(&sync).contains("applied patch"));
        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);

        let filter_status = repo.breeze_ok(&["filter", "status"]);
        assert!(filter_status.contains("enabled"), "{filter_status}");
        assert!(filter_status.contains("fog.json"), "{filter_status}");
    }

    #[test]
    fn pull_with_upstream_change_elsewhere_keeps_the_patch() {
        let (repo, editor) = prepare("filter-pull-elsewhere");
        commit_file(&repo, "other.txt", "other\n");
        let upstream = add_remote(&repo);
        enable_and_edit(&repo, &editor);

        upstream_commit(&repo, &upstream, "other.txt", "upstream\n");
        pull(&repo);

        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);
        let doctor = repo.breeze(&["doctor"]);
        assert_eq!(doctor.status.code(), Some(0), "{}", output_text(&doctor));
    }

    #[test]
    fn upstream_change_in_the_same_hunk_defers_the_patch() {
        let (repo, editor) = prepare("filter-pull-conflict");
        let upstream = add_remote(&repo);
        enable_and_edit(&repo, &editor);

        upstream_commit(&repo, &upstream, "fog.json", "one\ntwo\nTHREE\n");
        pull(&repo);

        let expected = "one\ntwo\nTHREE\n";
        assert_eq!(repo.read(&repo.main, "fog.json"), expected);
        assert_clean(&repo, &repo.main);

        // Consistency invariant: deferred smudge output and clean pass-through.
        let (code, smudged) = filter_stdin(&repo, "smudge", "fog.json", expected.as_bytes());
        assert_eq!(code, 0);
        assert_eq!(smudged, expected.as_bytes());
        let (code, cleaned) = filter_stdin(&repo, "clean", "fog.json", expected.as_bytes());
        assert_eq!(code, 0);
        assert_eq!(cleaned, expected.as_bytes());

        let human = repo.breeze_ok(&["status"]);
        assert!(human.contains("fog.json [deferred]"), "{human}");
        let json = repo.breeze_ok(&["status", "--json"]);
        let value: serde_json::Value = serde_json::from_str(&json).expect("status json");
        let deferred = value["deferred"].as_array().expect("deferred array");
        assert!(deferred.iter().any(|item| item == "fog.json"), "{json}");

        let doctor = repo.breeze(&["doctor"]);
        assert_eq!(doctor.status.code(), Some(1), "{}", output_text(&doctor));
        assert!(
            output_text(&doctor).contains("DEFERRED fog.json"),
            "{}",
            output_text(&doctor)
        );

        let sync = repo.breeze(&["sync", "--all"]);
        assert_eq!(sync.status.code(), Some(3), "{}", output_text(&sync));
        assert!(
            output_text(&sync).contains("DEFERRED fog.json"),
            "{}",
            output_text(&sync)
        );
        assert_eq!(repo.read(&repo.main, "fog.json"), expected);
        assert_clean(&repo, &repo.main);
    }

    #[test]
    fn nearby_upstream_change_remerges_the_patch() {
        let (repo, editor) = prepare("filter-pull-nearby");
        let upstream = add_remote(&repo);
        enable_and_edit(&repo, &editor);

        upstream_commit(&repo, &upstream, "fog.json", "PRE\none\ntwo\nthree\n");
        pull(&repo);

        assert_eq!(
            repo.read(&repo.main, "fog.json"),
            "PRE\none\ntwo\nthree\nlocal\n"
        );
        assert_clean(&repo, &repo.main);
        let doctor = repo.breeze(&["doctor"]);
        assert_eq!(doctor.status.code(), Some(0), "{}", output_text(&doctor));
    }

    #[test]
    fn successful_rebase_keeps_the_patch_applied() {
        let (repo, editor) = prepare("filter-rebase-ok");
        git_ok(&repo, &repo.main, &["checkout", "-q", "-b", "upstream"]);
        repo.write("feature.txt", "feature\n");
        git_ok(&repo, &repo.main, &["add", "feature.txt"]);
        git_ok(&repo, &repo.main, &["commit", "-m", "feature"]);
        git_ok(&repo, &repo.main, &["checkout", "-q", "main"]);
        repo.write("main.txt", "main\n");
        git_ok(&repo, &repo.main, &["add", "main.txt"]);
        git_ok(&repo, &repo.main, &["commit", "-m", "main"]);

        enable_and_edit(&repo, &editor);
        git_ok(&repo, &repo.main, &["rebase", "upstream"]);

        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);
        let human = repo.breeze_ok(&["status"]);
        assert!(human.contains("Patches (shared): fog.json"), "{human}");
        assert!(!human.contains("[deferred]"), "{human}");
    }

    #[test]
    fn conflicted_rebase_abort_leaves_the_patch_recorded_and_clean() {
        let (repo, editor) = prepare("filter-rebase-conflict");
        git_ok(&repo, &repo.main, &["checkout", "-q", "-b", "upstream"]);
        repo.write("fog.json", "one\nFEATURE\nthree\n");
        git_ok(&repo, &repo.main, &["commit", "-am", "feature"]);
        git_ok(&repo, &repo.main, &["checkout", "-q", "main"]);
        repo.write("fog.json", "one\nMAIN\nthree\n");
        git_ok(&repo, &repo.main, &["commit", "-am", "main"]);

        enable_and_edit(&repo, &editor);
        let rebase = git_err(&repo, &repo.main, &["rebase", "upstream"]);
        assert!(
            output_text(&rebase).contains("could not apply"),
            "{}",
            output_text(&rebase)
        );
        git_ok(&repo, &repo.main, &["rebase", "--abort"]);

        assert_eq!(
            repo.read(&repo.main, "fog.json"),
            "one\nMAIN\nthree\nlocal\n"
        );
        assert_clean(&repo, &repo.main);
        assert!(!repo.main.join(".git/rebase-merge").exists());
        assert!(!repo.main.join(".git/rebase-apply").exists());
        let human = repo.breeze_ok(&["status"]);
        assert!(human.contains("Patches (shared): fog.json"), "{human}");
        let doctor = repo.breeze(&["doctor"]);
        assert_eq!(doctor.status.code(), Some(0), "{}", output_text(&doctor));
    }

    #[test]
    fn reset_checkout_and_restore_reapply_the_patch() {
        let (repo, editor) = prepare("filter-reset");
        enable_and_edit(&repo, &editor);

        git_ok(&repo, &repo.main, &["reset", "--hard"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);

        fs::write(repo.main.join("fog.json"), "junk\n").expect("write junk");
        git_ok(&repo, &repo.main, &["checkout", "--", "fog.json"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);

        fs::write(repo.main.join("fog.json"), "junk\n").expect("write junk");
        git_ok(&repo, &repo.main, &["restore", "fog.json"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);
    }

    #[test]
    fn stash_round_trip_keeps_the_patch() {
        let (repo, editor) = prepare("filter-stash");
        enable_and_edit(&repo, &editor);

        // Nothing to stash: clean() already hides the patch from Git.
        git_ok(&repo, &repo.main, &["stash", "push"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);

        // A genuine manual tweak is stashed and restored on top of the patch.
        let tweaked = repo.read(&repo.main, "fog.json").replacen("one", "ONE", 1);
        fs::write(repo.main.join("fog.json"), &tweaked).expect("write tweak");
        git_ok(&repo, &repo.main, &["stash", "push"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);

        git_ok(&repo, &repo.main, &["stash", "pop"]);
        let restored = repo.read(&repo.main, "fog.json");
        assert!(restored.contains("ONE"), "{restored}");
        assert!(restored.contains("local"), "{restored}");
        assert!(!status(&repo, &repo.main).trim().is_empty());
    }

    #[test]
    fn committing_through_the_filter_leaks_only_unpatched_content() {
        let (repo, editor) = prepare("filter-leak");
        enable_and_edit(&repo, &editor);
        repo.breeze_ok(&["guard", "install"]);

        // Git sees the patched file as unchanged; the commit carries only the
        // ordinary change, and the guard hook must not block it.
        repo.write("notes.txt", "notes\n");
        git_ok(&repo, &repo.main, &["add", "-A"]);
        git_ok(&repo, &repo.main, &["commit", "-m", "leak check"]);

        assert_eq!(git_ok(&repo, &repo.main, &["show", "HEAD:fog.json"]), BASE);
        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);
        let human = repo.breeze_ok(&["status"]);
        assert!(human.contains("Patches (shared): fog.json"), "{human}");
    }

    #[test]
    fn manual_tweak_shows_as_the_only_diff() {
        let (repo, editor) = prepare("filter-tweak");
        enable_and_edit(&repo, &editor);

        let tweaked = repo.read(&repo.main, "fog.json").replacen("one", "ONE", 1);
        fs::write(repo.main.join("fog.json"), tweaked).expect("write tweak");

        let diff = git_ok(&repo, &repo.main, &["diff", "--", "fog.json"]);
        assert!(diff.contains("-one"), "{diff}");
        assert!(diff.contains("+ONE"), "{diff}");
        assert!(!diff.contains("+local"), "{diff}");
        assert!(status(&repo, &repo.main).contains(" M fog.json"));
    }

    #[test]
    fn promote_drops_the_record_and_commits_patched_content() {
        let (repo, editor) = prepare("filter-promote");
        enable_and_edit(&repo, &editor);

        let output = repo.breeze_ok(&["promote", "fog.json"]);
        assert!(output.contains("git add fog.json"), "{output}");
        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);

        let attributes =
            fs::read_to_string(repo.main.join(".git/info/attributes")).expect("attributes");
        assert!(!attributes.contains("fog.json"), "{attributes}");
        assert!(git_ok(
            &repo,
            &repo.main,
            &["config", "--local", "--get", "filter.breeze.smudge"]
        )
        .contains("filter smudge %f"));

        git_ok(&repo, &repo.main, &["add", "fog.json"]);
        git_ok(&repo, &repo.main, &["commit", "-m", "promote"]);
        assert_eq!(
            git_ok(&repo, &repo.main, &["show", "HEAD:fog.json"]),
            PATCHED
        );

        let store = repo.store_dir();
        assert!(!store.join("shared/patches/fog.json.patch").exists());
        assert!(!store.join("shared/patches/fog.json.content").exists());
        let human = repo.breeze_ok(&["status"]);
        assert!(human.contains("0 patch(es)"), "{human}");
        assert_clean(&repo, &repo.main);
    }

    #[test]
    fn disable_and_uninstall_remove_filter_plumbing_and_keep_records() {
        let (repo, editor) = prepare("filter-disable");
        enable_and_edit(&repo, &editor);

        repo.breeze_ok(&["filter", "disable"]);
        assert!(!git(
            &repo,
            &repo.main,
            &["config", "--local", "--get", "filter.breeze.smudge"]
        )
        .status
        .success());
        let attributes =
            fs::read_to_string(repo.main.join(".git/info/attributes")).expect("attributes");
        assert!(!attributes.contains("fog.json"), "{attributes}");
        assert_eq!(repo.read(&repo.main, "fog.json"), BASE);
        assert_clean(&repo, &repo.main);
        assert!(repo
            .breeze_ok(&["status"])
            .contains("Patches (shared): fog.json"));

        // Re-enabling converts the existing record and captures content again.
        let content = repo.store_dir().join("shared/patches/fog.json.content");
        fs::remove_file(&content).expect("remove captured content");
        enable(&repo);
        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert!(content.is_file());
        assert_clean(&repo, &repo.main);

        repo.breeze_ok(&["guard", "install"]);
        repo.breeze_ok(&["guard", "uninstall"]);
        assert!(!git(
            &repo,
            &repo.main,
            &["config", "--local", "--get", "filter.breeze.smudge"]
        )
        .status
        .success());
        assert_eq!(repo.read(&repo.main, "fog.json"), BASE);
        assert_clean(&repo, &repo.main);
    }

    #[test]
    fn missing_filter_binary_fails_git_loudly_and_doctor_repairs_it() {
        let (repo, editor) = prepare("filter-missing-binary");
        enable_and_edit(&repo, &editor);

        git_ok(
            &repo,
            &repo.main,
            &[
                "config",
                "--local",
                "filter.breeze.smudge",
                "/nonexistent/breeze filter smudge %f",
            ],
        );
        git_ok(
            &repo,
            &repo.main,
            &[
                "config",
                "--local",
                "filter.breeze.clean",
                "/nonexistent/breeze filter clean %f",
            ],
        );
        fs::write(repo.main.join("fog.json"), "junk\n").expect("write junk");
        let checkout = git_err(&repo, &repo.main, &["checkout", "--", "fog.json"]);
        let text = output_text(&checkout);
        assert!(
            text.contains("smudge filter breeze failed") || text.contains("No such file"),
            "{text}"
        );

        let doctor = repo.breeze(&["doctor"]);
        assert_eq!(doctor.status.code(), Some(1), "{}", output_text(&doctor));
        assert!(
            output_text(&doctor).contains("filter binary is missing"),
            "{}",
            output_text(&doctor)
        );

        repo.breeze_ok(&["doctor", "--fix"]);
        let smudge = git_ok(
            &repo,
            &repo.main,
            &["config", "--local", "--get", "filter.breeze.smudge"],
        );
        assert!(!smudge.contains("/nonexistent"), "{smudge}");
        git_ok(&repo, &repo.main, &["checkout", "--", "fog.json"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);
    }

    #[test]
    fn new_worktree_materializes_patched_content() {
        let (repo, editor) = prepare("filter-worktree-add");
        enable_and_edit(&repo, &editor);

        let added = repo.base.join("wt3");
        let added_str = added.to_str().expect("utf-8 worktree path");
        git_ok(
            &repo,
            &repo.main,
            &["worktree", "add", "-b", "extra", added_str],
        );
        assert_eq!(repo.read(&added, "fog.json"), PATCHED);
        assert_clean(&repo, &added);
    }

    #[test]
    fn filter_enable_is_idempotent() {
        let (repo, editor) = prepare("filter-idempotent");
        enable(&repo);
        edit(&repo, &editor);
        enable(&repo);

        let attributes =
            fs::read_to_string(repo.main.join(".git/info/attributes")).expect("attributes");
        assert_eq!(
            attributes
                .lines()
                .filter(|line| *line == "fog.json filter=breeze")
                .count(),
            1,
            "{attributes}"
        );
        let smudges = git_ok(
            &repo,
            &repo.main,
            &["config", "--local", "--get-all", "filter.breeze.smudge"],
        );
        assert_eq!(smudges.lines().count(), 1, "{smudges}");

        assert_eq!(repo.read(&repo.main, "fog.json"), PATCHED);
        assert_clean(&repo, &repo.main);
        assert!(repo
            .store_dir()
            .join("shared/patches/fog.json.content")
            .is_file());
    }

    #[test]
    fn filter_mode_refuses_branch_and_worktree_layers() {
        let (repo, editor) = prepare("filter-layers");
        enable(&repo);

        for layer in ["branch", "worktree"] {
            let (code, stderr) =
                repo.breeze_err(&["edit", "fog.json", "--layer", layer, "--editor", &editor]);
            assert_eq!(code, 5, "{stderr}");
            assert!(
                stderr.contains("not supported in filter mode yet"),
                "{stderr}"
            );
        }

        // Enabling also refuses repos that already have non-shared patches.
        let (other, other_editor) = prepare("filter-layers-existing");
        other.breeze_ok(&[
            "edit",
            "fog.json",
            "--layer",
            "branch",
            "--editor",
            &other_editor,
        ]);
        let (code, stderr) = other.breeze_err(&["filter", "enable"]);
        assert_eq!(code, 5, "{stderr}");
        assert!(
            stderr.contains("filter mode only supports shared-layer patches"),
            "{stderr}"
        );
    }
}
