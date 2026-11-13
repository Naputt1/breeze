//! Minimal dotenv parsing.
//!
//! Phase 0 provides types and a basic parser; a later phase extends the syntax
//! (interpolation, inline comments, multi-line values) and the writer.

#![allow(dead_code)] // The environment command lands in a later phase.

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
}
