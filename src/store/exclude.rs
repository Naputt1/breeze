//! The managed block inside `<common-dir>/info/exclude`.
//!
//! Breeze keeps the paths it links in a clearly delimited block so it can
//! merge idempotently without ever touching entries outside the block.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// Opening marker of the managed block.
pub const BEGIN: &str = "# >>> breeze managed >>>";
/// Closing marker of the managed block.
pub const END: &str = "# <<< breeze managed <<<";

/// Read the exclude file, treating a missing file as empty.
pub fn read(exclude_file: &Path) -> Result<String> {
    match fs::read_to_string(exclude_file) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", exclude_file.display())),
    }
}

/// Paths recorded inside the managed block, in file order.
pub fn managed_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut inside = false;

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed == BEGIN {
            inside = true;
            continue;
        }
        if trimmed == END {
            inside = false;
            continue;
        }
        if inside {
            let entry = line.trim();
            if !entry.is_empty() {
                paths.push(entry.to_string());
            }
        }
    }

    paths
}

/// Insert `path` into the managed block, keeping the block idempotent.
pub fn upsert(text: &str, path: &str) -> String {
    let mut paths = managed_paths(text);
    if !paths.iter().any(|entry| entry == path) {
        paths.push(path.to_string());
    }
    rewrite(text, &paths)
}

/// Remove `path` from the managed block.
pub fn remove(text: &str, path: &str) -> String {
    let paths: Vec<String> = managed_paths(text)
        .into_iter()
        .filter(|entry| entry != path)
        .collect();
    rewrite(text, &paths)
}

/// Add a path to the managed block on disk.
pub fn add_path(exclude_file: &Path, path: &str) -> Result<()> {
    let text = read(exclude_file)?;
    write(exclude_file, &upsert(&text, path))
}

/// Remove a path from the managed block on disk.
pub fn remove_path(exclude_file: &Path, path: &str) -> Result<()> {
    if !exclude_file.exists() {
        return Ok(());
    }
    let text = read(exclude_file)?;
    write(exclude_file, &remove(&text, path))
}

/// Managed entries that are no longer listed in `known`.
pub fn stale_paths(exclude_file: &Path, known: &HashSet<String>) -> Result<Vec<String>> {
    let text = read(exclude_file)?;
    Ok(managed_paths(&text)
        .into_iter()
        .filter(|entry| !known.contains(entry))
        .collect())
}

/// Rewrite the block in place, preserving everything outside it.
fn rewrite(text: &str, paths: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut inside = false;
    let mut replaced = false;

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed == BEGIN {
            inside = true;
            if !replaced {
                push_block(&mut lines, paths);
                replaced = true;
            }
            continue;
        }
        if trimmed == END {
            inside = false;
            continue;
        }
        if !inside {
            lines.push(line.to_string());
        }
    }

    if !replaced && !paths.is_empty() {
        let needs_blank = lines
            .last()
            .map(|line| !line.trim().is_empty())
            .unwrap_or(false);
        if needs_blank {
            lines.push(String::new());
        }
        push_block(&mut lines, paths);
    }

    while lines
        .last()
        .map(|line| line.trim().is_empty())
        .unwrap_or(false)
    {
        lines.pop();
    }

    let mut result = lines.join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    result
}

fn push_block(lines: &mut Vec<String>, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    lines.push(BEGIN.to_string());
    lines.extend(paths.iter().cloned());
    lines.push(END.to_string());
}

fn write(exclude_file: &Path, text: &str) -> Result<()> {
    if let Some(parent) = exclude_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(exclude_file, text)
        .with_context(|| format!("failed to write {}", exclude_file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_block_and_preserves_other_entries() {
        let original = "node_modules/\n*.log\n";
        let updated = upsert(original, "config/local.toml");

        assert!(updated.starts_with("node_modules/\n*.log\n"));
        assert!(updated.contains(BEGIN));
        assert!(updated.contains("config/local.toml"));
        assert!(updated.contains(END));
        assert_eq!(managed_paths(&updated), vec!["config/local.toml"]);
    }

    #[test]
    fn upsert_is_idempotent_and_ordered() {
        let once = upsert("", "a.txt");
        let twice = upsert(&once, "a.txt");
        let third = upsert(&twice, "b.txt");

        assert_eq!(once, twice);
        assert_eq!(managed_paths(&third), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn remove_drops_empty_block_only() {
        let with_block = upsert("keep-me\n", "a.txt");
        let emptied = remove(&with_block, "a.txt");

        assert_eq!(emptied, "keep-me\n");
        assert!(!emptied.contains(BEGIN));
    }

    #[test]
    fn remove_keeps_remaining_entries() {
        let text = upsert(&upsert("", "a.txt"), "b.txt");
        let updated = remove(&text, "a.txt");
        assert_eq!(managed_paths(&updated), vec!["b.txt"]);
    }

    #[test]
    fn edits_outside_the_block_are_untouched() {
        let text = format!("above\n{BEGIN}\na.txt\n{END}\nbelow\n");
        let updated = upsert(&text, "b.txt");
        assert!(updated.starts_with("above\n"));
        assert!(updated.ends_with("below\n"));
        assert_eq!(managed_paths(&updated), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn stale_entries_are_reported() {
        let text = upsert(&upsert("", "keep.txt"), "stale.txt");
        let dir = std::env::temp_dir().join(format!(
            "breeze-exclude-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("exclude");
        fs::write(&file, text).unwrap();

        let known: HashSet<String> = ["keep.txt".to_string()].into_iter().collect();
        assert_eq!(stale_paths(&file, &known).unwrap(), vec!["stale.txt"]);

        remove_path(&file, "stale.txt").unwrap();
        assert!(stale_paths(&file, &known).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let missing = Path::new("/definitely/not/a/breeze/exclude");
        assert_eq!(read(missing).unwrap(), "");
    }
}
