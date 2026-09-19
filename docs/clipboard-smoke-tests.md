# Optional clipboard smoke tests

Clipboard integration is platform-specific, so it should remain an optional smoke
test rather than a prerequisite for the workspace suite.

When a change touches clipboard commands, run the normal checks first:

```sh
cargo fmt --all --check
cargo test --workspace
```

Then, on the target desktop environment, copy a short non-secret value into the
system clipboard and verify that the command can read and write it. Do not use real
credentials or production secrets in this check. Record the operating system and
whether the clipboard check was run in the pull request.
