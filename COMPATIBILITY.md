# Sotto compatibility

This page records the current CLI release, the version expectations of Sotto's maintained
adoption paths, and the platform evidence available for them. A declared target is not necessarily
covered by a real-CLI test. The evidence column says what has actually been exercised and links to
the source record where possible.

Last checked: 18 October 2026.

## CLI release baseline

The latest published CLI is [v0.9.0](https://github.com/getsotto/sotto/releases/tag/v0.9.0),
published on 14 September 2026. The current `main` branch includes `sotto mcp`, merged in
[PR #624](https://github.com/getsotto/sotto/pull/624), but that command is not available in the
v0.9.0 release. A command present on `main` should not be assumed to exist in an installed release.

The release workflow builds packages for macOS Apple Silicon and Intel, Linux x86_64 and ARM64,
and Windows x86_64. Its build and test matrix is defined in the
[CLI release workflow](.github/workflows/release.yml). Building a release target does not by itself
prove every interactive or keychain behaviour on that operating system.

## Adoption paths

| Path | CLI version policy | Declared scope | Current evidence and limits |
| --- | --- | --- | --- |
| [Homebrew tap](https://github.com/getsotto/homebrew-tap) | Formula currently packages v0.9.0. Users receive the formula's reviewed release. | macOS 15 or newer; Apple Silicon primary, Intel best-effort and Homebrew Tier 3. | Formula and package checks cover macOS 15 and 26 on Apple Silicon and Intel. The tap remains a preview. Workflow PR creation, a controlled bot-created update PR, and the isolated Apple Silicon identity smoke remain launch gates; the scheduled updater run and Intel identity smoke are also unverified. See the [readiness record](https://github.com/getsotto/homebrew-tap/blob/main/releases/READINESS.md). |
| [VS Code extension](https://github.com/getsotto/sotto-vscode) | Requires Sotto CLI 0.9.0 or newer. | Desktop VS Code 1.85 or newer on macOS, Windows, and Linux. Remote SSH, WSL, dev containers, and VS Code for the Web are unsupported. | Hosted extension tests pass on all three desktop operating systems with a probe CLI and VS Code 1.142.0. A real CLI check on macOS stopped at the expected missing-identity error before keychain access. Real unlock prompting, terminal echo, the declared VS Code minimum, and real-CLI behaviour on Windows and Linux are unverified. The extension is a local preview, not yet published to the Marketplace; see its [platform test record](https://github.com/getsotto/sotto-vscode/blob/main/spike/RESULTS.md). |
| [GitHub Action](https://github.com/getsotto/sotto-action) | The workflow author must choose an exact published `vX.Y.Z` CLI release. The action does not choose or float to the latest CLI automatically. Its CI currently tests with CLI v0.5.0; v0.4.0 was the first release with the full target set. | Linux x86_64 and ARM64, macOS x86_64 and ARM64, and Windows x86_64. | The action test workflow exercises its install paths across the release target matrix and validates failure cases on Linux and Windows. This verifies installation behaviour for the pinned version, not every CLI runtime feature on every target. See the [action test workflow](https://github.com/getsotto/sotto-action/blob/main/.github/workflows/test.yml) and [platform documentation](https://github.com/getsotto/sotto-action#inputs-and-outputs). |
| [Example repo](https://github.com/getsotto/sotto-example) | Its Linux local-Sotto walkthrough currently pins CLI v0.6.0. This is a test pin, not a minimum supported version. | Eight language examples are checked on Linux, macOS, and Windows. | The language checks run on all three operating systems. The end-to-end local CLI walkthrough runs on Ubuntu with a disposable GNOME keyring; it does not establish local CLI or keychain behaviour on macOS or Windows. See the [example workflow](https://github.com/getsotto/sotto-example/blob/main/.github/workflows/check-examples.yml). |
| Local MCP | Requires a CLI release containing `sotto mcp`; no published release contains it yet. The current `main` branch does. | Local MCP over stdio. No particular graphical agent host is certified. The CLI's platform targets do not on their own establish MCP support on each platform. | A manual stdio-client smoke used synthetic secrets on macOS 26.5.2 arm64 against source revision `153d85ab`. It exercised separate unlock, session expiry, secret injection, and suppression of child output from the MCP result. Linux and Windows MCP smoke tests and graphical-host tests have not been run. See the [agent workflow guide](AGENT-WORKFLOWS.md) for the supported interaction and security boundary. |

## Keeping this page accurate

Update this page with each CLI release and whenever an integration changes its minimum CLI
version, declared platform scope, or verified platform evidence. Keep platform claims tied to the
integration's own tests or recorded manual evidence. Where only source on `main` has the feature,
label it as unreleased until a published CLI includes it.

This page describes compatibility evidence, not security certification. Sotto is pre-1.0 and has
not had a third-party cryptographic audit; read the [security policy](SECURITY.md) before deciding
whether it suits your use.
