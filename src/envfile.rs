//! Minimal dotenv parsing, merging and writing.
//!
//! The parser deliberately stays small: blank lines and `#` comments are
//! skipped, `export KEY=VALUE` is accepted, values may be bare, single-quoted
//! or double-quoted, and double-quoted values support the usual escapes.
//! Nothing is interpolated.

#![allow(dead_code)] // `exported` is parsed for completeness; commands use keys/values.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// One `KEY=VALUE` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvEntry {
    pub key: String,
    pub value: String,
    pub exported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvFileError {
    #[error("line {line}: expected KEY=VALUE")]
    MissingEquals { line: usize },

    #[error("line {line}: empty variable name")]
    EmptyKey { line: usize },

    #[error("line {line}: unterminated quoted value")]
    UnterminatedQuote { line: usize },
}

/// Parse dotenv text into entries.
///
/// Blank lines and `#` comments are skipped. `export KEY=VALUE` is accepted.
/// Values may be bare, single-quoted or double-quoted; double-quoted values
/// support `\n`, `\r`, `\t`, `\\` and `\"` escapes.
pub fn parse(input: &str) -> Result<Vec<EnvEntry>, EnvFileError> {
    let mut entries = Vec::new();

    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (exported, body) = match line.strip_prefix("export ") {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };

        let Some((raw_key, raw_value)) = body.split_once('=') else {
            return Err(EnvFileError::MissingEquals { line: line_number });
        };

        let key = raw_key.trim();
        if key.is_empty() {
            return Err(EnvFileError::EmptyKey { line: line_number });
        }
        if key.chars().any(char::is_whitespace) {
            return Err(EnvFileError::EmptyKey { line: line_number });
        }

        let value = parse_value(raw_value.trim(), line_number)?;
        entries.push(EnvEntry {
            key: key.to_string(),
            value,
            exported,
        });
    }

    Ok(entries)
}

fn parse_value(value: &str, line: usize) -> Result<String, EnvFileError> {
    if let Some(inner) = value.strip_prefix('"') {
        let Some(inner) = inner.strip_suffix('"') else {
            return Err(EnvFileError::UnterminatedQuote { line });
        };
        return Ok(unescape_double_quoted(inner));
    }

    if let Some(inner) = value.strip_prefix('\'') {
        let Some(inner) = inner.strip_suffix('\'') else {
            return Err(EnvFileError::UnterminatedQuote { line });
        };
        return Ok(inner.to_string());
    }

    Ok(value.to_string())
}

fn unescape_double_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
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

/// Read and merge env files in order; later files win.
pub fn load_merged(paths: &[PathBuf]) -> Result<BTreeMap<String, String>> {
    let mut merged = BTreeMap::new();

    for path in paths {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };

        let entries = parse(&text).map_err(|err| anyhow!("{}: {err}", path.display()))?;
        for entry in entries {
            merged.insert(entry.key, entry.value);
        }
    }

    Ok(merged)
}

/// Render entries as plain dotenv lines.
pub fn format_dotenv(values: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in values {
        out.push_str(key);
        out.push('=');
        out.push_str(&dotenv_value(value));
        out.push('\n');
    }
    out
}

/// Render entries as shell `export` statements.
pub fn format_shell(values: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in values {
        out.push_str("export ");
        out.push_str(key);
        out.push('=');
        out.push_str(&format!("'{}'", value.replace('\'', "'\\''")));
        out.push('\n');
    }
    out
}

/// Render entries as a JSON object.
pub fn format_json(values: &BTreeMap<String, String>) -> Result<String> {
    let mut map = serde_json::Map::new();
    for (key, value) in values {
        map.insert(key.clone(), serde_json::Value::String(value.clone()));
    }
    let mut out = serde_json::to_string_pretty(&serde_json::Value::Object(map))?;
    out.push('\n');
    Ok(out)
}

/// Write `contents` atomically (temp file in the same directory, then rename).
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "env".to_string());
    let temp = parent.join(format!(".{name}.tmp-{}", std::process::id()));

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(&temp)
        .with_context(|| format!("failed to open {}", temp.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write {}", temp.display()))?;
    let _ = file.sync_all();
    drop(file);

    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(err).with_context(|| format!("failed to rename into {}", path.display()));
    }
    Ok(())
}

fn dotenv_value(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./:@%+,=*".contains(ch));
    if safe {
        value.to_string()
    } else {
        quote_double(value)
    }
}

fn quote_double(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_dotenv() {
        let entries = parse(
            "\
# a comment
FOO=bar
export BAZ=qux

EMPTY=
",
        )
        .expect("parse");

        assert_eq!(
            entries,
            vec![
                EnvEntry {
                    key: "FOO".to_string(),
                    value: "bar".to_string(),
                    exported: false,
                },
                EnvEntry {
                    key: "BAZ".to_string(),
                    value: "qux".to_string(),
                    exported: true,
                },
                EnvEntry {
                    key: "EMPTY".to_string(),
                    value: String::new(),
                    exported: false,
                },
            ]
        );
    }

    #[test]
    fn parses_quoted_values() {
        let entries =
            parse("A=\"line\\nbreak\"\nB='single $value'\nC=\"say \\\"hi\\\"\"\n").expect("parse");

        assert_eq!(entries[0].value, "line\nbreak");
        assert_eq!(entries[1].value, "single $value");
        assert_eq!(entries[2].value, "say \"hi\"");
    }

    #[test]
    fn values_with_hashes_are_not_comments() {
        let entries = parse("A=value#not-a-comment\nB='# hash'\n").expect("parse");
        assert_eq!(entries[0].value, "value#not-a-comment");
        assert_eq!(entries[1].value, "# hash");
    }

    #[test]
    fn keys_and_export_prefix_are_trimmed() {
        let entries = parse("  KEY  =  value  \nexport OTHER=x\n").expect("parse");
        assert_eq!(entries[0].key, "KEY");
        assert_eq!(entries[0].value, "value");
        assert!(entries[1].exported);
        assert_eq!(entries[1].key, "OTHER");
    }

    #[test]
    fn reports_invalid_lines() {
        assert_eq!(
            parse("NOPE\n"),
            Err(EnvFileError::MissingEquals { line: 1 })
        );
        assert_eq!(parse("=value\n"), Err(EnvFileError::EmptyKey { line: 1 }));
        assert_eq!(
            parse("A=\"unterminated\n"),
            Err(EnvFileError::UnterminatedQuote { line: 1 })
        );
    }

    #[test]
    fn merges_layers_with_later_files_winning() {
        let dir = test_dir("merge");
        let shared = dir.join("shared.env");
        let branch = dir.join("branch.env");
        fs::write(&shared, "A=shared\nB=shared\nC=shared\n").unwrap();
        fs::write(&branch, "B=branch\nC=branch\nD=branch\n").unwrap();

        let merged = load_merged(&[shared, branch, dir.join("missing.env")]).expect("merge");
        assert_eq!(
            merged,
            [
                ("A".to_string(), "shared".to_string()),
                ("B".to_string(), "branch".to_string()),
                ("C".to_string(), "branch".to_string()),
                ("D".to_string(), "branch".to_string()),
            ]
            .into_iter()
            .collect()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn formats_dotenv_shell_and_json() {
        let values: BTreeMap<String, String> = [
            ("SIMPLE".to_string(), "one".to_string()),
            ("SPACED".to_string(), "two words".to_string()),
            ("SECRET".to_string(), "***".to_string()),
            ("QUOTED".to_string(), "it's here".to_string()),
        ]
        .into_iter()
        .collect();

        let dotenv = format_dotenv(&values);
        assert!(dotenv.contains("SIMPLE=one\n"));
        assert!(dotenv.contains("SPACED=\"two words\"\n"));
        assert!(dotenv.contains("SECRET=***\n"));

        let shell = format_shell(&values);
        assert!(shell.contains("export SIMPLE='one'\n"));
        assert!(shell.contains("export QUOTED='it'\\''s here'\n"));

        let json = format_json(&values).expect("json");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");
        assert_eq!(parsed["SPACED"], "two words");
        assert_eq!(parsed["SECRET"], "***");
    }

    #[test]
    fn dotenv_roundtrips_quoted_values() {
        let values: BTreeMap<String, String> = [
            ("NEWLINE".to_string(), "a\nb".to_string()),
            ("HASH".to_string(), "x#y".to_string()),
        ]
        .into_iter()
        .collect();

        let rendered = format_dotenv(&values);
        let parsed: BTreeMap<String, String> = parse(&rendered)
            .expect("parse")
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect();
        assert_eq!(parsed, values);
    }

    #[test]
    fn write_atomic_replaces_file_with_private_mode() {
        let dir = test_dir("atomic");
        let path = dir.join(".env.local");
        write_atomic(&path, "A=1\n").expect("write");
        assert_eq!(fs::read_to_string(&path).unwrap(), "A=1\n");

        write_atomic(&path, "A=2\n").expect("rewrite");
        assert_eq!(fs::read_to_string(&path).unwrap(), "A=2\n");

        if cfg!(unix) {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "breeze-envfile-{}-{label}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
