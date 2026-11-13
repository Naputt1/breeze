//! Phase 5 hardening tests: symlink safety, private modes, redaction and the
//! plan's acceptance run-through.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::store::overlay::testutil::TempRepo;

fn write_editor(repo: &TempRepo, label: &str, body: &str) -> String {
    let path = repo.base.join(format!("{label}-editor.sh"));
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write editor");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod editor");
    }
    path.to_str().expect("editor path").to_string()
}

fn commit_file(repo: &TempRepo, relative: &str, contents: &str) {
    repo.write(relative, contents);
    repo.git(&repo.main, &["add", relative]);
    repo.git(&repo.main, &["commit", "-m", &format!("add {relative}")]);
}

fn store_files(repo: &TempRepo) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .expect("read store dir")
            .map(|entry| entry.expect("dir entry").path())
            .collect();
        entries.sort();
        for entry in entries {
            let metadata = fs::symlink_metadata(&entry).expect("store metadata");
            out.push(entry.clone());
            if metadata.is_dir() {
                walk(&entry, out);
            }
        }
    }

    let mut out = Vec::new();
    walk(&repo.store_dir(), &mut out);
    out
}

#[test]
fn store_file_swapped_for_an_outside_symlink_is_refused() {
    let repo = TempRepo::new("hardening-store-symlink");
    repo.write("secret.txt", "inside-store\n");
    repo.breeze_ok(&["link", "secret.txt"]);

    let store_file = repo.store_dir().join("shared/files/secret.txt");
    let victim = repo.base.join("outside-victim.txt");
    fs::write(&victim, "OUTSIDE SECRET VALUE\n").expect("write victim");
    fs::remove_file(&store_file).expect("remove store copy");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&victim, &store_file).expect("plant escape symlink");

    // Every store-reading command must refuse rather than trust the link.
    let doctor = repo.breeze_in(&repo.main, &["doctor"]);
    assert!(!doctor.status.success());
    let doctor_out = format!(
        "{}{}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(doctor_out.contains("BROKEN"), "{doctor_out}");
    assert!(!doctor_out.contains("OUTSIDE"), "{doctor_out}");

    let sync = repo.breeze_in(&repo.main, &["sync", "--all"]);
    assert!(!sync.status.success());
    assert!(!String::from_utf8_lossy(&sync.stdout).contains("OUTSIDE"));
    assert!(!String::from_utf8_lossy(&sync.stderr).contains("OUTSIDE"));

    let (code, stderr) = repo.breeze_err(&["link", "secret.txt"]);
    assert_eq!(code, 5, "{stderr}");
    assert!(!stderr.contains("OUTSIDE"), "{stderr}");

    let unlink = repo.breeze_in(&repo.main, &["unlink", "secret.txt"]);
    assert!(!unlink.status.success());
    assert!(!String::from_utf8_lossy(&unlink.stderr).contains("OUTSIDE"));

    assert_eq!(
        fs::read_to_string(&victim).expect("victim still readable"),
        "OUTSIDE SECRET VALUE\n",
        "breeze must never modify the symlink target"
    );
    assert!(
        fs::symlink_metadata(&store_file)
            .expect("store path")
            .file_type()
            .is_symlink(),
        "breeze must not silently replace the suspicious link"
    );
}

#[test]
fn patch_file_swapped_for_an_outside_symlink_is_not_written_through() {
    let repo = TempRepo::new("hardening-patch-symlink");
    commit_file(&repo, "fog.json", "{\"ports\":{}}\n");

    let editor = write_editor(&repo, "append", "printf '\\n// one\\n' >> \"$1\"");
    repo.breeze_ok(&["edit", "fog.json", "--editor", &editor]);

    let patch = repo
        .store_dir()
        .join("branches")
        .join(repo.branch_id(&repo.main))
        .join("patches/fog.json.patch");
    assert!(patch.is_file());

    let victim = repo.base.join("outside-patch-victim.txt");
    fs::write(&victim, "DO NOT OVERWRITE\n").expect("write victim");
    fs::remove_file(&patch).expect("remove patch");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&victim, &patch).expect("plant escape symlink");

    let editor = write_editor(&repo, "append-two", "printf '\\n// two\\n' >> \"$1\"");
    let (code, stderr) =
        repo.breeze_err(&["edit", "fog.json", "--editor", &editor, "--layer", "branch"]);
    assert_eq!(code, 5, "{stderr}");
    assert_eq!(
        fs::read_to_string(&victim).expect("victim still readable"),
        "DO NOT OVERWRITE\n"
    );
}

#[test]
fn a_full_workflow_leaves_every_store_entry_private() {
    let repo = TempRepo::new("hardening-modes");

    repo.write("linked.txt", "linked\n");
    repo.breeze_ok(&["link", "linked.txt"]);

    commit_file(&repo, "tracked.txt", "base\n");
    let editor = write_editor(&repo, "mode-editor", "printf 'local\\n' >> \"$1\"");
    repo.breeze_ok(&["edit", "tracked.txt", "--editor", &editor]);

    write_shared_env(&repo, "API_TOKEN=supersecretvalue\n");
    repo.breeze_ok(&["env", "--write"]);
    fs::write(repo.main.join(".env.local"), "LOCAL=1\n").expect("clobber .env.local");
    repo.breeze_ok(&["env", "--write", "--force"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        for path in store_files(&repo) {
            let metadata = fs::symlink_metadata(&path).expect("metadata");
            assert!(
                !metadata.file_type().is_symlink(),
                "store contains a symlink: {}",
                path.display()
            );
            let mode = metadata.permissions().mode() & 0o777;
            if metadata.is_dir() {
                assert_eq!(
                    mode,
                    0o700,
                    "directory {} has mode {mode:o}",
                    path.display()
                );
            } else {
                assert_eq!(mode, 0o600, "file {} has mode {mode:o}", path.display());
            }
        }

        let env_local = repo.main.join(".env.local");
        let mode = fs::metadata(&env_local).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

/// Write the shared layer's `env` file with the store's private permissions.
fn write_shared_env(repo: &TempRepo, contents: &str) {
    let path = repo.store_dir().join("shared/env");
    crate::store::write_private_file(&path, contents.as_bytes()).expect("write store env");
}

#[test]
fn values_never_appear_without_reveal_or_in_errors() {
    let repo = TempRepo::new("hardening-redaction");
    write_shared_env(&repo, "API_TOKEN=supersecretvalue\n");

    for format in ["dotenv", "shell", "json"] {
        let output = repo.breeze_ok(&["env", "--format", format]);
        assert!(
            !output.contains("supersecretvalue"),
            "`env --format {format}` leaked a value: {output}"
        );
        assert!(output.contains("***"), "{format}: {output}");
    }

    let status = repo.breeze_ok(&["status", "--json"]);
    assert!(!status.contains("supersecretvalue"), "{status}");
    assert!(status.contains("linked") || status.contains("patched") || status.contains("issues"));

    // A link conflict must name paths, never contents.
    repo.write("conflict.txt", "source-secret-A\n");
    repo.breeze_ok(&["link", "conflict.txt"]);
    let other_path = repo.other.join("conflict.txt");
    fs::remove_file(&other_path).expect("remove other link");
    fs::write(&other_path, "target-secret-B\n").expect("write conflicting file");

    let (code, stderr) = repo.breeze_err(&["link", "conflict.txt"]);
    assert_eq!(code, 5, "{stderr}");
    assert!(!stderr.contains("source-secret-A"), "{stderr}");
    assert!(!stderr.contains("target-secret-B"), "{stderr}");

    // Binary refusal must not echo file contents.
    commit_file(&repo, "binary.bin", "BINARYSECRET\0payload");
    let output = repo.breeze(&["edit", "binary.bin", "--editor", "true"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("BINARYSECRET"), "{stderr}");
}

#[test]
fn acceptance_run_through_from_the_plan() {
    let repo = TempRepo::new("hardening-acceptance");

    // 1. Link an ignored file; it is visible in both worktrees with a clean tree.
    repo.write(".env.local", "SECRET=local-only\n");
    repo.breeze_ok(&["link", ".env.local"]);
    assert_eq!(repo.read(&repo.main, ".env.local"), "SECRET=local-only\n");
    assert_eq!(repo.read(&repo.other, ".env.local"), "SECRET=local-only\n");
    assert!(repo.status_clean(&repo.main), "main tree is dirty");
    assert!(repo.status_clean(&repo.other), "other tree is dirty");

    // 2. Edit a tracked file through an editor; the change lives as a patch.
    repo.write("fog.json", "{\"ports\":{\"web\":8080}}\n");
    repo.git(&repo.main, &["add", "fog.json"]);
    repo.git(&repo.main, &["commit", "-m", "add fog.json"]);
    repo.git(&repo.other, &["reset", "--hard", "main"]);

    let editor = write_editor(&repo, "acceptance", "printf '// local-only\\n' >> \"$1\"");
    repo.breeze_ok(&["edit", "fog.json", "--layer", "shared", "--editor", &editor]);
    assert!(repo.read(&repo.main, "fog.json").contains("// local-only"));
    assert!(!repo.read(&repo.other, "fog.json").contains("// local-only"));

    // 3. Sync applies the patch to the other worktree.
    repo.breeze_ok(&["sync", "--all"]);
    assert!(repo.read(&repo.other, "fog.json").contains("// local-only"));

    // 4. Guard blocks a commit that would capture the overlay.
    repo.breeze_ok(&["guard", "install"]);
    repo.git(&repo.main, &["add", "fog.json"]);
    let blocked = Command::new("git")
        .arg("-C")
        .arg(&repo.main)
        .args(["commit", "-m", "leak local overlay"])
        .env("GIT_EDITOR", "true")
        .output()
        .expect("run git commit");
    assert!(!blocked.status.success(), "guard did not block the commit");
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(stderr.contains("breeze"), "{stderr}");
    assert_eq!(
        repo.git(&repo.main, &["log", "-1", "--format=%s"]).trim(),
        "add fog.json",
        "the blocked commit must not land"
    );

    // 5. Promote makes the change intentional, and the commit succeeds.
    repo.breeze_ok(&["promote", "fog.json"]);
    repo.git(&repo.main, &["add", "fog.json"]);
    repo.git(&repo.main, &["commit", "-m", "promote local overlay"]);
    assert!(repo
        .git(&repo.main, &["show", "HEAD:fog.json"])
        .contains("// local-only"));

    let overrides =
        fs::read_to_string(repo.store_dir().join("state/overrides.json")).expect("overrides");
    assert!(
        !overrides.contains("fog.json"),
        "promote should drop the patch entry: {overrides}"
    );
    assert!(repo.status_clean(&repo.main), "tree dirty after promote");
}
