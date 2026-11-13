//! `breeze run` — execute a command with the merged overlay environment.
//!
//! Store values override the inherited process environment because tracked
//! defaults and store layers rank above the ambient environment. Values are
//! never printed.

use std::process::Command;

use anyhow::Result;

use crate::cli::RunArgs;
use crate::commands::Ctx;
use crate::envfile;
use crate::exit;
use crate::store::overlay::Repo;

pub fn run(ctx: &Ctx, args: &RunArgs) -> Result<i32> {
    if args.cmd.is_empty() {
        return Err(crate::exit::CommandError::new(exit::USAGE, "no command given").into());
    }

    let repo = Repo::open(ctx)?;
    let merged = envfile::load_merged(&repo.env_files())?;

    let program = &args.cmd[0];
    let mut command = Command::new(program);
    command.args(&args.cmd[1..]);
    for (key, value) in &merged {
        command.env(key, value);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // `exec` replaces this process, so stdin/stdout/stderr and signals are
        // inherited exactly like a shell's `exec`.
        let err = command.exec();
        let code = match err.kind() {
            std::io::ErrorKind::NotFound => 127,
            std::io::ErrorKind::PermissionDenied => 126,
            _ => 1,
        };
        eprintln!("breeze: {}: {err}", program.to_string_lossy());
        Ok(code)
    }

    #[cfg(not(unix))]
    {
        let status = command.status()?;
        Ok(status.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::{Command, Stdio};

    use crate::store::overlay::testutil::{breeze_bin, TempRepo};

    #[test]
    fn run_applies_store_values_over_the_process_environment() {
        let repo = TempRepo::new("run-env");
        fs::write(repo.store_dir().join("shared/env"), "FOO=fromstore\n").unwrap();

        let output = Command::new(breeze_bin())
            .arg("--repo")
            .arg(&repo.main)
            .args(["run", "--", "sh", "-c", "printf '%s' \"$FOO\""])
            .env("XDG_DATA_HOME", &repo.data)
            .env("FOO", "fromprocess")
            .stdin(Stdio::null())
            .output()
            .expect("run breeze");

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "fromstore");
    }

    #[test]
    fn run_passes_arguments_and_exit_status_through() {
        let repo = TempRepo::new("run-args");

        let output = Command::new(breeze_bin())
            .arg("--repo")
            .arg(&repo.main)
            .args([
                "run",
                "--",
                "sh",
                "-c",
                "printf '%s-%s' \"$1\" \"$2\"; exit 7",
                "sh",
                "a",
                "b",
            ])
            .env("XDG_DATA_HOME", &repo.data)
            .stdin(Stdio::null())
            .output()
            .expect("run breeze");

        assert_eq!(String::from_utf8_lossy(&output.stdout), "a-b");
        assert_eq!(output.status.code(), Some(7));
    }

    #[test]
    fn run_missing_command_exits_127() {
        let repo = TempRepo::new("run-missing");
        let output = Command::new(breeze_bin())
            .arg("--repo")
            .arg(&repo.main)
            .args(["run", "--", "breeze-definitely-not-a-command"])
            .env("XDG_DATA_HOME", &repo.data)
            .stdin(Stdio::null())
            .output()
            .expect("run breeze");

        assert_eq!(output.status.code(), Some(127));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("breeze-definitely-not-a-command"),
            "{stderr}"
        );
    }

    #[test]
    fn run_non_executable_exits_126() {
        let repo = TempRepo::new("run-noexec");
        let script = repo.main.join("not-executable");
        fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        }

        let output = Command::new(breeze_bin())
            .arg("--repo")
            .arg(&repo.main)
            .args(["run", "--"])
            .arg(&script)
            .env("XDG_DATA_HOME", &repo.data)
            .stdin(Stdio::null())
            .output()
            .expect("run breeze");

        assert_eq!(output.status.code(), Some(126));
    }
}
