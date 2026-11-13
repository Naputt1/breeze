//! Stable identifiers for repositories, branches and worktrees.
//!
//! Identifiers are derived only from paths and names, so every command can
//! recompute them without consulting the store first.

use std::path::Path;

/// Lowercase `input`, replacing every run of non `[a-z0-9]` characters with a
/// single `-`, then trimming leading and trailing `-`.
pub fn sanitize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_dash = false;

    for ch in input.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            pending_dash = true;
        }
    }

    out
}

/// First 8 hex characters of the FNV-1a 64-bit hash of `input`.
pub fn short_hash(input: &str) -> String {
    let hash = fnv1a_64(input.as_bytes());
    format!("{hash:016x}")[..8].to_string()
}

pub fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Repository identity: a name plus a hash of the canonical common dir.
pub fn repo_id(remote: Option<&str>, root: &Path, common_dir: &Path) -> String {
    let base = remote
        .and_then(remote_basename)
        .or_else(|| file_name(root))
        .unwrap_or_else(|| "repo".to_string());
    let name = non_empty(sanitize(&base), "repo");
    format!("{name}-{}", short_hash(&canonical_string(common_dir)))
}

/// Branch identity: a lowercased name plus a hash of the original name.
pub fn branch_id(branch: &str) -> String {
    let name = non_empty(sanitize(branch), "branch");
    format!("{name}-{}", short_hash(branch))
}

/// Worktree identity: the directory name plus a hash of the canonical path.
pub fn worktree_id(worktree: &Path) -> String {
    let base = file_name(worktree).unwrap_or_else(|| "worktree".to_string());
    let name = non_empty(sanitize(&base), "worktree");
    format!("{name}-{}", short_hash(&canonical_string(worktree)))
}

/// Best-effort display name for a remote URL.
pub fn remote_basename(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn non_empty(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn canonical_string(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sanitizes_to_lowercase_dsl() {
        assert_eq!(sanitize("My Repo__Name!!"), "my-repo-name");
        assert_eq!(sanitize("--Foo---Bar--"), "foo-bar");
        assert_eq!(sanitize("feature/Login Form"), "feature-login-form");
        assert_eq!(sanitize("weird///name"), "weird-name");
        assert_eq!(sanitize("!!!"), "");
    }

    #[test]
    fn hashes_are_stable_and_short() {
        assert_eq!(short_hash("abc"), "e71fa219");
        assert_eq!(short_hash("abc"), short_hash("abc"));
        assert_eq!(short_hash("abc").len(), 8);
        assert_ne!(short_hash("abc"), short_hash("abd"));
    }

    #[test]
    fn parses_remote_basenames() {
        assert_eq!(
            remote_basename("https://github.com/acme/widget.git").as_deref(),
            Some("widget")
        );
        assert_eq!(
            remote_basename("git@github.com:Acme/Widget.git").as_deref(),
            Some("Widget")
        );
        assert_eq!(
            remote_basename("ssh://git@host/acme/widget/").as_deref(),
            Some("widget")
        );
        assert_eq!(remote_basename("   "), None);
    }

    #[test]
    fn builds_repo_ids() {
        let common = PathBuf::from("/src/widget/.git");
        let root = PathBuf::from("/src/widget");

        let from_remote = repo_id(Some("https://github.com/acme/widget.git"), &root, &common);
        assert!(from_remote.starts_with("widget-"));
        assert_eq!(from_remote.len(), "widget-".len() + 8);
        assert_eq!(
            from_remote,
            repo_id(Some("https://github.com/acme/widget.git"), &root, &common)
        );

        let from_root = repo_id(None, &PathBuf::from("/src/My Repo"), &common);
        assert!(from_root.starts_with("my-repo-"));
    }

    #[test]
    fn builds_branch_and_worktree_ids() {
        let branch = branch_id("feature/Login");
        assert!(branch.starts_with("feature-login-"));
        assert_eq!(branch.len(), "feature-login-".len() + 8);

        let worktree = worktree_id(Path::new("/work/trees/My Tree"));
        assert!(worktree.starts_with("my-tree-"));
        assert_eq!(worktree.len(), "my-tree-".len() + 8);
        assert_eq!(worktree, worktree_id(Path::new("/work/trees/My Tree")));
    }
}
