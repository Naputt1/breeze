//! Git discovery via the `git` executable.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Everything Breeze needs to know about the current repository.
#[derive(Debug, Clone)]
pub struct GitInfo {
    /// Canonical top-level directory of the current worktree.
    pub root: PathBuf,
    /// Canonical path of the shared git directory (`.git` for the main worktree).
    pub common_dir: PathBuf,
    /// Checked-out branch, or `None` when HEAD is detached.
    pub branch: Option<String>,
    /// `remote.origin.url`, when configured.
    pub remote: Option<String>,
    /// All worktrees, in the order git reports them (main worktree first).
    #[allow(dead_code)] // Used by later phases.
    pub worktrees: Vec<WorktreeInfo>,
    /// Canonical path of the main worktree.
    pub main_worktree: PathBuf,
}

/// One entry from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    #[allow(dead_code)] // Used by later phases.
    pub head: Option<String>,
    pub branch: Option<String>,
    #[allow(dead_code)] // Used by later phases.
    pub bare: bool,
    #[allow(dead_code)] // Used by later phases.
    pub detached: bool,
}

impl GitInfo {
    /// Discover repository information, starting at `start` or the current
    /// directory.
    pub fn discover(start: Option<&Path>) -> Result<Self> {
        let root = resolve_root(start)?;
        let common_dir = common_dir(&root)?;
        let branch = current_branch(&root)?;
        let remote = remote_origin_url(&root)?;
        let worktrees = list_worktrees(&root)?;
        let main_worktree = worktrees
            .first()
            .map(|worktree| worktree.path.clone())
            .unwrap_or_else(|| root.clone());

        Ok(Self {
            root,
            common_dir,
            branch,
            remote,
            worktrees,
            main_worktree,
        })
    }
}

/// Resolve the worktree root with `git rev-parse --show-toplevel`.
pub fn resolve_root(start: Option<&Path>) -> Result<PathBuf> {
    let start = match start {
        Some(path) => {
            if !path.exists() {
                bail!("repository path does not exist: {}", path.display());
            }
            if path.is_file() {
                path.parent().unwrap_or(path).to_path_buf()
            } else {
                path.to_path_buf()
            }
        }
        None => std::env::current_dir().context("failed to determine the current directory")?,
    };

    let output = git(&start, &["rev-parse", "--show-toplevel"])?
        .output()
        .context("failed to run git; is git installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "not a git repository: {}{}",
            start.display(),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(" ({})", stderr.trim())
            }
        );
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let root = text.trim();
    if root.is_empty() {
        bail!("git returned an empty repository root");
    }

    PathBuf::from(root)
        .canonicalize()
        .with_context(|| format!("failed to resolve repository root {root}"))
}

/// Canonical shared git directory.
pub fn common_dir(root: &Path) -> Result<PathBuf> {
    let output = git(root, &["rev-parse", "--git-common-dir"])?
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!(
            "failed to find the git common directory for {}",
            root.display()
        );
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let raw = text.trim();
    if raw.is_empty() {
        bail!("git returned an empty common directory");
    }

    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };

    path.canonicalize()
        .with_context(|| format!("failed to resolve git common directory {}", path.display()))
}

/// Checked-out branch, or `None` when detached.
pub fn current_branch(root: &Path) -> Result<Option<String>> {
    if let Some(branch) = git_stdout(root, &["rev-parse", "--abbrev-ref", "HEAD"])? {
        if branch != "HEAD" {
            return Ok(Some(branch));
        }
    }

    // An unborn branch (fresh `git init`, no commits) has no HEAD commit yet,
    // but its name is still available from the symbolic ref.
    Ok(git_stdout(
        root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )?)
}

/// The `origin` remote URL, when one is configured.
pub fn remote_origin_url(root: &Path) -> Result<Option<String>> {
    let output = git(root, &["config", "--get", "remote.origin.url"])?
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let url = text.trim();
    if url.is_empty() {
        Ok(None)
    } else {
        Ok(Some(url.to_string()))
    }
}

/// Parse `git worktree list --porcelain`.
pub fn list_worktrees(root: &Path) -> Result<Vec<WorktreeInfo>> {
    let output = git(root, &["worktree", "list", "--porcelain"])?
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_worktree_list(&text))
}

fn parse_worktree_list(text: &str) -> Vec<WorktreeInfo> {
    let mut worktrees: Vec<WorktreeInfo> = Vec::new();
    let mut current: Option<WorktreeInfo> = None;

    for line in text.lines() {
        if line.is_empty() {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            let path = PathBuf::from(path);
            let path = path.canonicalize().unwrap_or(path);
            current = Some(WorktreeInfo {
                path,
                head: None,
                branch: None,
                bare: false,
                detached: false,
            });
        } else if let Some(worktree) = current.as_mut() {
            if let Some(head) = line.strip_prefix("HEAD ") {
                worktree.head = Some(head.to_string());
            } else if let Some(branch) = line.strip_prefix("branch ") {
                worktree.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
            } else if line == "bare" {
                worktree.bare = true;
            } else if line == "detached" {
                worktree.detached = true;
            }
        }
    }

    if let Some(worktree) = current {
        worktrees.push(worktree);
    }

    worktrees
}

fn git(dir: &Path, args: &[&str]) -> Result<Command> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    Ok(command)
}

/// Run git and return trimmed stdout, or `None` when it fails or is empty.
fn git_stdout(dir: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = git(dir, args)?.output().context("failed to run git")?;
    if !output.status.success() {
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let value = text.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_worktree_list() {
        let text = "\
worktree /src/widget
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /src/widget-feature
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feature/login

worktree /src/detached
HEAD 3333333333333333333333333333333333333333
detached
";
        let worktrees = parse_worktree_list(text);
        assert_eq!(worktrees.len(), 3);
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert_eq!(worktrees[1].branch.as_deref(), Some("feature/login"));
        assert!(worktrees[2].detached);
        assert!(worktrees[2].branch.is_none());
        assert!(!worktrees[0].bare);
    }

    #[test]
    fn parses_bare_worktree() {
        let text = "worktree /src/bare\nHEAD 1111111111111111111111111111111111111111\nbare\n";
        let worktrees = parse_worktree_list(text);
        assert_eq!(worktrees.len(), 1);
        assert!(worktrees[0].bare);
    }

    #[test]
    fn rejects_missing_repository_path() {
        let missing = PathBuf::from("/definitely/not/a/breeze/repository/path");
        let error = resolve_root(Some(&missing)).expect_err("should reject missing path");
        assert!(error.to_string().contains("does not exist"));
    }
}
