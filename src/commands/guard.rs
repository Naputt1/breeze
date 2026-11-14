//! `breeze guard` — install, remove and inspect the commit/push guard hooks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::{CheckRebaseArgs, GuardArgs, GuardCommand, GuardInstallArgs};
use crate::commands::{revert, Ctx};
use crate::exit::{self, CommandError};
use crate::store::now_rfc3339;
use crate::store::overlay::{self, Repo};
use crate::store::overrides::OverrideMode;

const HOOKS: [&str; 2] = ["pre-commit", "pre-push"];
const POST_CHECKOUT: &str = "post-checkout";
const POST_MERGE: &str = "post-merge";
const POST_REWRITE: &str = "post-rewrite";
const POST_APPLYPATCH: &str = "post-applypatch";
const PRE_REBASE: &str = "pre-rebase";
const MANAGED_HOOKS: [&str; 7] = [
    "pre-commit",
    "pre-push",
    POST_CHECKOUT,
    POST_MERGE,
    POST_REWRITE,
    POST_APPLYPATCH,
    PRE_REBASE,
];
const MARKER: &str = "# breeze-managed (installed ";

pub fn run(ctx: &Ctx, args: &GuardArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    match &args.command {
        GuardCommand::Install(install_args) => install(&repo, ctx, install_args),
        GuardCommand::Uninstall => uninstall(&repo, ctx),
        GuardCommand::Status => status(&repo, ctx),
        GuardCommand::CheckRebase(check_args) => check_rebase(&repo, ctx, check_args),
    }
}

fn install(repo: &Repo, ctx: &Ctx, args: &GuardInstallArgs) -> Result<i32> {
    if let Some(custom) = configured_hooks_path(repo)? {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!(
                "core.hooksPath is set to `{custom}`; Breeze only manages hooks under {}, so installation was refused\nunset core.hooksPath or install the hooks there manually",
                repo.info.common_dir.join("hooks").display()
            ),
        )
        .into());
    }

    let hooks_dir = repo.info.common_dir.join("hooks");
    create_hooks_dir(&hooks_dir)?;
    let installed = now_rfc3339();
    let overrides = repo.layout.overrides_file.display().to_string();
    let binary = std::env::current_exe()
        .context("failed to resolve the absolute path of the breeze binary")?
        .display()
        .to_string();
    let root = repo.info.root.display().to_string();

    let mut names: Vec<&str> = HOOKS.to_vec();
    if args.with_post_checkout || args.with_all_hooks {
        names.push(POST_CHECKOUT);
    }
    if args.with_pull_hooks || args.with_all_hooks {
        names.push(POST_MERGE);
        names.push(POST_REWRITE);
    }
    if args.with_all_hooks {
        names.push(POST_APPLYPATCH);
        names.push(PRE_REBASE);
    }

    let mut report = Vec::new();
    for name in names {
        let path = hooks_dir.join(name);
        let backup = backup_path(&hooks_dir, name);

        if path.is_file() {
            let existing = fs::read_to_string(&path).unwrap_or_default();
            if !existing.contains(MARKER.trim_end()) {
                if !backup.exists() {
                    fs::copy(&path, &backup).with_context(|| {
                        format!("failed to back up existing hook {}", path.display())
                    })?;
                    set_hook_mode(&backup)?;
                    report.push(format!("backed up existing {name} to {}", backup.display()));
                } else {
                    report.push(format!(
                        "kept existing backup {} (current {name} will be replaced)",
                        backup.display()
                    ));
                }
            }
        }

        let script = match name {
            "pre-commit" => pre_commit_script(&overrides, &installed),
            "pre-push" => pre_push_script(&overrides, &installed),
            POST_CHECKOUT => post_checkout_script(&binary, &root, &installed),
            POST_MERGE | POST_REWRITE | POST_APPLYPATCH => {
                pull_hook_script(name, &binary, &root, &installed)
            }
            PRE_REBASE => pre_rebase_script(&binary, args.rebase_auto_revert, &installed),
            _ => unreachable!("unmanaged hook {name}"),
        };
        write_hook(&path, &script)?;
        report.push(format!("installed {}", path.display()));
    }

    if !ctx.quiet {
        println!("guard installed for {}", repo.info.root.display());
        for line in report {
            println!("  {line}");
        }
    }

    Ok(exit::SUCCESS)
}

fn check_rebase(repo: &Repo, ctx: &Ctx, args: &CheckRebaseArgs) -> Result<i32> {
    let overrides = repo.overrides()?;
    let current = repo.current_target();
    let mut all_paths: Vec<String> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.mode == OverrideMode::Patch)
        .map(|entry| entry.path.clone())
        .collect();
    all_paths.sort();
    all_paths.dedup();

    let active: Vec<String> = all_paths
        .into_iter()
        .filter(|path| {
            overlay::best_entry(
                overrides
                    .overrides
                    .iter()
                    .filter(|entry| entry.mode == OverrideMode::Patch && &entry.path == path),
                &current,
            )
            .is_some()
        })
        .collect();

    if active.is_empty() {
        return Ok(exit::SUCCESS);
    }

    if args.auto_revert {
        eprintln!(
            "breeze: reverting {} tracked patch overlay(s) before the rebase; backups are kept",
            active.len()
        );
        for path in &active {
            revert::revert_relative(repo, ctx, path, true)?;
        }
        return Ok(exit::SUCCESS);
    }

    eprintln!(
        "breeze: {} tracked patch overlay(s) are active; a rebase can discard or conflict with them",
        active.len()
    );
    for path in active.iter().take(10) {
        eprintln!("  {path}");
    }
    if active.len() > 10 {
        eprintln!("  ... and {} more", active.len() - 10);
    }
    eprintln!("run `breeze revert --all` and retry the rebase");
    eprintln!(
        "or reinstall with `breeze guard install --with-all-hooks --rebase-auto-revert` to revert automatically"
    );
    Ok(exit::GUARD_BLOCKED)
}

fn uninstall(repo: &Repo, ctx: &Ctx) -> Result<i32> {
    let hooks_dir = repo.info.common_dir.join("hooks");

    for name in MANAGED_HOOKS {
        let path = hooks_dir.join(name);
        let backup = backup_path(&hooks_dir, name);

        if path.is_file() {
            let contents = fs::read_to_string(&path).unwrap_or_default();
            if contents.contains(MARKER.trim_end()) {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
                if !ctx.quiet {
                    println!("removed {}", path.display());
                }
            } else if !ctx.quiet {
                println!("left non-breeze hook in place: {}", path.display());
            }
        }

        if backup.is_file() {
            if path.exists() {
                if !ctx.quiet {
                    println!(
                        "WARN {} exists; not restoring {}",
                        path.display(),
                        backup.display()
                    );
                }
            } else {
                fs::rename(&backup, &path)
                    .with_context(|| format!("failed to restore {}", path.display()))?;
                set_hook_mode(&path)?;
                if !ctx.quiet {
                    println!("restored {}", path.display());
                }
            }
        }
    }

    Ok(exit::SUCCESS)
}

fn status(repo: &Repo, ctx: &Ctx) -> Result<i32> {
    let hooks_dir = repo.info.common_dir.join("hooks");

    if let Some(custom) = configured_hooks_path(repo)? {
        println!("core.hooksPath: {custom} (Breeze cannot manage hooks here)");
    }

    for name in MANAGED_HOOKS {
        let path = hooks_dir.join(name);
        let backup = backup_path(&hooks_dir, name);
        let installed = fs::read_to_string(&path)
            .map(|contents| contents.contains(MARKER.trim_end()))
            .unwrap_or(false);
        let state = if installed {
            "installed"
        } else {
            "not installed"
        };
        match opt_in_hint(name) {
            Some(hint) if !installed => println!(
                "{name}: {state} (opt-in: `breeze guard install {hint}`) ({})",
                path.display()
            ),
            _ => println!("{name}: {state} ({})", path.display()),
        }
        if backup.is_file() {
            println!("  backup: {}", backup.display());
        }
    }

    if !ctx.quiet {
        println!("hooks: {}", hooks_dir.display());
    }
    Ok(exit::SUCCESS)
}

fn configured_hooks_path(repo: &Repo) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo.info.root)
        .args(["config", "--get", "core.hooksPath"])
        .output()
        .context("failed to run git config")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if value.is_empty() { None } else { Some(value) })
}

fn backup_path(hooks_dir: &Path, name: &str) -> PathBuf {
    hooks_dir.join(format!("{name}.breeze-backup"))
}

fn opt_in_hint(name: &str) -> Option<&'static str> {
    match name {
        POST_CHECKOUT => Some("--with-post-checkout"),
        POST_MERGE | POST_REWRITE => Some("--with-pull-hooks"),
        POST_APPLYPATCH | PRE_REBASE => Some("--with-all-hooks"),
        _ => None,
    }
}

fn create_hooks_dir(hooks_dir: &Path) -> Result<()> {
    fs::create_dir_all(hooks_dir)
        .with_context(|| format!("failed to create {}", hooks_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(hooks_dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set permissions on {}", hooks_dir.display()))?;
    }
    Ok(())
}

fn write_hook(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;
    set_hook_mode(path)
}

fn set_hook_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn overridden_function() -> &'static str {
    r#"overridden() {
  target="$1"
  while IFS= read -r candidate; do
    if [ "$candidate" = "$target" ]; then
      return 0
    fi
  done <<EOF
$(sed -n 's/.*"path": "\(.*\)".*/\1/p' "$OVERRIDES")
EOF
  return 1
}"#
}

fn pre_commit_script(overrides: &str, installed: &str) -> String {
    let template = r#"#!/bin/sh
# breeze-managed (installed __INSTALLED__)
# Blocks commits that stage paths Breeze keeps local-only.
OVERRIDES=__OVERRIDES__

if [ "${BREEZE_ALLOW_COMMIT:-}" = "1" ]; then
  exit 0
fi

[ -f "$OVERRIDES" ] || exit 0

__OVERRIDDEN__

blocked=""
while IFS= read -r path; do
  [ -z "$path" ] && continue
  if overridden "$path"; then
    blocked="${blocked}  ${path}
"
  fi
done <<EOF
$(git diff --cached --name-only --diff-filter=ACMR)
EOF

if [ -n "$blocked" ]; then
  printf 'Breeze: local-only paths staged. Unstage, promote (breeze promote <path>), or set BREEZE_ALLOW_COMMIT=1 to bypass.\n' >&2
  printf '%s' "$blocked" >&2
  exit 6
fi

backup="$(dirname "$0")/pre-commit.breeze-backup"
if [ -x "$backup" ]; then
  "$backup" "$@"
  exit $?
fi

exit 0
"#;

    template
        .replace("__INSTALLED__", installed)
        .replace("__OVERRIDES__", &shell_quote(overrides))
        .replace("__OVERRIDDEN__", overridden_function())
}

fn pre_push_script(overrides: &str, installed: &str) -> String {
    let template = r#"#!/bin/sh
# breeze-managed (installed __INSTALLED__)
# Blocks pushes that include paths Breeze keeps local-only.
OVERRIDES=__OVERRIDES__

if [ "${BREEZE_ALLOW_COMMIT:-}" = "1" ]; then
  exit 0
fi

[ -f "$OVERRIDES" ] || exit 0

zero=0000000000000000000000000000000000000000
empty_tree=$(git hash-object -t tree /dev/null 2>/dev/null)

backup="$(dirname "$0")/pre-push.breeze-backup"
stdin_copy=""
if [ -x "$backup" ]; then
  stdin_copy=$(mktemp 2>/dev/null) || stdin_copy=""
  if [ -n "$stdin_copy" ]; then
    cat > "$stdin_copy"
    exec 0< "$stdin_copy"
  fi
fi

__OVERRIDDEN__

blocked=""
while read -r local_ref local_sha remote_ref remote_sha; do
  [ -z "$local_sha" ] && continue
  [ "$local_sha" = "$zero" ] && continue
  if [ -z "$remote_sha" ] || [ "$remote_sha" = "$zero" ]; then
    base="$empty_tree"
  else
    base="$remote_sha"
  fi
  while IFS= read -r path; do
    [ -z "$path" ] && continue
    if overridden "$path"; then
      blocked="${blocked}  ${path}
"
    fi
  done <<EOF
$(git diff --name-only "$base" "$local_sha" 2>/dev/null)
EOF
done

if [ -n "$blocked" ]; then
  printf 'Breeze: local-only paths would be pushed. Unstage or promote (breeze promote <path>), or set BREEZE_ALLOW_COMMIT=1 to bypass.\n' >&2
  printf '%s' "$blocked" >&2
  exit 6
fi

if [ -x "$backup" ]; then
  if [ -n "$stdin_copy" ]; then
    "$backup" "$@" < "$stdin_copy"
    status=$?
    rm -f "$stdin_copy"
    exit $status
  fi
  "$backup" "$@"
  exit $?
fi

[ -n "$stdin_copy" ] && rm -f "$stdin_copy"
exit 0
"#;

    template
        .replace("__INSTALLED__", installed)
        .replace("__OVERRIDES__", &shell_quote(overrides))
        .replace("__OVERRIDDEN__", overridden_function())
}

fn post_checkout_script(binary: &str, root: &str, installed: &str) -> String {
    let template = r#"#!/bin/sh
# breeze-managed (installed __INSTALLED__)
# Opt-in post-checkout hook: hydrate overlays after a branch or worktree checkout.
# Git also calls this for file checkouts ($3 = 0); those must not reapply patches.
# Breeze must never fail the checkout, so every step is best effort.
BREEZE=__BINARY__
REPO=__REPO__

if [ "$3" = "1" ]; then
  if [ ! -x "$BREEZE" ]; then
    printf 'breeze: warning: embedded binary not found at %s; run `breeze guard install` again.\n' "$BREEZE" >&2
  elif "$BREEZE" --repo "$REPO" --quiet status >/dev/null 2>&1; then
    if ! "$BREEZE" --repo "$REPO" --quiet sync --all; then
      printf 'breeze: patch sync needs attention; run breeze doctor\n' >&2
    fi
  fi
fi

backup="${0%/*}/post-checkout.breeze-backup"
if [ -x "$backup" ]; then
  "$backup" "$@" || true
fi

exit 0
"#;

    template
        .replace("__INSTALLED__", installed)
        .replace("__BINARY__", &shell_quote(binary))
        .replace("__REPO__", &shell_quote(root))
}

fn pull_hook_script(name: &str, binary: &str, root: &str, installed: &str) -> String {
    let template = r#"#!/bin/sh
# breeze-managed (installed __INSTALLED__)
# Opt-in __NAME__ hook: refresh overlays after a local history operation.
# Breeze must never fail the git operation, so every step is best effort.
BREEZE=__BINARY__
REPO=__REPO__

if [ ! -x "$BREEZE" ]; then
  printf 'breeze: warning: embedded binary not found at %s; run `breeze guard install` again.\n' "$BREEZE" >&2
elif "$BREEZE" --repo "$REPO" --quiet status >/dev/null 2>&1; then
  if ! "$BREEZE" --repo "$REPO" --quiet sync --all; then
    printf 'breeze: patch sync needs attention; run breeze doctor\n' >&2
  fi
fi

backup="${0%/*}/__NAME__.breeze-backup"
if [ -x "$backup" ]; then
  "$backup" "$@" || true
fi

exit 0
"#;

    template
        .replace("__INSTALLED__", installed)
        .replace("__NAME__", name)
        .replace("__BINARY__", &shell_quote(binary))
        .replace("__REPO__", &shell_quote(root))
}

fn pre_rebase_script(binary: &str, auto_revert: bool, installed: &str) -> String {
    let template = r#"#!/bin/sh
# breeze-managed (installed __INSTALLED__)
# Opt-in pre-rebase hook: block a rebase while Breeze patches are active,
# or revert them first when auto-revert was requested at install time.
# Breeze must never lose tracked patches silently.
BREEZE=__BINARY__

if [ ! -x "$BREEZE" ]; then
  printf 'breeze: warning: embedded binary not found at %s; run `breeze guard install` again.\n' "$BREEZE" >&2
else
  "$BREEZE" --repo "$(pwd)" --quiet guard check-rebase__AUTO_REVERT__
  status=$?
  case "$status" in
    0|4) ;;
    *) exit "$status" ;;
  esac
fi

backup="${0%/*}/pre-rebase.breeze-backup"
if [ -x "$backup" ]; then
  "$backup" "$@" || exit $?
fi

exit 0
"#;

    template
        .replace("__INSTALLED__", installed)
        .replace(
            "__AUTO_REVERT__",
            if auto_revert { " --auto-revert" } else { "" },
        )
        .replace("__BINARY__", &shell_quote(binary))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use crate::store::overlay::testutil::TempRepo;

    use crate::commands::edit::tests::{commit_file, write_editor};

    fn hook(repo: &TempRepo, name: &str) -> std::path::PathBuf {
        repo.main.join(".git").join("hooks").join(name)
    }

    fn make_link(repo: &TempRepo) {
        repo.write("local.txt", "local\n");
        repo.breeze_ok(&["link", "local.txt"]);
    }

    fn break_link(repo: &TempRepo) {
        fs::remove_file(repo.main.join("local.txt")).unwrap();
    }

    fn link_present(_repo: &TempRepo, root: &Path) -> bool {
        fs::symlink_metadata(root.join("local.txt"))
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
    }

    /// Run Git with the temporary Breeze store in its environment, so hooks
    /// invoked by Git resolve the same store the test set up.
    fn git_ok(repo: &TempRepo, dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("XDG_DATA_HOME", &repo.data)
            .env("XDG_CACHE_HOME", repo.base.join("cache"))
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git_failure(repo: &TempRepo, dir: &Path, args: &[&str]) -> std::process::Output {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("XDG_DATA_HOME", &repo.data)
            .env("XDG_CACHE_HOME", repo.base.join("cache"))
            .output()
            .expect("run git");
        assert!(
            !output.status.success(),
            "git {:?} unexpectedly succeeded",
            args
        );
        output
    }

    #[test]
    fn pre_commit_blocks_staged_override_paths_and_honours_the_override_env() {
        let repo = TempRepo::new("guard-precommit");
        commit_file(&repo, "fog.json", "base\n");
        let editor = write_editor(&repo, "guard-add", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--editor", &editor]);

        repo.breeze_ok(&["guard", "install"]);
        let pre_commit = hook(&repo, "pre-commit");
        assert!(pre_commit.is_file());

        repo.git(&repo.main, &["add", "fog.json"]);

        let status = Command::new(&pre_commit)
            .current_dir(&repo.main)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(6));

        let allowed = Command::new(&pre_commit)
            .current_dir(&repo.main)
            .env("BREEZE_ALLOW_COMMIT", "1")
            .status()
            .unwrap();
        assert_eq!(allowed.code(), Some(0));
    }

    #[test]
    fn pre_push_blocks_commits_that_include_override_paths() {
        let repo = TempRepo::new("guard-prepush");
        commit_file(&repo, "fog.json", "base\n");
        let editor = write_editor(&repo, "guard-push-add", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--editor", &editor]);
        repo.breeze_ok(&["guard", "install"]);

        repo.git(&repo.main, &["add", "fog.json"]);
        repo.git(&repo.main, &["commit", "--no-verify", "-m", "local-only"]);

        let pre_push = hook(&repo, "pre-push");
        let head = repo.git(&repo.main, &["rev-parse", "HEAD"]);
        let input = format!(
            "refs/heads/main {} refs/heads/main 0000000000000000000000000000000000000000\n",
            head.trim()
        );

        let mut child = Command::new(&pre_push)
            .current_dir(&repo.main)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(6));

        let mut allowed = Command::new(&pre_push)
            .current_dir(&repo.main)
            .env("BREEZE_ALLOW_COMMIT", "1")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        allowed
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        assert_eq!(allowed.wait().unwrap().code(), Some(0));
    }

    #[test]
    fn install_backs_up_and_chains_an_existing_hook_and_uninstall_restores_it() {
        let repo = TempRepo::new("guard-chain");
        let hooks = repo.main.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let custom = hooks.join("pre-commit");
        fs::write(&custom, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&custom, fs::Permissions::from_mode(0o700)).unwrap();

        repo.breeze_ok(&["guard", "install"]);
        assert!(hooks.join("pre-commit.breeze-backup").is_file());
        assert!(fs::read_to_string(&custom)
            .unwrap()
            .contains("breeze-managed"));

        let status = repo.breeze_ok(&["guard", "status"]);
        assert!(status.contains("pre-commit: installed"), "{status}");
        assert!(status.contains("backup:"), "{status}");

        repo.breeze_ok(&["guard", "uninstall"]);
        assert!(!fs::read_to_string(&custom)
            .unwrap()
            .contains("breeze-managed"));
        assert!(!hooks.join("pre-commit.breeze-backup").exists());

        let status = repo.breeze_ok(&["guard", "status"]);
        assert!(status.contains("pre-commit: not installed"), "{status}");
    }

    #[test]
    fn install_refuses_a_custom_hooks_path() {
        let repo = TempRepo::new("guard-custom");
        repo.git(&repo.main, &["config", "core.hooksPath", ".githooks"]);
        let (code, stderr) = repo.breeze_err(&["guard", "install"]);
        assert_eq!(code, 5);
        assert!(stderr.contains("core.hooksPath"), "{stderr}");
    }

    #[test]
    fn post_checkout_is_opt_in_chains_and_never_fails_the_checkout() {
        let repo = TempRepo::new("guard-post-checkout");
        let hooks = repo.main.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let custom = hooks.join("post-checkout");
        let marker = repo.base.join("post-checkout-chained.marker");
        let original = format!("#!/bin/sh\necho chained > '{}'\n", marker.display());
        fs::write(&custom, &original).unwrap();
        fs::set_permissions(&custom, fs::Permissions::from_mode(0o700)).unwrap();

        let status = repo.breeze_ok(&["guard", "status"]);
        assert!(status.contains("post-checkout: not installed"), "{status}");

        repo.breeze_ok(&["guard", "install", "--with-post-checkout"]);
        let contents = fs::read_to_string(&custom).unwrap();
        assert!(contents.contains("breeze-managed"), "{contents}");
        assert!(contents.contains("BREEZE='"), "{contents}");
        assert!(!contents.contains("command -v breeze"), "{contents}");
        assert!(contents.contains("\"$3\" = \"1\""), "{contents}");
        assert!(contents.contains("sync --all"), "{contents}");
        assert!(
            contents.contains("patch sync needs attention"),
            "{contents}"
        );
        assert!(hooks.join("post-checkout.breeze-backup").is_file());

        let status = repo.breeze_ok(&["guard", "status"]);
        assert!(status.contains("post-checkout: installed"), "{status}");

        // The hook exits 0 even when sync cannot do useful work, and chains.
        let output = Command::new(&custom)
            .args(["old", "new", "1"])
            .current_dir(&repo.main)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(marker.is_file(), "the pre-existing hook was not chained");

        repo.breeze_ok(&["guard", "uninstall"]);
        assert_eq!(fs::read_to_string(&custom).unwrap(), original);
        assert!(!hooks.join("post-checkout.breeze-backup").exists());
        let status = repo.breeze_ok(&["guard", "status"]);
        assert!(status.contains("post-checkout: not installed"), "{status}");
    }

    #[test]
    fn pull_hooks_are_opt_in_and_install_both_when_requested() {
        let repo = TempRepo::new("guard-pull-optin");
        let hooks = repo.main.join(".git/hooks");

        repo.breeze_ok(&["guard", "install"]);
        assert!(!hooks.join("post-merge").exists());
        assert!(!hooks.join("post-rewrite").exists());

        let status = repo.breeze_ok(&["guard", "status"]);
        assert!(status.contains("post-merge: not installed"), "{status}");
        assert!(status.contains("post-rewrite: not installed"), "{status}");
        assert!(status.contains("--with-pull-hooks"), "{status}");

        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);
        for name in ["post-merge", "post-rewrite"] {
            let path = hooks.join(name);
            let contents = fs::read_to_string(&path).unwrap();
            assert!(contents.contains("breeze-managed"), "{contents}");
            assert!(contents.contains("BREEZE='"), "{contents}");
            assert!(!contents.contains("command -v breeze"), "{contents}");
            assert!(contents.contains("embedded binary not found"), "{contents}");
            assert!(contents.contains("--quiet status"), "{contents}");
            assert!(contents.contains("sync --all"), "{contents}");
            assert!(
                contents.contains("breeze: patch sync needs attention; run breeze doctor"),
                "{contents}"
            );
            assert!(contents.contains("exit 0"), "{contents}");
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "{name} is not executable");
        }

        let status = repo.breeze_ok(&["guard", "status"]);
        assert!(status.contains("post-merge: installed"), "{status}");
        assert!(status.contains("post-rewrite: installed"), "{status}");
    }

    #[test]
    fn pull_hooks_chain_existing_hooks_and_reinstall_is_idempotent() {
        let repo = TempRepo::new("guard-pull-chain");
        let hooks = repo.main.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let merge_marker = repo.base.join("post-merge-chained.marker");
        let rewrite_marker = repo.base.join("post-rewrite-chained.marker");
        let merge_original = format!("#!/bin/sh\necho \"$@\" > '{}'\n", merge_marker.display());
        let rewrite_original = format!("#!/bin/sh\necho \"$@\" > '{}'\n", rewrite_marker.display());
        for (name, body) in [
            ("post-merge", merge_original.as_str()),
            ("post-rewrite", rewrite_original.as_str()),
        ] {
            let path = hooks.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }

        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);
        assert_eq!(
            fs::read_to_string(hooks.join("post-merge.breeze-backup")).unwrap(),
            merge_original
        );
        assert_eq!(
            fs::read_to_string(hooks.join("post-rewrite.breeze-backup")).unwrap(),
            rewrite_original
        );

        // Reinstalling keeps the original backup and never stacks a second block.
        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);
        let contents = fs::read_to_string(hooks.join("post-merge")).unwrap();
        assert_eq!(contents.matches("breeze-managed").count(), 1, "{contents}");
        assert_eq!(
            fs::read_to_string(hooks.join("post-merge.breeze-backup")).unwrap(),
            merge_original
        );

        // With no `breeze` on PATH the hooks stay silent, exit 0, and chain.
        let empty_bin = repo.base.join("empty-bin");
        fs::create_dir_all(&empty_bin).unwrap();

        let output = Command::new(hooks.join("post-merge"))
            .arg("1")
            .current_dir(&repo.main)
            .env("PATH", &empty_bin)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            fs::read_to_string(&merge_marker).unwrap().trim(),
            "1",
            "the pre-existing post-merge hook was not chained"
        );

        let output = Command::new(hooks.join("post-rewrite"))
            .arg("amend")
            .current_dir(&repo.main)
            .env("PATH", &empty_bin)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            fs::read_to_string(&rewrite_marker).unwrap().trim(),
            "amend",
            "the pre-existing post-rewrite hook was not chained"
        );

        repo.breeze_ok(&["guard", "uninstall"]);
        assert_eq!(
            fs::read_to_string(hooks.join("post-merge")).unwrap(),
            merge_original
        );
        assert_eq!(
            fs::read_to_string(hooks.join("post-rewrite")).unwrap(),
            rewrite_original
        );
        assert!(!hooks.join("post-merge.breeze-backup").exists());
        assert!(!hooks.join("post-rewrite.breeze-backup").exists());
        let status = repo.breeze_ok(&["guard", "status"]);
        assert!(status.contains("post-merge: not installed"), "{status}");
        assert!(status.contains("post-rewrite: not installed"), "{status}");
    }

    #[test]
    fn pull_hook_warns_but_never_fails_when_sync_reports_conflicts() {
        let repo = TempRepo::new("guard-pull-warning");
        commit_file(&repo, "fog.json", "base\n");
        let editor = write_editor(&repo, "guard-pull-add", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--layer", "shared", "--editor", &editor]);

        // A conflicting commit in the linked worktree makes the shared patch
        // fail its three-way apply during `sync --all`.
        fs::write(repo.other.join("fog.json"), "other\n").unwrap();
        repo.git(&repo.other, &["add", "fog.json"]);
        repo.git(&repo.other, &["commit", "-m", "conflicting change"]);

        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);
        let output = Command::new(hook(&repo, "post-merge"))
            .arg("1")
            .current_dir(&repo.main)
            .env("XDG_DATA_HOME", &repo.data)
            .env("XDG_CACHE_HOME", repo.base.join("cache"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("breeze: patch sync needs attention; run breeze doctor"),
            "{stderr}"
        );
    }

    #[test]
    fn pull_hook_uses_the_embedded_binary_even_with_a_hostile_path() {
        let repo = TempRepo::new("guard-pull-absolute");
        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);

        let bin = repo.base.join("fake-bin");
        fs::create_dir_all(&bin).unwrap();
        let log = repo.base.join("fake-breeze.log");
        let fake = bin.join("breeze");
        fs::write(
            &fake,
            format!("#!/bin/sh\necho invoked >> '{}'\nexit 3\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();

        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new(hook(&repo, "post-merge"))
            .arg("1")
            .current_dir(&repo.main)
            .env("PATH", &path)
            .env("XDG_DATA_HOME", &repo.data)
            .env("XDG_CACHE_HOME", repo.base.join("cache"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(!log.exists(), "the hook ran breeze from PATH");
    }

    #[test]
    fn pull_hook_warns_when_the_embedded_binary_is_missing() {
        let repo = TempRepo::new("guard-pull-missing");
        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);

        let installed = hook(&repo, "post-merge");
        let contents = fs::read_to_string(&installed).unwrap();
        let broken = contents
            .lines()
            .map(|line| {
                if line.starts_with("BREEZE=") {
                    "BREEZE='/nonexistent/breeze'".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&installed, format!("{broken}\n")).unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o700)).unwrap();

        let output = Command::new(&installed)
            .arg("1")
            .current_dir(&repo.main)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("embedded binary not found"), "{stderr}");
    }

    #[test]
    fn with_all_hooks_installs_the_full_set_and_pull_flags_stay_scoped() {
        let repo = TempRepo::new("guard-all-hooks");
        let hooks = repo.main.join(".git/hooks");

        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);
        for name in ["post-merge", "post-rewrite"] {
            assert!(hooks.join(name).is_file(), "{name} was not installed");
        }
        for name in ["post-applypatch", "pre-rebase"] {
            assert!(
                !hooks.join(name).exists(),
                "{name} must not be part of --with-pull-hooks"
            );
        }

        repo.breeze_ok(&["guard", "install", "--with-all-hooks"]);
        for name in [
            "pre-commit",
            "pre-push",
            "post-checkout",
            "post-merge",
            "post-rewrite",
            "post-applypatch",
            "pre-rebase",
        ] {
            let path = hooks.join(name);
            assert!(path.is_file(), "{name} was not installed");
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "{name} is not executable");
        }

        let status = repo.breeze_ok(&["guard", "status"]);
        assert!(status.contains("post-applypatch: installed"), "{status}");
        assert!(status.contains("pre-rebase: installed"), "{status}");

        repo.breeze_ok(&["guard", "uninstall"]);
        for name in [
            "pre-commit",
            "pre-push",
            "post-checkout",
            "post-merge",
            "post-rewrite",
            "post-applypatch",
            "pre-rebase",
        ] {
            assert!(!hooks.join(name).exists(), "{name} was not removed");
        }
    }

    #[test]
    fn post_checkout_skips_file_checkouts_and_syncs_branch_checkouts() {
        let repo = TempRepo::new("guard-post-checkout-file");
        commit_file(&repo, "fog.json", "base\n");
        let editor = write_editor(&repo, "guard-checkout-add", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--layer", "shared", "--editor", &editor]);
        repo.breeze_ok(&["guard", "install", "--with-post-checkout"]);

        // A file checkout restores HEAD content; the hook must not reapply
        // the patch over a deliberate restore.
        git_ok(&repo, &repo.main, &["checkout", "--", "fog.json"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\n");

        // A branch checkout does sync, so the shared patch comes back.
        git_ok(&repo, &repo.main, &["checkout", "-b", "guard-other"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\nlocal\n");
    }

    #[test]
    fn merge_fast_forward_repairs_links() {
        let repo = TempRepo::new("guard-merge-ff");
        commit_file(&repo, "base.txt", "base\n");
        make_link(&repo);
        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);

        git_ok(&repo, &repo.other, &["merge", "main"]);
        fs::write(repo.other.join("feature.txt"), "feature\n").unwrap();
        repo.git(&repo.other, &["add", "feature.txt"]);
        repo.git(&repo.other, &["commit", "-m", "feature"]);

        break_link(&repo);
        git_ok(&repo, &repo.main, &["merge", "--ff-only", "feature"]);
        assert!(
            link_present(&repo, &repo.main),
            "post-merge did not repair the link"
        );
    }

    #[test]
    fn merge_non_fast_forward_repairs_links() {
        let repo = TempRepo::new("guard-merge-noff");
        commit_file(&repo, "base.txt", "base\n");
        make_link(&repo);
        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);

        fs::write(repo.other.join("feature.txt"), "feature\n").unwrap();
        repo.git(&repo.other, &["add", "feature.txt"]);
        repo.git(&repo.other, &["commit", "-m", "feature"]);

        repo.write("main.txt", "main\n");
        repo.git(&repo.main, &["add", "main.txt"]);
        repo.git(&repo.main, &["commit", "-m", "main"]);

        break_link(&repo);
        git_ok(
            &repo,
            &repo.main,
            &["merge", "--no-ff", "feature", "-m", "merge"],
        );
        assert!(
            link_present(&repo, &repo.main),
            "post-merge did not repair the link"
        );
    }

    #[test]
    fn pull_fast_forward_repairs_links() {
        let repo = TempRepo::new("guard-pull-ff");
        commit_file(&repo, "base.txt", "base\n");
        let remote = repo.base.join("remote.git");
        repo.git(&repo.base, &["init", "--bare", remote.to_str().unwrap()]);
        repo.git(
            &repo.main,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        repo.git(&repo.main, &["push", "-u", "origin", "main"]);
        // The store identity includes the remote name, so re-init after
        // adding the remote before recording overlays.
        repo.breeze_ok(&["init"]);
        make_link(&repo);
        repo.git(&repo.other, &["merge", "main"]);
        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);

        fs::write(repo.other.join("remote.txt"), "remote\n").unwrap();
        git_ok(&repo, &repo.other, &["add", "remote.txt"]);
        git_ok(&repo, &repo.other, &["commit", "-m", "remote change"]);
        git_ok(&repo, &repo.other, &["push", "origin", "feature:main"]);

        break_link(&repo);
        git_ok(&repo, &repo.main, &["pull", "--ff-only", "origin", "main"]);
        assert!(
            link_present(&repo, &repo.main),
            "the pull did not run post-merge"
        );
    }

    #[test]
    fn pull_with_a_merge_repairs_links() {
        let repo = TempRepo::new("guard-pull-merge");
        commit_file(&repo, "base.txt", "base\n");
        let remote = repo.base.join("remote.git");
        repo.git(&repo.base, &["init", "--bare", remote.to_str().unwrap()]);
        repo.git(
            &repo.main,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        repo.git(&repo.main, &["push", "-u", "origin", "main"]);
        repo.breeze_ok(&["init"]);
        make_link(&repo);
        repo.git(&repo.other, &["merge", "main"]);
        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);

        fs::write(repo.other.join("remote.txt"), "remote\n").unwrap();
        git_ok(&repo, &repo.other, &["add", "remote.txt"]);
        git_ok(&repo, &repo.other, &["commit", "-m", "remote change"]);
        git_ok(&repo, &repo.other, &["push", "origin", "feature:main"]);

        repo.write("main.txt", "main\n");
        git_ok(&repo, &repo.main, &["add", "main.txt"]);
        git_ok(&repo, &repo.main, &["commit", "-m", "local change"]);

        break_link(&repo);
        git_ok(
            &repo,
            &repo.main,
            &["pull", "--no-rebase", "origin", "main"],
        );
        assert!(
            link_present(&repo, &repo.main),
            "the merging pull did not run post-merge"
        );
    }

    #[test]
    fn pull_rebase_repairs_links_through_post_rewrite() {
        let repo = TempRepo::new("guard-pull-rebase");
        commit_file(&repo, "base.txt", "base\n");
        let remote = repo.base.join("remote.git");
        repo.git(&repo.base, &["init", "--bare", remote.to_str().unwrap()]);
        repo.git(
            &repo.main,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        repo.git(&repo.main, &["push", "-u", "origin", "main"]);
        repo.breeze_ok(&["init"]);
        make_link(&repo);
        repo.git(&repo.other, &["merge", "main"]);
        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);

        fs::write(repo.other.join("remote.txt"), "remote\n").unwrap();
        git_ok(&repo, &repo.other, &["add", "remote.txt"]);
        git_ok(&repo, &repo.other, &["commit", "-m", "remote change"]);
        git_ok(&repo, &repo.other, &["push", "origin", "feature:main"]);

        repo.write("main.txt", "main\n");
        git_ok(&repo, &repo.main, &["add", "main.txt"]);
        git_ok(&repo, &repo.main, &["commit", "-m", "local change"]);

        break_link(&repo);
        git_ok(&repo, &repo.main, &["pull", "--rebase", "origin", "main"]);
        assert!(
            link_present(&repo, &repo.main),
            "the rebasing pull did not run post-rewrite"
        );
    }

    #[test]
    fn successful_rebase_repairs_links() {
        let repo = TempRepo::new("guard-rebase-ok");
        commit_file(&repo, "base.txt", "base\n");
        make_link(&repo);
        repo.git(&repo.other, &["merge", "main"]);
        repo.breeze_ok(&["guard", "install", "--with-all-hooks"]);

        fs::write(repo.other.join("feature.txt"), "feature\n").unwrap();
        repo.git(&repo.other, &["add", "feature.txt"]);
        repo.git(&repo.other, &["commit", "-m", "feature"]);

        repo.write("main.txt", "main\n");
        repo.git(&repo.main, &["add", "main.txt"]);
        repo.git(&repo.main, &["commit", "-m", "main"]);

        break_link(&repo);
        git_ok(&repo, &repo.main, &["rebase", "feature"]);
        assert!(
            link_present(&repo, &repo.main),
            "the rebase did not run post-rewrite"
        );
    }

    #[test]
    fn conflicting_and_aborted_rebase_runs_no_post_hook() {
        let repo = TempRepo::new("guard-rebase-conflict");
        commit_file(&repo, "fog.json", "base\n");
        make_link(&repo);
        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);
        repo.git(&repo.other, &["merge", "main"]);

        fs::write(repo.other.join("fog.json"), "feature\n").unwrap();
        repo.git(&repo.other, &["add", "fog.json"]);
        repo.git(&repo.other, &["commit", "-m", "feature change"]);

        repo.write("fog.json", "main\n");
        repo.git(&repo.main, &["add", "fog.json"]);
        repo.git(&repo.main, &["commit", "-m", "main change"]);

        break_link(&repo);
        let output = git_failure(&repo, &repo.main, &["rebase", "feature"]);
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            combined.contains("CONFLICT") || combined.contains("could not apply"),
            "{combined}"
        );
        assert!(
            !link_present(&repo, &repo.main),
            "a conflicting rebase must not run post-rewrite"
        );

        git_ok(&repo, &repo.main, &["rebase", "--abort"]);
        assert!(
            !link_present(&repo, &repo.main),
            "an aborted rebase must not run post-rewrite"
        );
    }

    #[test]
    fn git_am_success_runs_post_applypatch_and_repairs_links() {
        let repo = TempRepo::new("guard-am");
        commit_file(&repo, "base.txt", "base\n");
        make_link(&repo);

        fs::write(repo.other.join("am.txt"), "am\n").unwrap();
        repo.git(&repo.other, &["add", "am.txt"]);
        repo.git(&repo.other, &["commit", "-m", "am change"]);
        let patches = repo.base.join("patches");
        fs::create_dir_all(&patches).unwrap();
        repo.git(
            &repo.other,
            &["format-patch", "-1", "-o", patches.to_str().unwrap()],
        );
        let patch = fs::read_dir(&patches)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();

        repo.breeze_ok(&["guard", "install", "--with-all-hooks"]);
        break_link(&repo);
        git_ok(&repo, &repo.main, &["am", patch.to_str().unwrap()]);
        assert!(
            link_present(&repo, &repo.main),
            "post-applypatch did not repair the link"
        );
    }

    #[test]
    fn worktree_add_runs_post_checkout_and_hydrates_the_new_worktree() {
        let repo = TempRepo::new("guard-worktree-add");
        commit_file(&repo, "base.txt", "base\n");
        make_link(&repo);
        repo.breeze_ok(&["guard", "install", "--with-post-checkout"]);

        let added = repo.base.join("added-worktree");
        git_ok(
            &repo,
            &repo.main,
            &["worktree", "add", added.to_str().unwrap(), "-b", "added"],
        );
        assert!(
            link_present(&repo, &added),
            "post-checkout did not hydrate the new worktree"
        );
    }

    #[test]
    fn pre_rebase_blocks_while_patches_are_active_and_allows_after_revert() {
        let repo = TempRepo::new("guard-pre-rebase-block");
        commit_file(&repo, "fog.json", "base\n");
        let editor = write_editor(&repo, "guard-rebase-edit", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--layer", "shared", "--editor", &editor]);
        repo.git(&repo.other, &["merge", "main"]);

        fs::write(repo.other.join("feature.txt"), "feature\n").unwrap();
        repo.git(&repo.other, &["add", "feature.txt"]);
        repo.git(&repo.other, &["commit", "-m", "feature"]);
        repo.breeze_ok(&["guard", "install", "--with-all-hooks"]);

        // Git refuses a dirty worktree before it runs pre-rebase, so commit the
        // patched content (bypassing the guard) to exercise the hook itself.
        git_ok(&repo, &repo.main, &["add", "fog.json"]);
        git_ok(
            &repo,
            &repo.main,
            &["commit", "--no-verify", "-m", "local patch"],
        );

        let output = git_failure(&repo, &repo.main, &["rebase", "feature"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("breeze revert --all"), "{stderr}");
        assert!(
            repo.read(&repo.main, "fog.json").contains("local"),
            "the patch was silently discarded"
        );

        // Reverting removes the patch; restore the committed content so the
        // worktree is clean, then pre-rebase allows the rebase.
        repo.breeze_ok(&["revert", "--all"]);
        git_ok(&repo, &repo.main, &["checkout", "--", "fog.json"]);
        git_ok(&repo, &repo.main, &["rebase", "feature"]);
    }

    #[test]
    fn pre_rebase_auto_revert_restores_patches_and_allows_rebase() {
        let repo = TempRepo::new("guard-pre-rebase-auto");
        commit_file(&repo, "fog.json", "base\n");
        let editor = write_editor(&repo, "guard-auto-edit", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--layer", "shared", "--editor", &editor]);
        repo.git(&repo.other, &["merge", "main"]);

        fs::write(repo.other.join("feature.txt"), "feature\n").unwrap();
        repo.git(&repo.other, &["add", "feature.txt"]);
        repo.git(&repo.other, &["commit", "-m", "feature"]);
        repo.breeze_ok(&[
            "guard",
            "install",
            "--with-all-hooks",
            "--rebase-auto-revert",
        ]);

        // Exercise the hook body directly: a normal Git rebase refuses the
        // dirty worktree before invoking pre-rebase on current Git versions.
        repo.breeze_ok(&["guard", "check-rebase", "--auto-revert"]);
        assert_eq!(repo.read(&repo.main, "fog.json"), "base\n");
        let status = repo.breeze_ok(&["status", "--json"]);
        assert!(!status.contains("\"patched\":[{"), "{status}");

        git_ok(&repo, &repo.main, &["rebase", "feature"]);
    }

    #[test]
    fn post_rewrite_chains_existing_hook_with_args_and_stdin() {
        let repo = TempRepo::new("guard-rewrite-chain");
        commit_file(&repo, "base.txt", "base\n");
        let hooks = repo.main.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let args_marker = repo.base.join("rewrite-args.marker");
        let stdin_marker = repo.base.join("rewrite-stdin.marker");
        let custom = format!(
            "#!/bin/sh\nprintf '%s' \"$*\" > '{}'\ncat > '{}'\n",
            args_marker.display(),
            stdin_marker.display()
        );
        let path = hooks.join("post-rewrite");
        fs::write(&path, &custom).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();

        repo.breeze_ok(&["guard", "install", "--with-pull-hooks"]);
        git_ok(&repo, &repo.main, &["commit", "--amend", "--no-edit"]);

        assert_eq!(fs::read_to_string(&args_marker).unwrap(), "amend");
        assert!(
            !fs::read_to_string(&stdin_marker).unwrap().is_empty(),
            "the chained hook did not receive stdin"
        );
    }
}
