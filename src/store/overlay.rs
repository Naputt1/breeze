//! Shared overlay machinery used by the Phase 1 commands.
//!
//! This module owns layer resolution, worktree discovery, relative symlinks,
//! backups and the event log. Commands stay thin on top of it.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use crate::commands::Ctx;
use crate::exit::{self, CommandError};
use crate::gitinfo::GitInfo;
use crate::store::identity;
use crate::store::overrides::{OverrideEntry, OverridesFile};
use crate::store::{create_private_dir, now_rfc3339, write_private_file, RepoLayout, Store};

/// Overlay layers, ordered from lowest to highest precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Shared,
    Branch,
    Worktree,
}

impl Layer {
    /// Parse a `--layer` value; the default is `shared`.
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("shared") {
            "shared" => Ok(Self::Shared),
            "branch" => Ok(Self::Branch),
            "worktree" => Ok(Self::Worktree),
            other => Err(CommandError::new(
                exit::USAGE,
                format!("unknown layer `{other}`; expected `shared`, `branch` or `worktree`"),
            )
            .into()),
        }
    }

    /// Parse a layer name recorded in `overrides.json`.
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "shared" => Some(Self::Shared),
            "branch" => Some(Self::Branch),
            "worktree" => Some(Self::Worktree),
            _ => None,
        }
    }

    /// Canonical layer name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Branch => "branch",
            Self::Worktree => "worktree",
        }
    }
}

/// One registered worktree with its precomputed identifiers.
#[derive(Debug, Clone)]
pub struct WorktreeTarget {
    pub path: PathBuf,
    pub id: String,
    pub branch_id: Option<String>,
    pub current: bool,
}

/// A resolved Breeze repository: git context, store metadata and layout.
#[derive(Debug, Clone)]
pub struct Repo {
    pub info: GitInfo,
    #[allow(dead_code)] // Exposed for later phases.
    pub store: Store,
    #[allow(dead_code)] // Exposed for later phases.
    pub meta: crate::store::RepoMetadata,
    #[allow(dead_code)] // Exposed for later phases.
    pub repo_id: String,
    pub branch_id: Option<String>,
    pub worktree_id: String,
    pub layout: RepoLayout,
}

impl Repo {
    /// Discover the repository and require an initialized store (exit 4).
    pub fn open(ctx: &Ctx) -> Result<Self> {
        let info = GitInfo::discover(ctx.repo.as_deref())?;
        let repo_id = identity::repo_id(
            info.remote.as_deref(),
            &info.main_worktree,
            &info.common_dir,
        );
        let branch_id = info.branch.as_deref().map(identity::branch_id);
        let worktree_id = identity::worktree_id(&info.root);
        let store = Store::discover()?;
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

        let meta = store.load_meta(&repo_id)?;
        Ok(Self {
            info,
            store,
            meta,
            repo_id,
            branch_id,
            worktree_id,
            layout,
        })
    }

    /// Path of the shared `info/exclude` file.
    pub fn exclude_path(&self) -> PathBuf {
        self.info.common_dir.join("info").join("exclude")
    }

    /// Directory that holds `files/` and `env` for one layer.
    pub fn layer_dir(&self, layer: Layer) -> Result<PathBuf> {
        match layer {
            Layer::Shared => Ok(self.layout.shared_dir.clone()),
            Layer::Branch => match &self.branch_id {
                Some(branch_id) => Ok(self.layout.branches_dir.join(branch_id)),
                None => Err(CommandError::new(
                    exit::USAGE,
                    "the branch layer needs a checked-out branch (HEAD is detached)",
                )
                .into()),
            },
            Layer::Worktree => Ok(self.layout.worktree_dir.clone()),
        }
    }

    /// Store path for a linked file in a layer.
    pub fn store_file(&self, layer: Layer, rel: &str) -> Result<PathBuf> {
        Ok(self.layer_dir(layer)?.join("files").join(rel))
    }

    /// Store-relative `source` string recorded in `overrides.json`.
    pub fn source_for(&self, layer: Layer, rel: &str) -> Result<String> {
        let file = self.store_file(layer, rel)?;
        Ok(relative_string(&self.layout.repo_dir, &file))
    }

    /// Env files in precedence order: shared, branch, worktree.
    pub fn env_files(&self) -> Vec<PathBuf> {
        let mut files = vec![self.layout.shared_dir.join("env")];
        if let Some(branch_id) = &self.branch_id {
            files.push(self.layout.branches_dir.join(branch_id).join("env"));
        }
        files.push(self.layout.worktree_dir.join("env"));
        files
    }

    /// Layer directories that apply to this worktree, lowest precedence first.
    pub fn current_layer_dirs(&self) -> Vec<(Layer, PathBuf)> {
        let mut dirs = vec![(Layer::Shared, self.layout.shared_dir.clone())];
        if let Some(branch_id) = &self.branch_id {
            dirs.push((Layer::Branch, self.layout.branches_dir.join(branch_id)));
        }
        dirs.push((Layer::Worktree, self.layout.worktree_dir.clone()));
        dirs
    }

    /// Every layer directory in the store, for repository-wide name checks.
    pub fn all_layer_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.layout.shared_dir.clone()];
        for base in [&self.layout.branches_dir, &self.layout.worktrees_dir] {
            let Ok(entries) = fs::read_dir(base) else {
                continue;
            };
            let mut children: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    fs::symlink_metadata(path)
                        .map(|metadata| metadata.is_dir())
                        .unwrap_or(false)
                })
                .collect();
            children.sort();
            dirs.extend(children);
        }
        dirs
    }

    /// Load the overrides index.
    pub fn overrides(&self) -> Result<OverridesFile> {
        OverridesFile::load(&self.layout.overrides_file)
    }

    /// Persist the overrides index.
    pub fn save_overrides(&self, overrides: &OverridesFile) -> Result<()> {
        overrides.save(&self.layout.overrides_file)
    }

    /// True only for a regular store file. A symlink is never accepted: overlay
    /// reads must not follow links that could point outside the store.
    pub fn is_regular_store_file(path: &Path) -> bool {
        fs::symlink_metadata(path)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
    }

    /// Write a store copy with private permissions, creating parents.
    pub fn write_store_file(&self, path: &Path, contents: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            create_private_dir(parent)?;
        }
        write_private_file(path, contents)
    }

    /// Remove empty store parents above `file`, stopping at a layer root.
    pub fn prune_empty_parents(&self, file: &Path) {
        let mut current = file.parent().map(Path::to_path_buf);

        while let Some(dir) = current {
            if !dir.starts_with(&self.layout.repo_dir) {
                break;
            }
            let is_layer_root = dir == self.layout.repo_dir
                || dir == self.layout.shared_dir
                || dir == self.layout.worktree_dir
                || dir.parent() == Some(self.layout.branches_dir.as_path());
            if is_layer_root {
                break;
            }
            match fs::remove_dir(&dir) {
                Ok(()) => current = dir.parent().map(Path::to_path_buf),
                Err(_) => break,
            }
        }
    }

    /// Back up a file into `backups/<timestamp>/` with private permissions.
    pub fn backup_file(&self, source: &Path) -> Result<PathBuf> {
        let stamp = now_rfc3339().replace(':', "-");
        let base = self
            .layout
            .backups_dir
            .join(format!("{stamp}-{}", std::process::id()));
        create_private_dir(&base)?;

        let name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        let mut dest = base.join(&name);
        let mut index = 1;
        while dest.exists() {
            dest = base.join(format!("{name}.{index}"));
            index += 1;
        }

        let contents =
            fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
        write_private_file(&dest, &contents)?;
        Ok(dest)
    }

    /// Append an operation (path and action only, never contents) to the log.
    pub fn log_event(&self, action: &str, path: &str) -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.layout.events_log)
            .with_context(|| format!("failed to open {}", self.layout.events_log.display()))?;
        writeln!(file, "{}\t{}\t{}", now_rfc3339(), action, path)
            .with_context(|| format!("failed to append to {}", self.layout.events_log.display()))
    }

    /// Registered worktrees, skipping bare entries and (optionally) missing
    /// directories.
    pub fn worktree_targets(&self, include_missing: bool) -> Vec<WorktreeTarget> {
        self.info
            .worktrees
            .iter()
            .filter(|worktree| !worktree.bare)
            .filter(|worktree| include_missing || worktree.path.exists())
            .map(|worktree| WorktreeTarget {
                path: worktree.path.clone(),
                id: identity::worktree_id(&worktree.path),
                branch_id: worktree.branch.as_deref().map(identity::branch_id),
                current: worktree.path == self.info.root,
            })
            .collect()
    }

    /// The current worktree as a target.
    pub fn current_target(&self) -> WorktreeTarget {
        WorktreeTarget {
            path: self.info.root.clone(),
            id: self.worktree_id.clone(),
            branch_id: self.branch_id.clone(),
            current: true,
        }
    }

    /// Targets a new link applies to, based on its layer.
    pub fn targets_for_layer(&self, layer: Layer) -> Result<Vec<WorktreeTarget>> {
        let targets = self.worktree_targets(false);
        Ok(match layer {
            Layer::Shared => targets,
            Layer::Branch => {
                let branch_id = self.branch_id.clone().ok_or_else(|| {
                    CommandError::new(
                        exit::USAGE,
                        "the branch layer needs a checked-out branch (HEAD is detached)",
                    )
                })?;
                targets
                    .into_iter()
                    .filter(|target| target.branch_id.as_deref() == Some(branch_id.as_str()))
                    .collect()
            }
            Layer::Worktree => targets
                .into_iter()
                .filter(|target| target.id == self.worktree_id)
                .collect(),
        })
    }
}

/// Whether an override applies to a worktree, based on its layer and source.
pub fn entry_applies(entry: &OverrideEntry, target: &WorktreeTarget) -> bool {
    match Layer::from_name(&entry.layer) {
        Some(Layer::Shared) => true,
        Some(Layer::Branch) => match source_branch_id(&entry.source) {
            Some(branch_id) => target.branch_id.as_deref() == Some(branch_id),
            None => false,
        },
        Some(Layer::Worktree) => match source_worktree_id(&entry.source) {
            Some(worktree_id) => worktree_id == target.id,
            None => false,
        },
        None => false,
    }
}

/// Highest-precedence override that applies to `target`.
pub fn best_entry<'a, I>(entries: I, target: &WorktreeTarget) -> Option<&'a OverrideEntry>
where
    I: IntoIterator<Item = &'a OverrideEntry>,
{
    entries
        .into_iter()
        .filter(|entry| entry_applies(entry, target))
        .max_by_key(|entry| Layer::from_name(&entry.layer))
}

fn source_branch_id(source: &str) -> Option<&str> {
    source
        .strip_prefix("branches/")?
        .split('/')
        .next()
        .filter(|value| !value.is_empty())
}

fn source_worktree_id(source: &str) -> Option<&str> {
    source
        .strip_prefix("worktrees/")?
        .split('/')
        .next()
        .filter(|value| !value.is_empty())
}

/// Join user input against the worktree root, rejecting `..` components.
/// True when any component of `path` is `..`.
///
/// Callers reject these before joining the path into a worktree: a `..` could
/// climb out of the repository, and canonicalization alone cannot undo that.
pub(crate) fn has_parent_dir(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn join_input(repo: &Repo, input: &Path) -> Result<PathBuf> {
    if input.as_os_str().is_empty() {
        return Err(CommandError::new(exit::USAGE, "path must not be empty").into());
    }
    if has_parent_dir(input) {
        return Err(unsafe_refused(format!(
            "path `{}` contains `..`; pass a path inside the worktree",
            input.display()
        )));
    }
    Ok(if input.is_absolute() {
        input.to_path_buf()
    } else {
        repo.info.root.join(input)
    })
}

/// Resolve a path that may not exist yet (for unlink/sync/doctor).
pub fn relative_lenient_path(repo: &Repo, input: &Path) -> Result<String> {
    let joined = join_input(repo, input)?;
    let parent = joined.parent().unwrap_or(&repo.info.root);
    let canonical_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let absolute = match joined.file_name() {
        Some(name) => canonical_parent.join(name),
        None => canonical_parent,
    };

    let relative = absolute.strip_prefix(&repo.info.root).map_err(|_| {
        unsafe_refused(format!(
            "path `{}` is outside the worktree {}",
            input.display(),
            repo.info.root.display()
        ))
    })?;
    let relative = relative_string(&repo.info.root, relative);

    if relative.is_empty() || relative == "." {
        return Err(unsafe_refused("path must name a file inside the worktree"));
    }
    Ok(relative)
}

/// Resolve and validate a file that `breeze link` may move into the store.
pub fn validate_link_source(repo: &Repo, input: &Path) -> Result<(String, PathBuf)> {
    let joined = join_input(repo, input)?;
    let relative = relative_lenient_path(repo, input)?;
    let absolute = joined;

    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CommandError::new(
                exit::USAGE,
                format!("path does not exist: {}", input.display()),
            )
            .into());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", absolute.display()));
        }
    };

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&absolute)
            .with_context(|| format!("failed to read symlink {}", absolute.display()))?;
        let resolved = if target.is_absolute() {
            target
        } else {
            absolute.parent().unwrap_or(&repo.info.root).join(target)
        };
        let resolved = resolved.canonicalize().map_err(|_| {
            unsafe_refused(format!(
                "`{}` is a dangling symlink; remove it before linking",
                absolute.display()
            ))
        })?;

        if resolved.starts_with(&repo.info.root) {
            if resolved.is_dir() {
                return Err(unsafe_refused(format!(
                    "`{}` points at a directory; only files can be linked",
                    input.display()
                )));
            }
        } else if !resolved.starts_with(store_root(repo)) {
            return Err(unsafe_refused(format!(
                "symlink `{}` points outside the worktree ({}); refusing to link",
                input.display(),
                resolved.display()
            )));
        }
    } else if metadata.is_dir() {
        return Err(unsafe_refused(format!(
            "`{}` is a directory (or submodule); only files can be linked",
            input.display()
        )));
    } else if !metadata.is_file() {
        return Err(unsafe_refused(format!(
            "`{}` is not a regular file",
            input.display()
        )));
    }

    Ok((relative, absolute))
}

/// Canonical store root, falling back to the lexical path before it exists.
pub fn store_root(repo: &Repo) -> PathBuf {
    repo.layout
        .repo_dir
        .canonicalize()
        .unwrap_or_else(|_| repo.layout.repo_dir.clone())
}

/// Whether `git ls-files --error-unmatch` finds the path.
pub fn is_tracked(root: &Path, relative: &str) -> Result<bool> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--error-unmatch", "--", relative])
        .output()
        .context("failed to run git ls-files")?;
    Ok(output.status.success())
}

/// Whether the path is currently a symlink.
pub fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// Whether `path` exists (following symlinks).
pub fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Whether `link`, a symlink, resolves to `target`.
pub fn link_points_to(link: &Path, target: &Path) -> bool {
    if !is_symlink(link) {
        return false;
    }
    match (link.canonicalize(), target.canonicalize()) {
        (Ok(link_target), Ok(expected)) => link_target == expected,
        _ => false,
    }
}

/// Compute a relative path from `from_dir` to `target`.
pub fn relative_from(from_dir: &Path, target: &Path) -> PathBuf {
    if !target.is_absolute() {
        return target.to_path_buf();
    }

    let from: Vec<Component<'_>> = from_dir.components().collect();
    let to: Vec<Component<'_>> = target.components().collect();

    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    if common == 0 {
        return target.to_path_buf();
    }

    let mut relative = PathBuf::new();
    for _ in common..from.len() {
        relative.push("..");
    }
    for component in &to[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    relative
}

/// Create a relative symlink at `link` pointing at `target`.
#[cfg(unix)]
pub fn create_relative_symlink(link: &Path, target: &Path) -> Result<()> {
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    let parent = link.parent().unwrap_or(Path::new("."));
    let parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let relative = relative_from(&parent, &target);

    std::os::unix::fs::symlink(&relative, link).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            link.display(),
            relative.display()
        )
    })
}

#[cfg(not(unix))]
pub fn create_relative_symlink(link: &Path, _target: &Path) -> Result<()> {
    anyhow::bail!(
        "symlink overlays are not supported on this platform: {}",
        link.display()
    )
}

/// Replace `destination` with a real copy of `source`, using normal modes.
pub fn restore_copy(source: &Path, destination: &Path) -> Result<()> {
    let contents =
        fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if fs::symlink_metadata(destination)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        fs::remove_file(destination)
            .with_context(|| format!("failed to remove symlink {}", destination.display()))?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options
        .open(destination)
        .with_context(|| format!("failed to write {}", destination.display()))?;
    file.write_all(&contents)
        .with_context(|| format!("failed to write {}", destination.display()))?;
    Ok(())
}

/// Make sure a link target has its parent directories in the worktree.
pub fn ensure_link_parent(link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    Ok(())
}

/// Render `path` relative to `base` with `/` separators.
pub fn relative_string(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Build an `UNSAFE_REFUSED` error.
pub fn unsafe_refused(message: impl Into<String>) -> anyhow::Error {
    CommandError::new(exit::UNSAFE_REFUSED, message).into()
}

#[cfg(test)]
pub mod testutil {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::gitinfo::GitInfo;
    use crate::store::identity;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A throwaway git repository with a second worktree and its own store.
    pub struct TempRepo {
        pub base: PathBuf,
        pub main: PathBuf,
        pub other: PathBuf,
        pub data: PathBuf,
    }

    impl TempRepo {
        pub fn new(label: &str) -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let base = std::env::temp_dir().join(format!(
                "breeze-phase1-{}-{label}-{nanos}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&base).expect("create temp dir");

            let repo = Self {
                main: base.join("main"),
                other: base.join("wt2"),
                data: base.join("data"),
                base,
            };
            fs::create_dir_all(&repo.data).expect("create data dir");
            fs::create_dir_all(&repo.main).expect("create main worktree dir");

            repo.git(&repo.main, &["init", "-b", "main"]);
            repo.git(&repo.main, &["config", "user.email", "test@example.com"]);
            repo.git(&repo.main, &["config", "user.name", "Breeze Test"]);
            repo.git(&repo.main, &["commit", "--allow-empty", "-m", "initial"]);
            repo.git(
                &repo.main,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feature",
                    repo.other
                        .to_str()
                        .expect("utf-8 worktree path")
                        .to_string()
                        .as_str(),
                ],
            );
            repo.breeze_ok(&["init"]);
            repo
        }

        pub fn git(&self, dir: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        pub fn breeze(&self, args: &[&str]) -> Output {
            self.breeze_in(&self.main, args)
        }

        pub fn breeze_in(&self, dir: &Path, args: &[&str]) -> Output {
            self.breeze_in_env(dir, args, &[])
        }

        pub fn breeze_in_env(&self, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
            let mut command = Command::new(breeze_bin());
            command
                .arg("--repo")
                .arg(dir)
                .args(args)
                .env("XDG_DATA_HOME", &self.data)
                .env("XDG_CACHE_HOME", self.base.join("cache"))
                // Tests never inherit a developer editor; `$EDITOR` tests opt in.
                .env_remove("EDITOR")
                .stdin(Stdio::null());
            for (key, value) in envs {
                command.env(key, value);
            }
            command.output().expect("run breeze")
        }

        pub fn breeze_ok_in_env(&self, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> String {
            let output = self.breeze_in_env(dir, args, envs);
            assert!(
                output.status.success(),
                "breeze {:?} exited {:?}\nstdout: {}\nstderr: {}",
                args,
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        pub fn breeze_ok_in(&self, dir: &Path, args: &[&str]) -> String {
            let output = self.breeze_in(dir, args);
            assert!(
                output.status.success(),
                "breeze {:?} exited {:?}\nstdout: {}\nstderr: {}",
                args,
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        pub fn breeze_ok(&self, args: &[&str]) -> String {
            let output = self.breeze(args);
            assert!(
                output.status.success(),
                "breeze {:?} exited {:?}\nstdout: {}\nstderr: {}",
                args,
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        pub fn breeze_err(&self, args: &[&str]) -> (i32, String) {
            let output = self.breeze(args);
            assert!(
                !output.status.success(),
                "breeze {:?} unexpectedly succeeded\nstdout: {}",
                args,
                String::from_utf8_lossy(&output.stdout)
            );
            (
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        }

        pub fn write(&self, relative: &str, contents: &str) {
            let path = self.main.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(&path, contents).expect("write file");
        }

        pub fn read(&self, root: &Path, relative: &str) -> String {
            fs::read_to_string(root.join(relative)).expect("read file")
        }

        pub fn status_clean(&self, root: &Path) -> bool {
            self.git(root, &["status", "--porcelain"]).trim().is_empty()
        }

        pub fn store_dir(&self) -> PathBuf {
            let info = GitInfo::discover(Some(&self.main)).expect("discover repo");
            let repo_id = identity::repo_id(
                info.remote.as_deref(),
                &info.main_worktree,
                &info.common_dir,
            );
            self.data.join("breeze").join("repos").join(repo_id)
        }

        pub fn branch_id(&self, dir: &Path) -> String {
            let info = GitInfo::discover(Some(dir)).expect("discover repo");
            identity::branch_id(info.branch.as_deref().expect("branch"))
        }

        pub fn worktree_id(&self, dir: &Path) -> String {
            let info = GitInfo::discover(Some(dir)).expect("discover repo");
            identity::worktree_id(&info.root)
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// Path of the `breeze` binary built alongside the test harness.
    ///
    /// CI sets `BREEZE_BIN` explicitly: newer Cargo versions can place the
    /// unit-test harness under `target/debug/build/...`, where walking up from
    /// `current_exe()` no longer lands on `target/debug/`.
    pub fn breeze_bin() -> PathBuf {
        if let Some(path) = std::env::var_os("BREEZE_BIN") {
            let binary = PathBuf::from(path);
            assert!(
                binary.is_file(),
                "breeze binary not found at {}; run `cargo build` before `cargo test`",
                binary.display()
            );
            return binary;
        }

        let mut path = std::env::current_exe().expect("current test executable");
        path.pop();
        if path.file_name().map(|name| name == "deps").unwrap_or(false) {
            path.pop();
        }
        let binary = path.join(format!("breeze{}", std::env::consts::EXE_SUFFIX));
        assert!(
            binary.is_file(),
            "breeze binary not found at {}; run `cargo build` before `cargo test`",
            binary.display()
        );
        binary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::overrides::OverrideMode;

    #[test]
    fn relative_paths_walk_up_and_down() {
        assert_eq!(
            relative_from(Path::new("/a/b/c"), Path::new("/a/x/y")),
            PathBuf::from("../../x/y")
        );
        assert_eq!(
            relative_from(Path::new("/a/b"), Path::new("/a/b/c/d")),
            PathBuf::from("c/d")
        );
    }

    #[test]
    fn layer_names_roundtrip() {
        for layer in [Layer::Shared, Layer::Branch, Layer::Worktree] {
            assert_eq!(Layer::from_name(layer.name()), Some(layer));
        }
        assert_eq!(Layer::parse(None).unwrap(), Layer::Shared);
        assert!(Layer::parse(Some("bogus")).is_err());
    }

    #[test]
    fn override_sources_encode_layer_ids() {
        let entry = OverrideEntry {
            path: "x".to_string(),
            mode: OverrideMode::Link,
            layer: "branch".to_string(),
            source: "branches/feature-1234/files/x".to_string(),
            tracked: false,
            base_sha: None,
            name: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert_eq!(source_branch_id(&entry.source), Some("feature-1234"));
        assert_eq!(source_worktree_id(&entry.source), None);
    }

    use proptest::prelude::*;

    fn path_segments() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(
            prop_oneof![
                Just("..".to_string()),
                "[a-z]{1,6}".prop_map(|segment: String| segment),
            ],
            1..6,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn parent_components_are_detected_in_any_position(segments in path_segments()) {
            let path = segments.join("/");
            let contains_parent = segments.iter().any(|segment| segment == "..");
            prop_assert_eq!(has_parent_dir(Path::new(&path)), contains_parent);
        }

        #[test]
        fn plain_relative_paths_have_no_parent_component(
            segments in prop::collection::vec("[a-z][a-z0-9]{0,6}", 1..6),
        ) {
            prop_assert!(!has_parent_dir(Path::new(&segments.join("/"))));
        }
    }

    #[test]
    fn parent_components_are_detected_in_table_variants() {
        for path in [
            "..",
            "../x",
            "a/../x",
            "a/../../x",
            "./../x",
            "deep/nested/../../../escape",
        ] {
            assert!(has_parent_dir(Path::new(path)), "{path}");
        }
        for path in ["a/b", "./a", "a/./b", ".hidden"] {
            assert!(!has_parent_dir(Path::new(path)), "{path}");
        }
    }
}
