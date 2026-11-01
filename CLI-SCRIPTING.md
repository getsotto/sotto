# CLI scripting and exit status

This reference describes the current CLI behaviour. The Sotto error mapping lives in
[`Error::exit_code`](crates/cli/src/error.rs); argument parsing and process execution
are handled in [`main.rs`](crates/cli/src/main.rs).

## Sotto exit codes

For commands handled by Sotto itself, use the process status to distinguish broad error
categories. Several causes share a code, so a code does not identify one exact failure.

| Code | Meaning |
| --- | --- |
| 0 | The command completed without an error. This includes help/version output and some cancelled interactive prompts; it does not necessarily mean data changed. |
| 1 | Invalid input or configuration, an identity that is already initialised, or a quota/feature-gate rejection. |
| 2 | Command-line usage or missing-argument errors. Bare `sotto` also exits with 2 in a non-interactive session and prints help to stderr. |
| 3 | No project configuration (`sotto.toml`) was found, or a requested project, environment or secret was not found. |
| 4 | The store is locked, no identity is set up, or a cryptographic operation failed. |
| 5 | Storage, I/O, OS keychain, network/transport or server errors; a forbidden action; a legacy login response incompatible with the CLI; or an unavailable optional server capability. |
| 6 | A concurrent-modification conflict. |
| 7 | Hosted Cloud eligibility is required, or the server rate-limited the request. |

Normal argument-parser usage errors exit with 2; explicitly requesting `--help` or
`--version` succeeds with 0. Commands that offer interactive selection require arguments
in scripts rather than opening a picker when prompting is unavailable.

Runtime errors returned to the CLI's main handler are printed to stderr as
`error: <diagnostic>`. Argument-parser diagnostics also use stderr for usage errors;
explicit help/version output uses stdout. Other warnings and progress messages may
appear on stderr even when the command succeeds.

Keep diagnostics available for troubleshooting errors that share a code, but do not
parse their human-readable wording as a stable machine protocol. Where a command
provides `--json`, use that for its documented structured output; it does not replace
checking the process status.

## Capture the status immediately

This POSIX shell example lists environment metadata and discards stdout. It does not
request secret values or enable plaintext output. It expects an existing Sotto project
and whatever local access the command normally requires.

```sh
if sotto env ls --json >/dev/null; then
    status=0
else
    status=$?
fi

case "$status" in
    0) printf '%s\n' 'Environment list is available.' ;;
    2) printf '%s\n' 'Check command usage and required arguments.' >&2 ;;
    3) printf '%s\n' 'Check the project directory and selected resources.' >&2 ;;
    4) printf '%s\n' 'Check identity/unlock state and the diagnostic.' >&2 ;;
    5|7) printf '%s\n' 'Inspect the diagnostic before choosing recovery.' >&2 ;;
    *) printf 'Sotto exited with status %s.\n' "$status" >&2 ;;
esac

exit "$status"
```

Capture `$?` before running another command, which would replace it. The `if` form also
allows the failure to be handled when the surrounding script uses `set -e`. Avoid
`if ! sotto ...; then status=$?; fi`: inside that branch, `$?` is the status of the
negation, not the original command.

Do not retry automatically solely because the status is 5 or 7. Code 5 includes
permission and local storage errors as well as transport failures; code 7 includes
eligibility failures as well as rate limiting. Choose recovery from the operation and
its diagnostic, with an explicit retry policy where appropriate.

## `sotto run` and the launched program

`sotto run -- <command> [args...]` first prepares the secret environment and launches
the requested program. Configuration, unlock, environment preparation and launch
failures still use Sotto's mapping above. For example, an I/O error while launching the
program maps to 5.

Once the program launches, its exit behaviour takes precedence:

- **Unix:** Sotto replaces its process with the program using `exec`. The program's
  status and signal behaviour pass through. If `exec` fails, Sotto reports the launch
  error instead. Do not assume a universal numeric encoding for signal termination;
  the observing shell or process API determines how it is represented.
- **Other platforms:** Sotto starts the program, waits for it and exits with its numeric
  status. If the process API supplies no numeric status, the current implementation
  falls back to 1. A spawn/wait I/O failure is reported as a Sotto error.

For example, in a configured and accessible project on a system with `sh`:

```sh
sotto run -- sh -c 'exit 42'
```

After a successful launch, 42 is the program's status, not an undocumented Sotto error
category. A program may also return 1-7 for its own reasons. The number alone cannot
tell you whether Sotto failed before launch or the program returned that number.
Keep this distinction when handling failures in build scripts and CI jobs.
