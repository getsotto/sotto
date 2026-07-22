//! A faithful `.env` parser for `sotto import`.
//!
//! Unlike shell / dotenvy semantics, values are **not** variable-expanded and inline comments
//! are **not** stripped - a `$` or `#` inside a value is never mangled. Supported syntax:
//! - `KEY=value` and `export KEY=value`
//! - `#` comment lines and blank lines (skipped)
//! - single-quoted values (literal) and double-quoted values (interpret `\n \r \t \\ \"`)
//! - unquoted values: surrounding whitespace is trimmed (the usual `.env` convention, so a
//!   `KEY= value` line imports `value`); quote the value to preserve leading/trailing spaces.
//!
//! Multi-line values are not supported; each line is one assignment.

use crate::error::{Error, Result};

/// Parse `.env` text into `(key, value)` pairs.
pub fn parse(text: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (number, raw) in text.lines().enumerate() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line.split_once('=').ok_or_else(|| {
            Error::Input(format!(
                "invalid .env line {}: expected KEY=value",
                number + 1
            ))
        })?;
        let key = key.trim();
        if !is_valid_key(key) {
            return Err(Error::Input(format!(
                "invalid .env line {}: `{key}` is not a valid name",
                number + 1
            )));
        }
        out.push((key.to_string(), parse_value(value)));
    }
    Ok(out)
}

/// A POSIX-ish identifier: starts with a letter or `_`, then letters/digits/`_`.
fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(inner) = strip_pair(trimmed, '\'') {
        return inner.to_string();
    }
    if let Some(inner) = strip_pair(trimmed, '"') {
        return unescape_double(inner);
    }
    trimmed.to_string()
}

/// If `s` is wrapped in a matching pair of `quote`, return the inner slice.
fn strip_pair(s: &str, quote: char) -> Option<&str> {
    let bytes = s.as_bytes();
    let q = quote as u8;
    if s.len() >= 2 && bytes[0] == q && bytes[s.len() - 1] == q {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

fn unescape_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
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
    fn parses_basic_assignments_and_skips_noise() {
        let text = "\
# a comment
FOO=bar

export BAZ=qux
";
        assert_eq!(
            parse(text).unwrap(),
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn values_are_literal_no_expansion_or_comment_stripping() {
        let text = "PASS=p$$word#notacomment";
        assert_eq!(
            parse(text).unwrap(),
            vec![("PASS".to_string(), "p$$word#notacomment".to_string())]
        );
    }

    #[test]
    fn single_quotes_are_literal_double_quotes_unescape() {
        assert_eq!(parse("A='lit\\n$x'").unwrap()[0].1, "lit\\n$x".to_string());
        assert_eq!(parse("B=\"a\\nb\"").unwrap()[0].1, "a\nb".to_string());
    }

    #[test]
    fn unquoted_whitespace_trimmed_quoted_preserved() {
        // Unquoted: surrounding whitespace is trimmed (conventional .env behaviour).
        assert_eq!(parse("KEY=  value  ").unwrap()[0].1, "value".to_string());
        // Quoting preserves leading/trailing spaces.
        assert_eq!(
            parse("KEY=\"  value  \"").unwrap()[0].1,
            "  value  ".to_string()
        );
        assert_eq!(
            parse("KEY='  value  '").unwrap()[0].1,
            "  value  ".to_string()
        );
    }

    #[test]
    fn value_may_contain_equals() {
        assert_eq!(
            parse("TOKEN=abc=def==").unwrap()[0].1,
            "abc=def==".to_string()
        );
    }

    #[test]
    fn invalid_lines_error() {
        assert!(matches!(parse("NOEQUALS"), Err(Error::Input(_))));
        assert!(matches!(parse("1BAD=x"), Err(Error::Input(_))));
    }
}
