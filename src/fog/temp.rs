//! Temporary merged fog configs under the user cache directory.
//!
//! Files are named `<repo-id>/<branch-id>-<pid>.json`, written with mode 0600
//! inside a 0700 directory. Since `breeze fog` replaces itself with `fog` via
//! `exec`, Breeze cannot remove the file after fog exits; stale files are
//! pruned on later runs instead.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::store::write_private_file;

const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// `$XDG_CACHE_HOME/breeze/fog`, falling back to `~/.cache/breeze/fog`.
pub fn cache_root() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cache")))
        .context("could not determine a cache directory; set XDG_CACHE_HOME or HOME")?;
    Ok(base.join("breeze").join("fog"))
}

/// Write `config` as this process's temp file and return its path.
pub fn write_config(dir: &Path, branch_id: &str, pid: u32, config: &Value) -> Result<PathBuf> {
    let path = dir.join(format!("{branch_id}-{pid}.json"));
    let mut data =
        serde_json::to_string_pretty(config).context("failed to encode the merged fog config")?;
    data.push('\n');
    write_private_file(&path, data.as_bytes())?;
    Ok(path)
}

/// Remove this repository's temp files older than one day. Best effort.
pub fn prune_stale(dir: &Path) -> usize {
    let cutoff = SystemTime::now()
        .checked_sub(STALE_AFTER)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    prune_older_than(dir, cutoff)
}

fn prune_older_than(dir: &Path, cutoff: SystemTime) -> usize {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };

    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let pid = match path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(pid_from_file_name)
        {
            Some(pid) => pid,
            None => continue,
        };

        if pid_alive(pid) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(_) => continue,
        };
        if modified >= cutoff {
            continue;
        }

        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Extract the pid from a `<branch-id>-<pid>.json` file name. Branch ids
/// themselves contain dashes, so only the last segment is considered.
fn pid_from_file_name(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".json")?;
    let (_, pid) = stem.rsplit_once('-')?;
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Other platforms do not expose a cheap, portable liveness check; age alone
/// decides, matching the macOS behaviour the spec calls for.
#[cfg(not(target_os = "linux"))]
fn pid_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "breeze-fog-test-{}-{label}-{nanos}-{unique}",
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

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn writes_private_config_file() {
        let temp = TempDir::new("write");
        let dir = temp.path().join("repo-id");
        let config = serde_json::json!({ "scripts": { "dev": {} } });

        let path = write_config(&dir, "main-12345678", 4242, &config).expect("write config");

        assert_eq!(path, dir.join("main-12345678-4242.json"));
        let text = fs::read_to_string(&path).expect("read config");
        assert!(text.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("valid json"),
            config
        );

        #[cfg(unix)]
        {
            assert_eq!(mode_of(&path), 0o600);
            assert_eq!(mode_of(&dir), 0o700);
        }
    }

    #[test]
    fn parses_pids_from_file_names() {
        assert_eq!(pid_from_file_name("main-e71fa219-1234.json"), Some(1234));
        assert_eq!(pid_from_file_name("main-1234.json"), Some(1234));
        assert_eq!(pid_from_file_name("1234.json"), None);
        assert_eq!(pid_from_file_name("main-1234.txt"), None);
        assert_eq!(pid_from_file_name("main-abc.json"), None);
        assert_eq!(pid_from_file_name("main--.json"), None);
    }

    #[test]
    fn prunes_only_stale_temp_files() {
        let temp = TempDir::new("prune");
        let dir = temp.path().join("repo-id");
        fs::create_dir_all(&dir).expect("create dir");

        let stale = dir.join("main-e71fa219-999999999.json");
        let alive = dir.join(format!("main-e71fa219-{}.json", std::process::id()));
        let text = dir.join("notes.txt");
        let bad_pid = dir.join("main-e71fa219-nope.json");
        for path in [&stale, &alive, &text, &bad_pid] {
            fs::write(path, "{}").expect("write file");
        }

        let future = SystemTime::now() + Duration::from_secs(60 * 60);
        let removed = prune_older_than(&dir, future);

        assert!(!stale.exists());
        assert!(text.exists());
        assert!(bad_pid.exists());
        if cfg!(target_os = "linux") {
            assert!(alive.exists(), "never prune a live pid");
            assert_eq!(removed, 1);
        } else {
            assert!(!alive.exists());
            assert_eq!(removed, 2);
        }

        // Nothing is older than the epoch, so a second pass removes nothing.
        assert_eq!(prune_older_than(&dir, SystemTime::UNIX_EPOCH), 0);
    }

    #[test]
    fn prune_of_missing_directory_is_a_noop() {
        let temp = TempDir::new("prune-missing");
        assert_eq!(prune_stale(&temp.path().join("absent")), 0);
    }
}
