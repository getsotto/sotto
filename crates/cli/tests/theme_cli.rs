//! Behavioural tests for the `theme` commands, run through the real binary.
//!
//! The theme commands touch neither the store nor the keychain, so pointing `SOTTO_DATA_DIR`
//! at a fresh temp dir per test gives full isolation. Output is always piped here, which means
//! styling is off and every assertion reads plain text.

use std::path::Path;
use std::process::{Command, Output};

/// A `sotto` invocation hermetic against the developer's shell: an isolated data dir, with no
/// ambient theme/style/token overrides leaking in.
fn sotto(data_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sotto"));
    cmd.env("SOTTO_DATA_DIR", data_dir)
        .env_remove("SOTTO_THEME")
        .env_remove("SOTTO_TOKEN")
        .env_remove("NO_COLOR")
        .env_remove("CI");
    cmd
}

fn run(data_dir: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut cmd = sotto(data_dir);
    cmd.args(args);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.output().expect("run sotto")
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).unwrap()
}

#[test]
fn ls_lists_presets_and_marks_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["theme", "ls"], &[]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    let lines: Vec<&str> = stdout(&out).lines().collect();
    assert_eq!(lines.len(), 5, "expected five presets: {lines:?}");
    // Piped output is unstyled, so the current theme keeps the plain `*` marker.
    assert!(lines[0].starts_with("* nord"), "{:?}", lines[0]);
    for (line, name) in lines[1..]
        .iter()
        .zip(["sordino", "terminal", "monochrome", "tokyo-night"])
    {
        assert!(line.starts_with(&format!("  {name}")), "{line:?}");
    }
    assert!(
        !stdout(&out).contains('\x1b'),
        "piped output must carry no escape codes"
    );

    // --plain changes nothing when output is already piped, but must still work.
    let plain = run(dir.path(), &["--plain", "theme", "ls"], &[]);
    assert!(plain.status.success(), "stderr: {}", stderr(&plain));
    assert_eq!(stdout(&plain), stdout(&out));
}

#[test]
fn bare_theme_lists_like_ls() {
    let dir = tempfile::tempdir().unwrap();
    let bare = run(dir.path(), &["theme"], &[]);
    let ls = run(dir.path(), &["theme", "ls"], &[]);
    assert!(bare.status.success(), "stderr: {}", stderr(&bare));
    assert_eq!(stdout(&bare), stdout(&ls));
}

#[test]
fn set_persists_and_current_reports() {
    let dir = tempfile::tempdir().unwrap();
    let set = run(dir.path(), &["theme", "set", "sordino"], &[]);
    assert!(set.status.success(), "stderr: {}", stderr(&set));
    assert!(
        stderr(&set).contains("theme set to sordino"),
        "stderr: {}",
        stderr(&set)
    );

    let current = run(dir.path(), &["theme", "current"], &[]);
    assert_eq!(stdout(&current), "sordino\n");

    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("theme = \"sordino\""), "{config}");

    let ls = run(dir.path(), &["theme", "ls"], &[]);
    let marked = stdout(&ls)
        .lines()
        .find(|line| line.starts_with('*'))
        .expect("one marked theme");
    assert!(marked.contains("sordino"), "{marked:?}");
}

#[test]
fn set_rejects_unknown_theme() {
    let dir = tempfile::tempdir().unwrap();
    let set = run(dir.path(), &["theme", "set", "nope"], &[]);
    assert!(!set.status.success());
    assert!(
        stderr(&set).contains("unknown theme `nope`"),
        "stderr: {}",
        stderr(&set)
    );

    // The refusal saves nothing: the default still resolves.
    let current = run(dir.path(), &["theme", "current"], &[]);
    assert_eq!(stdout(&current), "nord\n");
}

#[test]
fn env_beats_saved_and_flag_beats_env() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path(), &["theme", "set", "sordino"], &[])
        .status
        .success());

    let out = run(
        dir.path(),
        &["theme", "current"],
        &[("SOTTO_THEME", "tokyo-night")],
    );
    assert_eq!(stdout(&out), "tokyo-night\n");

    let out = run(
        dir.path(),
        &["--theme", "monochrome", "theme", "current"],
        &[("SOTTO_THEME", "tokyo-night")],
    );
    assert_eq!(stdout(&out), "monochrome\n");
}

#[test]
fn unknown_requested_name_warns_and_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    for extra_env in [&[][..], &[("SOTTO_THEME", "bogus")][..]] {
        let args: Vec<&str> = if extra_env.is_empty() {
            vec!["--theme", "bogus", "theme", "current"]
        } else {
            vec!["theme", "current"]
        };
        let out = run(dir.path(), &args, extra_env);
        assert!(out.status.success(), "stderr: {}", stderr(&out));
        assert_eq!(stdout(&out), "nord\n");
        assert!(
            stderr(&out).contains("unknown theme `bogus`"),
            "stderr: {}",
            stderr(&out)
        );
    }
}

#[test]
fn custom_theme_file_is_listed_and_selectable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("themes")).unwrap();
    // No `name` field: the file stem becomes the theme name.
    std::fs::write(
        dir.path().join("themes/synth.toml"),
        "bg = \"#120024\"\n\
         fg = \"#ffffff\"\n\
         accent = \"#ff007f\"\n\
         success = \"#00ff66\"\n\
         warning = \"#ffaa00\"\n\
         error = \"#ff0033\"\n\
         muted = \"#775588\"\n\
         border = \"#331144\"\n",
    )
    .unwrap();

    let ls = run(dir.path(), &["theme", "ls"], &[]);
    assert!(
        stdout(&ls).lines().any(|line| line.contains("synth")),
        "stdout: {}",
        stdout(&ls)
    );

    let set = run(dir.path(), &["theme", "set", "synth"], &[]);
    assert!(set.status.success(), "stderr: {}", stderr(&set));
    let current = run(dir.path(), &["theme", "current"], &[]);
    assert_eq!(stdout(&current), "synth\n");
}
