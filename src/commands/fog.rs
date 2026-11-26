//! `breeze fog` — run fog with Breeze overlays merged over the tracked config.
//!
//! Breeze only understands its own `--config` convention and the overlay
//! layout; every other argument is forwarded to fog untouched. When no overlay
//! contributes anything, fog is exec'd with the original arguments so the
//! common path costs nothing.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::cli::FogArgs;
use crate::commands::Ctx;
use crate::exit::{self, CommandError};
use crate::fog::{merge, temp};
use crate::gitinfo::GitInfo;
use crate::store::{self, identity, RepoLayout};

const FOG_PROGRAM: &str = "fog";
const BASE_CONFIG: &str = "fog.json";
const OVERLAY_NAME: &str = "fog.local.json";

pub fn run(ctx: &Ctx, args: &FogArgs) -> Result<i32> {
    let info = GitInfo::discover(ctx.repo.as_deref())?;
    let repo_id = identity::repo_id(
        info.remote.as_deref(),
        &info.main_worktree,
        &info.common_dir,
    );
    let branch_id = info.branch.as_deref().map(identity::branch_id);
    let worktree_id = identity::worktree_id(&info.root);
    let store = store::Store::discover()?;
    let layout = store.layout(&repo_id, branch_id.as_deref(), &worktree_id);

    if !store.repo_exists(&repo_id) {
        return Err(CommandError::new(
            exit::NOT_INITIALIZED,
            format!(
                "breeze is not initialized for this repository\nstore: {}\nrun `breeze init` to create it",
                layout.repo_dir.display()
            ),
        )
        .into());
    }

    let branch_label = branch_id.unwrap_or_else(|| identity::branch_id("HEAD"));
    let invocation = prepare(
        &args.args,
        &info.root,
        &repo_id,
        &branch_label,
        &overlay_paths(&layout),
        &temp::cache_root()?,
        ctx.quiet,
    )?;

    exec(invocation)
}

/// A `fog` invocation, plus the temp config it points at (when Breeze created
/// one and is therefore responsible for removing it on failure).
#[derive(Debug)]
struct Invocation {
    program: OsString,
    args: Vec<OsString>,
    temp: Option<PathBuf>,
}

/// Overlay files in precedence order: shared, then branch, then worktree.
fn overlay_paths(layout: &RepoLayout) -> Vec<PathBuf> {
    let mut paths = vec![layout.fog_local_file.clone()];
    if let Some(branch_dir) = &layout.branch_dir {
        paths.push(branch_dir.join(OVERLAY_NAME));
    }
    paths.push(layout.worktree_dir.join(OVERLAY_NAME));
    paths
}

/// Build the fog invocation, merging overlays when any of them contributes.
///
/// `fog_args` are the original arguments. A consumed `--config <path>` selects
/// the tracked base config. Without any non-empty overlay, the original
/// arguments are returned untouched (fast path).
fn prepare(
    fog_args: &[OsString],
    repo_root: &Path,
    repo_id: &str,
    branch_label: &str,
    overlays: &[PathBuf],
    cache_root: &Path,
    quiet: bool,
) -> Result<Invocation> {
    let (base_path, remaining) = resolve_base_config(fog_args, repo_root)?;

    if !base_path.is_file() {
        return Err(CommandError::new(
            exit::GENERAL,
            format!("fog config not found: {}", base_path.display()),
        )
        .into());
    }

    let mut layers = Vec::new();
    for path in overlays {
        if !path.is_file() {
            continue;
        }
        let layer = read_json(path)?;
        // An empty overlay changes nothing, so it does not force a temp file.
        if layer.as_object().is_some_and(|object| object.is_empty()) {
            continue;
        }
        layers.push(layer);
    }

    if layers.is_empty() {
        return Ok(Invocation {
            program: OsString::from(FOG_PROGRAM),
            args: fog_args.to_vec(),
            temp: None,
        });
    }

    let mut merged = read_json(&base_path)?;
    for layer in layers {
        merged = merge::deep_merge(&merged, &layer);
    }

    if merged.get("scripts").is_none() && !quiet {
        eprintln!("warning: merged fog config has no `scripts` key");
    }

    let dir = cache_root.join(repo_id);
    temp::prune_stale(&dir);
    let temp_path = temp::write_config(&dir, branch_label, std::process::id(), &merged)?;

    let mut forwarded = Vec::with_capacity(remaining.len() + 4);
    forwarded.push(OsString::from("-c"));
    forwarded.push(temp_path.clone().into_os_string());
    // The merged config lives in Breeze's cache, so fog would otherwise treat
    // that directory as the project root and resolve relative service paths
    // there. Point fog back at the worktree it was invoked from. A
    // user-supplied `--project-dir` wins and is never duplicated.
    if !has_project_dir(&remaining) {
        forwarded.push(OsString::from("--project-dir"));
        forwarded.push(repo_root.as_os_str().to_os_string());
    }
    forwarded.extend(remaining);

    Ok(Invocation {
        program: OsString::from(FOG_PROGRAM),
        args: forwarded,
        temp: Some(temp_path),
    })
}

/// Whether the forwarded arguments already select a project root.
fn has_project_dir(args: &[OsString]) -> bool {
    args.iter().any(|arg| {
        arg == OsStr::new("--project-dir")
            || arg
                .to_str()
                .is_some_and(|text| text.starts_with("--project-dir="))
    })
}

/// Locate the tracked base config and strip the consumed `--config` flag.
fn resolve_base_config(args: &[OsString], repo_root: &Path) -> Result<(PathBuf, Vec<OsString>)> {
    let mut config: Option<OsString> = None;
    let mut remaining = Vec::with_capacity(args.len());
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];

        if arg == OsStr::new("--config") {
            let value = args.get(index + 1).ok_or_else(|| {
                usage_error("`--config` requires a path (usage: breeze fog --config <path> ...)")
            })?;
            set_config(&mut config, value.clone())?;
            index += 2;
            continue;
        }

        if let Some(value) = arg.to_str().and_then(|text| text.strip_prefix("--config=")) {
            set_config(&mut config, OsString::from(value))?;
            index += 1;
            continue;
        }

        remaining.push(arg.clone());
        index += 1;
    }

    let path = match config {
        Some(value) => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                repo_root.join(path)
            }
        }
        None => repo_root.join(BASE_CONFIG),
    };

    Ok((path, remaining))
}

fn set_config(slot: &mut Option<OsString>, value: OsString) -> Result<()> {
    if slot.is_some() {
        return Err(usage_error("`--config` may only be given once"));
    }
    *slot = Some(value);
    Ok(())
}

fn usage_error(message: impl Into<String>) -> anyhow::Error {
    CommandError::new(exit::USAGE, message).into()
}

fn read_json(path: &Path) -> Result<Value> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| {
        CommandError::new(
            exit::GENERAL,
            format!("invalid JSON in {}: {error}", path.display()),
        )
        .into()
    })
}

fn remove_temp(temp: &Option<PathBuf>) {
    if let Some(path) = temp {
        let _ = fs::remove_file(path);
    }
}

/// Replace this process with fog. Signals therefore reach fog directly, and
/// because Breeze no longer exists after a successful exec there is nothing to
/// clean up then: the pid-named temp file is left for the prune pass. Failure
/// paths still remove the file before returning.
#[cfg(unix)]
fn exec(invocation: Invocation) -> Result<i32> {
    use std::os::unix::process::CommandExt;

    let error = Command::new(&invocation.program)
        .args(&invocation.args)
        .exec();

    remove_temp(&invocation.temp);

    let program = invocation.program.to_string_lossy().into_owned();
    Err(CommandError::new(127, format!("failed to run {program}: {error}")).into())
}

#[cfg(not(unix))]
fn exec(invocation: Invocation) -> Result<i32> {
    let status = Command::new(&invocation.program)
        .args(&invocation.args)
        .status();
    remove_temp(&invocation.temp);

    match status {
        Ok(status) => Ok(status.code().unwrap_or(exit::GENERAL)),
        Err(error) => {
            let program = invocation.program.to_string_lossy().into_owned();
            Err(CommandError::new(127, format!("failed to run {program}: {error}")).into())
        }
    }
}

/// Test seam: run an invocation and wait for it, instead of replacing the test
/// process with `exec`.
#[cfg(test)]
fn run_fog(program: &OsStr, args: &[OsString]) -> Result<i32> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {}", program.to_string_lossy()))?;
    Ok(status.code().unwrap_or(exit::GENERAL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "breeze-fog-cmd-test-{}-{label}-{nanos}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write file");
    }

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn error_code(error: &anyhow::Error) -> i32 {
        error
            .downcast_ref::<CommandError>()
            .expect("command error")
            .code
    }

    struct Fixture {
        temp: TempDir,
        repo: PathBuf,
        base_config: String,
        shared: PathBuf,
    }

    impl Fixture {
        fn new(label: &str, base_config: &str, overlay: &str) -> Self {
            let temp = TempDir::new(label);
            let repo = temp.path().join("repo");
            fs::create_dir_all(&repo).expect("create repo");
            write_file(&repo.join("fog.json"), base_config);

            let shared = temp.path().join("store/shared/fog.local.json");
            write_file(&shared, overlay);

            Self {
                temp,
                repo,
                base_config: base_config.to_string(),
                shared,
            }
        }

        fn cache(&self) -> PathBuf {
            self.temp.path().join("cache")
        }
    }

    const BASE_CONFIG: &str = r#"{
        "scripts": {
            "dev": {
                "service": [
                    { "name": "api", "port": 3000 },
                    { "name": "worker", "cmd": "watch" }
                ]
            }
        },
        "ports": { "web": 8080 },
        "native_routes": [{ "host": "base" }]
    }"#;

    const OVERLAY: &str = r#"{
        "scripts": {
            "dev": {
                "service": [{ "name": "api", "port": 4000 }, { "name": "cron" }],
                "env": { "LOG": "debug" }
            }
        },
        "ports": { "web": 9090, "api": 8081 },
        "native_routes": [{ "host": "overlay" }]
    }"#;

    #[test]
    fn merges_overlays_and_writes_temp_config() {
        let fixture = Fixture::new("merge", BASE_CONFIG, OVERLAY);
        let missing_branch = fixture
            .temp
            .path()
            .join("store/branches/dev-x/fog.local.json");
        let original = os_args(&["up", "--port", "1234"]);

        let invocation = prepare(
            &original,
            &fixture.repo,
            "demo-12345678",
            "main-abcdef12",
            &[fixture.shared.clone(), missing_branch],
            &fixture.cache(),
            true,
        )
        .expect("prepare invocation");

        assert_eq!(invocation.program, OsString::from("fog"));
        assert_eq!(invocation.args[0], OsString::from("-c"));

        let temp_path = PathBuf::from(&invocation.args[1]);
        assert_eq!(invocation.temp.as_deref(), Some(temp_path.as_path()));
        assert!(temp_path.starts_with(fixture.cache().join("demo-12345678")));
        assert!(!temp_path.starts_with(&fixture.repo));
        assert_eq!(
            temp_path.file_name().unwrap().to_string_lossy(),
            format!("main-abcdef12-{}.json", std::process::id())
        );
        assert_eq!(invocation.args[2], OsString::from("--project-dir"));
        assert_eq!(invocation.args[3], fixture.repo.as_os_str().to_os_string());
        assert_eq!(&invocation.args[4..], original.as_slice());

        let merged: Value =
            serde_json::from_str(&fs::read_to_string(&temp_path).expect("read temp"))
                .expect("json");
        assert_eq!(merged["ports"], json!({ "web": 9090, "api": 8081 }));
        assert_eq!(
            merged["scripts"]["dev"]["service"],
            json!([
                { "name": "api", "port": 4000 },
                { "name": "worker", "cmd": "watch" },
                { "name": "cron" }
            ])
        );
        assert_eq!(merged["scripts"]["dev"]["env"], json!({ "LOG": "debug" }));
        assert_eq!(merged["native_routes"], json!([{ "host": "overlay" }]));

        // The tracked base config is untouched and no overlay file appeared.
        assert_eq!(
            fs::read_to_string(fixture.repo.join("fog.json")).expect("read base"),
            fixture.base_config
        );
        assert_eq!(
            fs::read_dir(&fixture.repo)
                .expect("read repo")
                .filter_map(|entry| entry.ok())
                .count(),
            1
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&temp_path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn no_overlay_is_a_fast_path() {
        let fixture = Fixture::new("fast", BASE_CONFIG, "{}");
        let overlay_dir = fixture.temp.path().join("store/branches/main-abcdef12");
        let original = os_args(&["up", "--port", "1234"]);

        let invocation = prepare(
            &original,
            &fixture.repo,
            "demo-12345678",
            "main-abcdef12",
            &[
                fixture.shared.clone(),
                overlay_dir.join("fog.local.json"),
                fixture.temp.path().join("absent/fog.local.json"),
            ],
            &fixture.cache(),
            true,
        )
        .expect("prepare invocation");

        assert!(invocation.temp.is_none());
        assert_eq!(invocation.args, original);
        assert!(
            !invocation
                .args
                .iter()
                .any(|arg| arg == OsStr::new("--project-dir")),
            "the fast path must not inject --project-dir"
        );
        assert!(!fixture.cache().exists(), "no cache files on the fast path");
    }

    #[test]
    fn config_flag_selects_the_base_and_is_not_forwarded() {
        let temp = TempDir::new("config-flag");
        let repo = temp.path().join("repo");
        let custom = repo.join("config/fog.alt.json");
        write_file(
            &custom,
            r#"{ "ports": { "web": 1 }, "scripts": { "dev": {} } }"#,
        );
        let overlay = temp.path().join("store/shared/fog.local.json");
        write_file(&overlay, r#"{ "ports": { "web": 2 } }"#);
        let original = os_args(&["--config", "config/fog.alt.json", "up"]);

        let invocation = prepare(
            &original,
            &repo,
            "demo-12345678",
            "main-abcdef12",
            std::slice::from_ref(&overlay),
            &temp.path().join("cache"),
            true,
        )
        .expect("prepare invocation");

        assert_eq!(invocation.args[0], OsString::from("-c"));
        let merged: Value = serde_json::from_str(
            &fs::read_to_string(PathBuf::from(&invocation.args[1])).expect("read"),
        )
        .expect("json");
        assert_eq!(merged["ports"]["web"], 2);
        assert_eq!(
            &invocation.args[2..],
            &[
                OsString::from("--project-dir"),
                repo.as_os_str().to_os_string(),
                OsString::from("up")
            ]
        );
        // With no overlay the original arguments (including --config) pass through.
        let fast = prepare(
            &original,
            &repo,
            "demo-12345678",
            "main-abcdef12",
            &[temp.path().join("absent/fog.local.json")],
            &temp.path().join("cache"),
            true,
        )
        .expect("prepare fast path");
        assert!(fast.temp.is_none());
        assert_eq!(fast.args, original);
    }

    #[test]
    fn merged_invocation_forwards_the_repo_root_as_project_dir() {
        let fixture = Fixture::new("project-dir", BASE_CONFIG, OVERLAY);
        let invocation = prepare(
            &os_args(&["dev", "--port", "1234"]),
            &fixture.repo,
            "demo-12345678",
            "main-abcdef12",
            std::slice::from_ref(&fixture.shared),
            &fixture.cache(),
            true,
        )
        .expect("prepare invocation");

        assert_eq!(invocation.args[0], OsString::from("-c"));
        assert_eq!(invocation.args[2], OsString::from("--project-dir"));
        assert_eq!(invocation.args[3], fixture.repo.as_os_str().to_os_string());
    }

    #[test]
    fn user_project_dir_is_not_duplicated_in_the_merged_invocation() {
        let fixture = Fixture::new("explicit-project-dir", BASE_CONFIG, OVERLAY);

        for explicit in [
            os_args(&["dev", "--project-dir", "/elsewhere"]),
            os_args(&["dev", "--project-dir=/elsewhere"]),
        ] {
            let invocation = prepare(
                &explicit,
                &fixture.repo,
                "demo-12345678",
                "main-abcdef12",
                std::slice::from_ref(&fixture.shared),
                &fixture.cache(),
                true,
            )
            .expect("prepare invocation");

            let injected = invocation
                .args
                .iter()
                .filter(|arg| {
                    *arg == OsStr::new("--project-dir")
                        || arg
                            .to_str()
                            .is_some_and(|text| text.starts_with("--project-dir="))
                })
                .count();
            assert_eq!(injected, 1, "explicit --project-dir must be preserved once");
            assert_eq!(&invocation.args[2..], explicit.as_slice());
        }
    }

    #[test]
    fn duplicate_config_flag_is_a_usage_error() {
        let fixture = Fixture::new("dup-config", BASE_CONFIG, OVERLAY);
        let error = prepare(
            &os_args(&["--config", "a.json", "--config", "b.json"]),
            &fixture.repo,
            "demo-12345678",
            "main-abcdef12",
            std::slice::from_ref(&fixture.shared),
            &fixture.cache(),
            true,
        )
        .expect_err("duplicate --config must fail");
        assert_eq!(error_code(&error), exit::USAGE);
    }

    #[test]
    fn missing_config_flag_value_is_a_usage_error() {
        let fixture = Fixture::new("missing-value", BASE_CONFIG, OVERLAY);
        let error = prepare(
            &os_args(&["up", "--config"]),
            &fixture.repo,
            "demo-12345678",
            "main-abcdef12",
            std::slice::from_ref(&fixture.shared),
            &fixture.cache(),
            true,
        )
        .expect_err("missing --config value must fail");
        assert_eq!(error_code(&error), exit::USAGE);
    }

    #[test]
    fn missing_base_config_is_a_general_error() {
        let fixture = Fixture::new("missing-base", BASE_CONFIG, OVERLAY);
        fs::remove_file(fixture.repo.join("fog.json")).expect("remove base");
        let error = prepare(
            &os_args(&["up"]),
            &fixture.repo,
            "demo-12345678",
            "main-abcdef12",
            std::slice::from_ref(&fixture.shared),
            &fixture.cache(),
            true,
        )
        .expect_err("missing base must fail");
        assert_eq!(error_code(&error), exit::GENERAL);
        assert!(error.to_string().contains("fog config not found"));
    }

    #[test]
    fn invalid_overlay_json_is_rejected() {
        let fixture = Fixture::new("bad-overlay", BASE_CONFIG, "{ not json");
        let error = prepare(
            &os_args(&["up"]),
            &fixture.repo,
            "demo-12345678",
            "main-abcdef12",
            std::slice::from_ref(&fixture.shared),
            &fixture.cache(),
            true,
        )
        .expect_err("invalid overlay must fail");
        assert_eq!(error_code(&error), exit::GENERAL);
    }

    #[cfg(unix)]
    #[test]
    fn fake_fog_receives_arguments_in_order() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("fake-fog", BASE_CONFIG, OVERLAY);
        let log_args = fixture.temp.path().join("fog-args.log");
        let log_config = fixture.temp.path().join("fog-config.json");

        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprev=''\nfor a in \"$@\"; do\n  if [ \"$prev\" = '-c' ]; then cp \"$a\" '{}'; fi\n  prev=\"$a\"\ndone\n",
            log_args.display(),
            log_config.display()
        );
        let bin = fixture.temp.path().join("bin/fog");
        write_file(&bin, &script);
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod");

        let original = os_args(&["up", "--port", "1234"]);
        let invocation = prepare(
            &original,
            &fixture.repo,
            "demo-12345678",
            "main-abcdef12",
            std::slice::from_ref(&fixture.shared),
            &fixture.cache(),
            true,
        )
        .expect("prepare");

        let code = run_fog(&bin.clone().into_os_string(), &invocation.args).expect("run fake fog");
        assert_eq!(code, exit::SUCCESS);

        let args = fs::read_to_string(&log_args).expect("read args log");
        let seen: Vec<&str> = args.lines().collect();
        let temp_path = PathBuf::from(&invocation.args[1]);
        let expected = vec![
            "-c",
            temp_path.to_str().expect("utf-8 temp path"),
            "--project-dir",
            fixture.repo.to_str().expect("utf-8 repo path"),
            "up",
            "--port",
            "1234",
        ];
        assert_eq!(seen, expected);

        let forwarded: Value =
            serde_json::from_str(&fs::read_to_string(&log_config).expect("read config log"))
                .expect("json");
        assert_eq!(forwarded["ports"]["web"], 9090);
        assert_eq!(forwarded["scripts"]["dev"]["service"][0]["port"], 4000);
    }

    #[cfg(unix)]
    #[test]
    fn fast_path_runs_fake_fog_with_original_arguments() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("fake-fast", BASE_CONFIG, "{}");
        let log_args = fixture.temp.path().join("fog-args.log");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            log_args.display()
        );
        let bin = fixture.temp.path().join("bin/fog");
        write_file(&bin, &script);
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod");

        let original = os_args(&["up", "--port", "1234"]);
        let invocation = prepare(
            &original,
            &fixture.repo,
            "demo-12345678",
            "main-abcdef12",
            std::slice::from_ref(&fixture.shared),
            &fixture.cache(),
            true,
        )
        .expect("prepare");

        assert!(invocation.temp.is_none());
        let code = run_fog(&bin.clone().into_os_string(), &invocation.args).expect("run fake fog");
        assert_eq!(code, exit::SUCCESS);

        let args = fs::read_to_string(&log_args).expect("read args log");
        let seen: Vec<&str> = args.lines().collect();
        assert_eq!(seen, vec!["up", "--port", "1234"]);
    }
}
