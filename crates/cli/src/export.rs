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
    fn dotenv_round_trips_through_parser() {
        let scenarios = [
            vec![],
            vec![
                ("EMPTY".into(), "".into()),
                ("LEADING_SPACE".into(), " leading".into()),
                ("TRAILING_SPACE".into(), "trailing ".into()),
                ("SINGLE_QUOTE".into(), "it's literal".into()),
                ("DOUBLE_QUOTE".into(), "say \"hello\"".into()),
                ("BACKSLASH".into(), r"C:\path\file".into()),
                ("DOLLAR".into(), "$HOME".into()),
                ("HASH".into(), "value#not-a-comment".into()),
                ("EQUALS".into(), "left=right==".into()),
                ("NEWLINE".into(), "line1\nline2".into()),
                ("CARRIAGE_RETURN".into(), "left\rright".into()),
                ("TAB".into(), "left\tright".into()),
                ("LITERAL_BACKSLASH_N".into(), r"line1\nline2".into()),
                ("UNICODE".into(), "café 東京".into()),
            ],
        ];

        for entries in scenarios {
            let rendered = render(ExportFormat::Dotenv, &entries);
            let parsed = crate::dotenv::parse(&rendered).expect("rendered dotenv should parse");
            assert_eq!(parsed, entries);
        }
    }

    #[cfg(unix)]
    #[test]
    fn shell_exports_round_trip_through_child_environment() {
        use std::process::Command;

        let entries = vec![
            ("PWF_EMPTY".into(), "".into()),
            ("PWF_SPACES".into(), " leading and trailing ".into()),
            (
                "PWF_QUOTES".into(),
                "single ' double \" backslash \\".into(),
            ),
            (
                "PWF_DOLLAR".into(),
                "$HOME $(printf expanded) \x60printf expanded\x60".into(),
            ),
            ("PWF_LINES".into(), "line1\nline2\rcarriage\ttab".into()),
            ("PWF_UNICODE".into(), "café 東京".into()),
            ("PWF_EQUALS_HASH".into(), "left=right#literal".into()),
        ];
        let mut script = render(ExportFormat::Shell, &entries);
        script.push_str(
            "sh -c 'printf \"%s\\0\" \"$PWF_EMPTY\" \"$PWF_SPACES\" \"$PWF_QUOTES\" \"$PWF_DOLLAR\" \"$PWF_LINES\" \"$PWF_UNICODE\" \"$PWF_EQUALS_HASH\"'\n",
        );

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("PWF_EMPTY", "inherited-value")
            .output()
            .expect("POSIX shell should run");
        assert!(
            output.status.success(),
            "shell failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let expected = entries
            .iter()
            .flat_map(|(_, value)| value.as_bytes().iter().copied().chain(std::iter::once(0)))
            .collect::<Vec<_>>();
        assert_eq!(output.stdout, expected);
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
