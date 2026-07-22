//! Plaintext export formats for `sotto export`.
//!
//! These are pure, deterministic renderers over `(name, value)` pairs (the caller decides whether
//! exporting plaintext is appropriate and where it goes). All take UTF-8 values.

use clap::ValueEnum;

/// Output format for `sotto export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    /// `KEY="value"` lines (a `.env` file).
    Dotenv,
    /// `export KEY='value'` lines for `eval`.
    Shell,
    /// A JSON object of name → value.
    Json,
}

/// Render `entries` in the chosen format.
pub fn render(format: ExportFormat, entries: &[(String, String)]) -> String {
    match format {
        ExportFormat::Dotenv => dotenv(entries),
        ExportFormat::Shell => shell(entries),
        ExportFormat::Json => json(entries),
    }
}

fn dotenv(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    for (key, value) in entries {
        out.push_str(key);
        out.push('=');
        out.push_str(&dotenv_quote(value));
        out.push('\n');
    }
    out
}

/// Always double-quote and escape, so values with spaces/quotes/newlines round-trip.
fn dotenv_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn shell(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    for (key, value) in entries {
        out.push_str("export ");
        out.push_str(key);
        out.push('=');
        out.push_str(&shell_quote(value));
        out.push('\n');
    }
    out
}

/// POSIX single-quote escaping: wrap in `'…'`, replacing each `'` with `'\''`.
fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn json(entries: &[(String, String)]) -> String {
    // BTreeMap → deterministic key order; serde_json handles all escaping.
    let map: std::collections::BTreeMap<&str, &str> = entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    serde_json::to_string_pretty(&map).expect("serialising a string map cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<(String, String)> {
        vec![
            ("DATABASE_URL".into(), "postgres://localhost".into()),
            ("QUOTE".into(), "a\"b'c".into()),
            ("MULTILINE".into(), "line1\nline2".into()),
        ]
    }

    #[test]
    fn dotenv_quotes_and_escapes() {
        let out = render(ExportFormat::Dotenv, &entries());
        assert!(out.contains("DATABASE_URL=\"postgres://localhost\"\n"));
        assert!(out.contains("QUOTE=\"a\\\"b'c\"\n"));
        assert!(out.contains("MULTILINE=\"line1\\nline2\"\n"));
    }

    #[test]
    fn shell_uses_posix_single_quoting() {
        let out = render(ExportFormat::Shell, &[("Q".into(), "a'b".into())]);
        // a'b  ->  'a'\''b'
        assert_eq!(out, "export Q='a'\\''b'\n");
    }

    #[test]
    fn json_is_valid_and_sorted() {
        let out = render(ExportFormat::Json, &entries());
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["DATABASE_URL"], "postgres://localhost");
        assert_eq!(parsed["MULTILINE"], "line1\nline2");
        // BTreeMap ordering: DATABASE_URL before MULTILINE before QUOTE
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["DATABASE_URL", "MULTILINE", "QUOTE"]);
    }

    #[test]
    fn empty_renders_empty() {
        assert_eq!(render(ExportFormat::Dotenv, &[]), "");
        assert_eq!(render(ExportFormat::Shell, &[]), "");
        assert_eq!(render(ExportFormat::Json, &[]), "{}");
    }
}
