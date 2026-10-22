use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=SOTTO_BUILD_SHA");
    let Some(sha) = env::var("SOTTO_BUILD_SHA")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    if !matches!(sha.len(), 40 | 64)
        || !sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        panic!("SOTTO_BUILD_SHA must be a full lowercase Git commit SHA");
    }
    println!("cargo:rustc-env=SOTTO_COMPILED_BUILD_SHA={sha}");
}
