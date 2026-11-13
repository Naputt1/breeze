//! On-disk store: paths, layout, repository metadata and private permissions.

pub mod exclude;
pub mod identity;
pub mod overlay;
pub mod overrides;
pub mod patches;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Current store metadata schema version.
pub const SCHEMA_VERSION: u32 = 1;

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// Contents of `repos/<repo-id>/repo.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMetadata {
    pub schema_version: u32,
    pub repo_id: String,
    pub root: String,
    pub common_dir: String,
    pub remote: Option<String>,
    pub main_worktree: String,
    pub created_at: String,
}

impl RepoMetadata {
    pub fn new(
        repo_id: String,
        root: &Path,
        common_dir: &Path,
        remote: Option<String>,
        main_worktree: &Path,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            repo_id,
            root: root.display().to_string(),
            common_dir: common_dir.display().to_string(),
            remote,
            main_worktree: main_worktree.display().to_string(),
            created_at: now_rfc3339(),
        }
    }
}

/// Location of the global store.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Resolve the store root: `$XDG_DATA_HOME/breeze`, then the platform data
    /// directory (via `dirs`), then `~/.local/share/breeze`.
    pub fn discover() -> Result<Self> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(dirs::data_dir)
            .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("share")))
            .context("could not determine a data directory; set XDG_DATA_HOME or HOME")?;
        Ok(Self {
            root: base.join("breeze"),
        })
    }

    #[cfg(test)]
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn repo_dir(&self, repo_id: &str) -> PathBuf {
        self.root.join("repos").join(repo_id)
    }

    /// Compute paths for one repository without creating anything.
    pub fn layout(&self, repo_id: &str, branch_id: Option<&str>, worktree_id: &str) -> RepoLayout {
        RepoLayout::new(self.repo_dir(repo_id), branch_id, worktree_id)
    }

    /// Whether `repos/<repo-id>/repo.json` exists.
    pub fn repo_exists(&self, repo_id: &str) -> bool {
        self.repo_dir(repo_id).join("repo.json").is_file()
    }

    /// Create the repository store layout and metadata files.
    pub fn create_repo(
        &self,
        meta: &RepoMetadata,
        branch_id: Option<&str>,
        worktree_id: &str,
    ) -> Result<RepoLayout> {
        let layout = self.layout(&meta.repo_id, branch_id, worktree_id);

        create_private_dir(&self.root)?;
        create_private_dir(&self.root.join("repos"))?;

        for dir in [
            &layout.repo_dir,
            &layout.shared_dir,
            &layout.files_dir,
            &layout.branches_dir,
            &layout.worktrees_dir,
            &layout.worktree_dir,
            &layout.state_dir,
            &layout.backups_dir,
        ] {
            create_private_dir(dir)?;
        }
        if let Some(branch_dir) = &layout.branch_dir {
            create_private_dir(branch_dir)?;
        }

        write_private_file(&layout.env_file, b"")?;
        write_private_file(&layout.fog_local_file, b"{}\n")?;
        write_private_file(&layout.events_log, b"")?;
        write_private_file(&layout.lock_file, b"")?;
        write_json(&layout.repo_file, meta)?;
        overrides::OverridesFile::empty().save(&layout.overrides_file)?;

        Ok(layout)
    }

    pub fn load_meta(&self, repo_id: &str) -> Result<RepoMetadata> {
        read_json(&self.repo_dir(repo_id).join("repo.json"))
    }
}

/// All paths owned by one repository inside the store.
#[derive(Debug, Clone)]
pub struct RepoLayout {
    pub repo_dir: PathBuf,
    pub repo_file: PathBuf,
    pub shared_dir: PathBuf,
    pub files_dir: PathBuf,
    pub env_file: PathBuf,
    pub fog_local_file: PathBuf,
    pub branches_dir: PathBuf,
    pub branch_dir: Option<PathBuf>,
    pub worktrees_dir: PathBuf,
    pub worktree_dir: PathBuf,
    pub state_dir: PathBuf,
    pub overrides_file: PathBuf,
    pub events_log: PathBuf,
    pub lock_file: PathBuf,
    pub backups_dir: PathBuf,
}

impl RepoLayout {
    pub fn new(repo_dir: PathBuf, branch_id: Option<&str>, worktree_id: &str) -> Self {
        let shared_dir = repo_dir.join("shared");
        let branches_dir = repo_dir.join("branches");
        let worktrees_dir = repo_dir.join("worktrees");
        let state_dir = repo_dir.join("state");

        Self {
            repo_file: repo_dir.join("repo.json"),
            shared_dir: shared_dir.clone(),
            files_dir: shared_dir.join("files"),
            env_file: shared_dir.join("env"),
            fog_local_file: shared_dir.join("fog.local.json"),
            branch_dir: branch_id.map(|id| branches_dir.join(id)),
            branches_dir,
            worktree_dir: worktrees_dir.join(worktree_id),
            worktrees_dir,
            overrides_file: state_dir.join("overrides.json"),
            events_log: state_dir.join("events.log"),
            lock_file: state_dir.join("lock"),
            state_dir,
            backups_dir: repo_dir.join("backups"),
            repo_dir,
        }
    }
}

/// Create (or tighten) a directory to mode 0700.
pub(crate) fn create_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(crate::exit::CommandError::new(
                    crate::exit::UNSAFE_REFUSED,
                    format!(
                        "refusing to use symlinked directory {} (it may escape the store)",
                        path.display()
                    ),
                )
                .into());
            }
            if !metadata.is_dir() {
                bail!("{} exists but is not a directory", path.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .with_context(|| format!("failed to create directory {}", path.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect directory {}", path.display()));
        }
    }
    set_mode(path, DIR_MODE)
}

/// Write a file with mode 0600, creating parent directories as needed.
pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }

    // Never write through a link: a store file swapped for a symlink must not
    // redirect writes outside the store.
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(crate::exit::CommandError::new(
                crate::exit::UNSAFE_REFUSED,
                format!(
                    "refusing to write through symlink {} (it may escape the store)",
                    path.display()
                ),
            )
            .into());
        }
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
    }

    let mut file = options
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    drop(file);
    set_mode(path, FILE_MODE)
}

pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut data = serde_json::to_string_pretty(value)
        .with_context(|| format!("failed to encode {}", path.display()))?;
    data.push('\n');
    write_private_file(path, data.as_bytes())
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let data =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("invalid JSON in {}", path.display()))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Current UTC time formatted as RFC 3339, without a chrono dependency.
pub fn now_rfc3339() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format_rfc3339(seconds)
}

/// Format Unix seconds as RFC 3339 UTC.
pub fn format_rfc3339(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let time = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3_600,
        (time % 3_600) / 60,
        time % 60
    )
}

/// Convert days since the Unix epoch to a civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
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
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "breeze-test-{}-{label}-{nanos}-{unique}",
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

    fn mode_of(path: &Path) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(path).expect("metadata").permissions().mode() & 0o777
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            0
        }
    }

    #[test]
    fn timestamp_formatting() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_rfc3339(1_700_086_400), "2023-11-15T22:13:20Z");
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn creates_layout_and_metadata() {
        let temp = TempDir::new("layout");
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).expect("create repo root");
        let store = Store::at(temp.path().join("data").join("breeze"));

        let meta = RepoMetadata::new(
            "demo-12345678".to_string(),
            &repo_root,
            &repo_root.join(".git"),
            None,
            &repo_root,
        );
        let layout = store
            .create_repo(&meta, Some("main-12345678"), "repo-12345678")
            .expect("create store");

        assert!(store.repo_exists("demo-12345678"));
        assert!(layout.repo_file.is_file());
        assert!(layout.files_dir.is_dir());
        assert!(layout.branch_dir.as_ref().expect("branch dir").is_dir());
        assert!(layout.worktree_dir.is_dir());
        assert!(layout.state_dir.is_dir());
        assert!(layout.backups_dir.is_dir());
        assert!(layout.env_file.is_file());
        assert!(layout.fog_local_file.is_file());
        assert!(layout.events_log.is_file());
        assert!(layout.lock_file.is_file());

        let loaded = store.load_meta("demo-12345678").expect("load metadata");
        assert_eq!(loaded.repo_id, meta.repo_id);
        assert_eq!(loaded.root, meta.root);
        assert_eq!(loaded.common_dir, meta.common_dir);
        assert_eq!(loaded.remote, None);
        assert_eq!(loaded.main_worktree, meta.main_worktree);
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert!(loaded.created_at.ends_with('Z'));

        let overrides =
            overrides::OverridesFile::load(&layout.overrides_file).expect("load overrides");
        assert!(overrides.overrides.is_empty());

        if cfg!(unix) {
            assert_eq!(mode_of(&layout.repo_dir), 0o700);
            assert_eq!(mode_of(&layout.files_dir), 0o700);
            assert_eq!(mode_of(&layout.state_dir), 0o700);
            assert_eq!(mode_of(&layout.repo_file), 0o600);
            assert_eq!(mode_of(&layout.overrides_file), 0o600);
            assert_eq!(mode_of(&layout.env_file), 0o600);
        }
    }

    #[test]
    fn overrides_roundtrip() {
        let temp = TempDir::new("overrides");
        let path = temp.path().join("state").join("overrides.json");
        let now = now_rfc3339();

        let mut file = overrides::OverridesFile::empty();
        file.overrides.push(overrides::OverrideEntry {
            path: "config/local.toml".to_string(),
            mode: overrides::OverrideMode::Patch,
            layer: "shared".to_string(),
            source: "shared/files/config/local.toml".to_string(),
            tracked: true,
            base_sha: Some("deadbeef".to_string()),
            created_at: now.clone(),
            updated_at: now,
        });
        file.save(&path).expect("save overrides");

        let loaded = overrides::OverridesFile::load(&path).expect("load overrides");
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.overrides.len(), 1);
        assert_eq!(loaded.counts(), (0, 1));
        assert_eq!(loaded.overrides[0].path, "config/local.toml");
        assert_eq!(loaded.overrides[0].base_sha.as_deref(), Some("deadbeef"));

        if cfg!(unix) {
            assert_eq!(mode_of(&path), 0o600);
        }
    }

    #[test]
    fn missing_overrides_file_is_empty() {
        let temp = TempDir::new("missing");
        let file =
            overrides::OverridesFile::load(&temp.path().join("overrides.json")).expect("load");
        assert!(file.overrides.is_empty());
        assert_eq!(file.schema_version, SCHEMA_VERSION);
    }
}
