//! The managed block inside `<common-dir>/info/attributes`.
//!
//! Filter mode registers each patched path here so Git runs the Breeze
//! clean/smudge driver for it. The block format mirrors `info/exclude`: Breeze
//! rewrites only the lines between its markers and leaves everything else
//! untouched.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// Opening marker of the managed block.
pub const BEGIN: &str = "# >>> breeze managed >>>";
/// Closing marker of the managed block.
pub const END: &str = "# <<< breeze managed <<<";

/// Filter driver names used by the attribute lines.
const DRIVER: &str = "breeze";

/// Read the attributes file, treating a missing file as empty.
pub fn read(attributes_file: &Path) -> Result<String> {
    match fs::read_to_string(attributes_file) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => {
            Err(err).with_context(|| format!("failed to read {}", attributes_file.display()))
        }
    }
}

/// Path patterns recorded inside the managed block, in file order.
pub fn managed_patterns(text: &str) -> Vec<String> {
    let mut patterns = Vec::new();
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
        if !inside {
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((pattern, _)) = line.rsplit_once(" filter=") {
            patterns.push(pattern.to_string());
        } else {
            patterns.push(line.to_string());
        }
    }

    patterns
}

/// Paths recorded inside the managed block, with attribute quoting undone.
pub fn managed_paths(text: &str) -> Vec<String> {
    managed_patterns(text)
        .into_iter()
        .map(|pattern| unquote_pattern(&pattern))
        .collect()
}

/// The attribute line for one repository-relative path.
pub fn line_for(path: &str) -> String {
    format!("{} filter={DRIVER}", pattern_for(path))
}

/// Rewrite the managed block so it contains exactly `paths`.
pub fn write_paths(attributes_file: &Path, paths: &[String]) -> Result<()> {
    let text = read(attributes_file)?;
    let lines: Vec<String> = paths.iter().map(|path| line_for(path)).collect();
    write(attributes_file, &rewrite(&text, &lines))
}

/// Insert `path` into the managed block, keeping the block idempotent.
#[cfg(test)]
pub fn upsert(text: &str, path: &str) -> String {
    let mut lines = managed_lines(text);
    let line = line_for(path);
    if !lines.iter().any(|entry| entry == &line) {
        lines.push(line);
    }
    rewrite(text, &lines)
}

/// Remove `path` from the managed block.
#[cfg(test)]
pub fn remove(text: &str, path: &str) -> String {
    let line = line_for(path);
    let lines: Vec<String> = managed_lines(text)
        .into_iter()
        .filter(|entry| entry != &line)
        .collect();
    rewrite(text, &lines)
}

#[cfg(test)]
fn managed_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
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
                lines.push(entry.to_string());
            }
        }
    }
    lines
}

/// Rewrite the block in place, preserving everything outside it.
fn rewrite(text: &str, lines: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    let mut replaced = false;

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed == BEGIN {
            inside = true;
            if !replaced {
                push_block(&mut out, lines);
                replaced = true;
            }
            continue;
        }
        if trimmed == END {
            inside = false;
            continue;
        }
        if !inside {
            out.push(line.to_string());
        }
    }

    if !replaced && !lines.is_empty() {
        let needs_blank = out
            .last()
            .map(|line| !line.trim().is_empty())
            .unwrap_or(false);
        if needs_blank {
            out.push(String::new());
        }
        push_block(&mut out, lines);
    }

    while out
        .last()
        .map(|line| line.trim().is_empty())
        .unwrap_or(false)
    {
        out.pop();
    }

    let mut result = out.join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    result
}

fn push_block(lines: &mut Vec<String>, entries: &[String]) {
    if entries.is_empty() {
        return;
    }
    lines.push(BEGIN.to_string());
    lines.extend(entries.iter().cloned());
    lines.push(END.to_string());
}

fn write(attributes_file: &Path, text: &str) -> Result<()> {
    if let Some(parent) = attributes_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(attributes_file, text)
        .with_context(|| format!("failed to write {}", attributes_file.display()))
}

/// Attribute pattern for a path: plain when safe, C-quoted otherwise.
fn pattern_for(path: &str) -> String {
    let safe = !path.is_empty()
        && !path.starts_with('#')
        && !path.starts_with('!')
        && path
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./+@".contains(ch));
    if safe {
        path.to_string()
    } else {
        let mut quoted = String::from("\"");
        for ch in path.chars() {
            match ch {
                '"' => quoted.push_str("\\\""),
                '\\' => quoted.push_str("\\\\"),
                '\n' => quoted.push_str("\\n"),
                '\t' => quoted.push_str("\\t"),
                other => quoted.push(other),
            }
        }
        quoted.push('"');
        quoted
    }
}

/// Undo the C-style quoting `pattern_for` applies.
fn unquote_pattern(pattern: &str) -> String {
    let Some(inner) = pattern
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return pattern.to_string();
    };

    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_block_and_preserves_other_entries() {
        let original = "*.log text\n";
        let updated = upsert(original, "config/local.toml");

        assert!(updated.starts_with("*.log text\n"));
        assert!(updated.contains(BEGIN));
        assert!(updated.contains("config/local.toml filter=breeze"));
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
        let with_block = upsert("keep-me text\n", "a.txt");
        let emptied = remove(&with_block, "a.txt");

        assert_eq!(emptied, "keep-me text\n");
        assert!(!emptied.contains(BEGIN));
    }

    #[test]
    fn quotes_paths_that_need_it() {
        let text = upsert("", "dir with space/file #1.txt");
        assert!(text.contains('"'), "{text}");
        assert_eq!(managed_paths(&text), vec!["dir with space/file #1.txt"]);
    }

    #[test]
    fn writes_exact_set_and_removes_block() {
        let dir = std::env::temp_dir().join(format!(
            "breeze-attributes-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("attributes");

        write_paths(&file, &["a.txt".to_string(), "b/c.txt".to_string()]).unwrap();
        assert_eq!(
            managed_patterns(&read(&file).unwrap()),
            vec!["a.txt", "b/c.txt"]
        );

        write_paths(&file, &["a.txt".to_string()]).unwrap();
        assert_eq!(managed_patterns(&read(&file).unwrap()), vec!["a.txt"]);

        write_paths(&file, &[]).unwrap();
        assert!(managed_patterns(&read(&file).unwrap()).is_empty());
        assert!(!read(&file).unwrap().contains(BEGIN));

        let _ = fs::remove_dir_all(&dir);
    }
}
