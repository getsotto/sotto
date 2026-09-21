use std::process::Command;

#[test]
fn invalid_share_limits_are_rejected_before_local_setup() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    for option in [
        "--views=0",
        "--views=-1",
        "--views=101",
        "--expire=0",
        "--expire=-1",
        "--expire=2592001",
    ] {
        let data_dir = scratch
            .path()
            .join(option.trim_start_matches('-').replace('=', "-"));
        let output = Command::new(env!("CARGO_BIN_EXE_sotto"))
            .current_dir(scratch.path())
            .env("SOTTO_DATA_DIR", &data_dir)
            .arg("share")
            .arg("DEMO")
            .arg(option)
            .output()
            .expect("run sotto");

        assert!(!output.status.success(), "accepted {option}");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        let (name, range) = if option.starts_with("--views") {
            ("--views", "1..=100")
        } else {
            ("--expire", "1..=2592000")
        };
        assert!(stderr.contains(name), "{option}: {stderr}");
        assert!(stderr.contains(range), "{option}: {stderr}");
        assert!(
            !data_dir.exists(),
            "{option} created {}",
            data_dir.display()
        );
    }
}
