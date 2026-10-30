[English](README.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português (BR)](README.pt-BR.md)

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="web/public/mark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="web/public/mark-light.svg">
    <img src="web/public/mark-dark.svg" alt="Sotto" width="64" height="64">
  </picture>
</p>

# Sotto

End-to-end encrypted secret sync for developer teams. Stop Slacking your `.env`.

> [!WARNING]
> Sotto is pre-1.0 and has **not** had a third-party cryptographic audit. It works end to end, but
> should not yet be trusted with critical production secrets. See [SECURITY.md](SECURITY.md).

Sotto is built around one Rust crypto implementation shared by the native CLI and the browser
client through WebAssembly. The server stores and synchronises encrypted data without ever
receiving plaintext secrets or usable keys.

## Ways to run Sotto

Sotto is one open-source product that you can run three ways. Whichever you choose, the server
only ever stores ciphertext.

| Mode | What you get |
| --- | --- |
| Local | The CLI and an encrypted vault on your own machine. No account and no server. |
| Self-hosted | The same Apache-2.0 server on infrastructure you operate. It applies no plan limits and no hosted-service checks; see [Self-hosting](#self-hosting). |
| Sotto Cloud | The server Sotto operates at [getsotto.co.uk](https://getsotto.co.uk), for when you would rather not run one. `sotto login` uses it by default. Plans and limits are on the [pricing page](https://getsotto.co.uk/#pricing), and service status is at [status.getsotto.co.uk](https://status.getsotto.co.uk). |

You can start locally and add a server later: after `sotto login`, `sotto push` uploads the active
environment as it stands.

## Current status

The end-to-end flow works: encrypt locally, sync ciphertext, decrypt on another device or in the
browser, and share a single secret via a one-time link. Teams work end to end too: organisations
with roles, per-member environment grants, key rotation on member removal, machine tokens for CI,
an organisation audit log, and an account reset for a lost Emergency Kit.

| Component | Available now |
| --- | --- |
| Crypto core | KDF, XChaCha20-Poly1305 AEAD + AAD, key wrapping, X25519 sealed-box grants, the environment vault hierarchy, data-key rewrap (rotation), share-link crypto, and key encoding - with native↔WASM golden vectors, codec fuzzing, and Kani proofs for envelope-header and key-symbol parsing |
| CLI | Local secrets (`init`, `set`, `get`, `ls`, `rm`, `import`, `export`, `run`), environments (`env ls/use/diff/copy`), an interactive dashboard, `login`/`push`/`pull` sync, new-device `setup`, `history` and `rollback`, one-time links (`share`, `shares ls/revoke`); teams: `org create/ls/invite/members/remove/plan/audit`, `grant`, `clone`, `rotate`, expiring machine `token create/ls/revoke` (with `SOTTO_TOKEN` mode for CI); `status`, `cloud status`, lost-kit `reset`, themes, and shell completions |
| Server | OAuth login + sessions, account + snapshot sync (versioned writes, ETag), orgs + memberships + roles, per-member vault-key grants, transactional key rotation, expiring machine tokens, secret history, an organisation audit log, opt-in organisation deletion with a recovery window, account reset, and one-time links - ciphertext only. A Sotto Cloud deployment also enforces plan limits and can bill through Stripe |
| Web | A landing page and guides; the vault at `/app`, which unlocks in the browser to browse, search, reveal, and copy secrets, create one-time links, share an environment with a member, and rotate an environment key; a team panel with organisations, members, email invites, the plan, the audit log, and organisation deletion; the one-time link recipient page; and a Sotto Cloud account page at `/cloud` |

## Install

Prebuilt, signed binaries for macOS (Apple Silicon + Intel), Linux (x86_64 + ARM64), and
Windows x86_64:

```sh
curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/getsotto/sotto/main/install.ps1 | iex
```

The installer verifies the archive's SHA-256 checksum - and its Sigstore signature, when `cosign`
is installed - before installing (`~/.local/bin` on macOS/Linux, `%LOCALAPPDATA%\sotto\bin` on
Windows). Prefer to look first? Grab an archive from the
[releases page](https://github.com/getsotto/sotto/releases) and verify it manually per
[SECURITY.md](SECURITY.md), or build from source (see [Developing](#developing)).

The installers, the Homebrew formula and the GitHub Action all install published releases, while
this README follows the `main` branch, which can be ahead of the latest release. A command described
here may therefore not exist in your installed version yet; `sotto --help` lists the commands your
version supports.

See [COMPATIBILITY.md](COMPATIBILITY.md) for integration version requirements, platform claims,
and the evidence currently available for them.

### Homebrew (preview)

The [Sotto tap](https://github.com/getsotto/homebrew-tap) provides another
installation route for macOS. The formula packages reviewed CLI releases and
requires macOS 15 or later. Apple Silicon is the primary platform; Intel is
best-effort and Homebrew Tier 3, with Homebrew planning to end Intel support
in or after September 2027. Check the tap's
[support and installation guidance](https://github.com/getsotto/homebrew-tap#readme)
and [release-readiness record](https://github.com/getsotto/homebrew-tap/blob/main/releases/READINESS.md)
before using it. Avoid this preview for critical production secrets.

Install and upgrade the formula with:

```sh
brew install getsotto/tap/sotto
```

```sh
brew update
brew upgrade getsotto/tap/sotto
```

If another `sotto` installation is on your `PATH`, inspect the resolved
executables and the Homebrew-managed binary before choosing which one to use:

```sh
which -a sotto
brew list --versions sotto
"$(brew --prefix)/bin/sotto" --version
```

To remove only the Homebrew formula, run:

```sh
brew uninstall getsotto/tap/sotto
```

Uninstalling the formula leaves project files, local Sotto data, Keychain
entries, and other `sotto` installations in place. Confirm the selected binary
and preserve your Emergency Kit before removing or switching an existing
installation.

### Shell completions

`sotto completions <shell>` prints a completion script to stdout. For example:

```sh
# bash
mkdir -p ~/.local/share/bash-completion/completions
sotto completions bash > ~/.local/share/bash-completion/completions/sotto

# zsh (add the directory to fpath in ~/.zshrc first)
mkdir -p ~/.zfunc
sotto completions zsh > ~/.zfunc/_sotto

# fish
mkdir -p ~/.config/fish/completions
sotto completions fish > ~/.config/fish/completions/sotto.fish
```

Release tarballs and zips also contain the matching `completions/sotto.*` files if you prefer to
copy them directly.

### GitHub Actions

For GitHub Actions, use the [Sotto Setup action](https://github.com/getsotto/sotto-action) to
install an exact CLI release and verify its checksum and Sigstore bundles before making `sotto`
available to later steps:

```yaml
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0
      - uses: getsotto/sotto-action@v1.1.0
        with:
          sotto-version: v0.9.0
      - run: sotto run -- npm test
        env:
          SOTTO_SERVER: ${{ vars.SOTTO_SERVER }}
          SOTTO_TOKEN: ${{ secrets.SOTTO_TOKEN }}
```

The action ref and `sotto-version` are independent. Keep `sotto-version` as an exact `vX.Y.Z`
release. Set the `SOTTO_SERVER` repository variable to your server's URL, including
`https://getsotto.co.uk` for Sotto Cloud: with `SOTTO_TOKEN` set, `sotto run` and `sotto export`
never fall back to a default server, so the token only goes to a server you named. See the
[action documentation](https://github.com/getsotto/sotto-action#readme) for matrix, Windows, and
reusable-workflow examples.

## Quick start

Want a runnable demo? [**sotto-example**](https://github.com/getsotto/sotto-example)
walks through local secret injection with a Python GIF and copyable steps, plus tiny examples
in JavaScript, TypeScript, Java, C#, PHP, Go, Rust, and C++. No account required; instructions cover
macOS, Linux, and Windows.

```sh
sotto init                   # create your identity + first project - SAVE the printed Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted locally before it ever touches disk
sotto import .env            # optional: pull in an existing file, still encrypted locally
sotto run -- npm start       # inject the environment's secrets into any command
sotto get DATABASE_URL -c    # copy a secret without printing it; clipboard clears after 45s when unchanged
sotto export --format dotenv --reveal   # print a .env; refuses a terminal without --reveal
```

`sotto init` creates `dev`, `staging` and `prod` environments and selects `dev`. Export writes
plaintext, so it needs `--reveal` on a terminal, just like `sotto get`; `--format` also takes
`shell` and `json`. Clipboard clearing is best-effort: replacing the clipboard protects the newer
content, while clipboard managers, suspension, or a terminated helper may retain a history copy.

Everything so far stays on your machine. To sync another device, work with a team or run CI, log
in to a server:

```sh
sotto login                  # Sotto Cloud by default; --server <url> for your own server
sotto push                   # upload the active environment's ciphertext
sotto share DATABASE_URL     # a one-time link whose key never reaches the server
```

`sotto login` opens a browser to sign in with GitHub. It uses Sotto Cloud at
[getsotto.co.uk](https://getsotto.co.uk) unless you pass `--server <url>` (see
[Self-hosting](#self-hosting) to run your own), and later commands reuse the server you chose.
Either way the server only ever stores ciphertext - the web vault at `/app` on the same server
decrypts in your browser, with keys that never leave your devices.

To browse secrets interactively, see the [dashboard guide](#interactive-dashboard).

## Agent workflows

Local coding agents can discover and run project-declared tasks through Sotto's local MCP server.
Read the [agent task guide](AGENT-WORKFLOWS.md) for setup, approval, and the security boundary.

## Using Sotto

### Another device

```sh
sotto login                  # same account as the first machine
sotto setup                  # unpack the Emergency Kit onto this device
sotto pull                   # download the ciphertext you already pushed
```

You need the Emergency Kit printed by `sotto init`; without it, a new device cannot decrypt the vault.

If you lose the Emergency Kit, `sotto reset` gives your account fresh keys. It is a reset, not a
recovery: everything encrypted under the old keys becomes permanently unreadable, and organisation
admins must grant your shared environments again. If a device can still unlock the vault, export
what you need from it first.

### Environments

Use `--env` to select an environment for one command without changing the project's default:

```sh
sotto run --env staging -- npm test
sotto ls --env staging
```

`--env` lasts for that command only; `sotto env use` changes the default.

List environments, switch the default, compare two, or preview a copy between them:

```sh
sotto env ls                          # list environments; the active one is marked
sotto env use staging                 # switch the project's active environment
sotto env diff dev staging            # compare keys; values stay hidden without --reveal
sotto env copy dev staging            # preview additions and updates without writing
sotto env copy dev staging --confirm  # apply the copy; never delete destination keys
```

`env diff` marks each key `=` (same value), `!` (different values), `<` (only in the first
environment) or `>` (only in the second). Without `--confirm`, copying is a dry-run. A confirmed
copy adds and updates secrets but leaves keys that exist only in the destination untouched.

### Import a dotenv file

`sotto import` reads a dotenv file and sets each assignment in one environment, encrypting every
value locally. It writes to the active environment unless you name another with `--env`:

```sh
sotto import .env                         # into the active environment
sotto import --env staging .env.staging   # into staging; the default environment is unchanged
```

An import merges into what is already there. Suppose `staging` already holds `API_URL` and `REGION`,
and `.env.staging` contains:

```dotenv
API_URL=https://staging.example.test
FEATURE_FLAG=on
```

After importing it into `staging`:

- `API_URL` takes the value from the file, because an existing name is updated.
- `FEATURE_FLAG` is added, because a new name is created.
- `REGION` is kept, because a name that is missing from the file is never removed.

Import writes to your local store only. After `sotto login`, upload the result as a separate step:

```sh
sotto push --env staging
```

The dotenv file is only read: import does not change, consume or delete it, so protect or remove it
yourself if it holds real values. A file that does not parse changes nothing, but a failure while
writing does not undo the assignments already stored, so check `sotto ls --env staging` after an
interrupted import.

To get an earlier value back, see [Restore an earlier secret version](#restore-an-earlier-secret-version).
That history comes from the server, so it only helps for values you had already pushed. Run
`sotto import --help` for the quoting and literal-value rules instead of repeating them here.

### Remove a secret

To remove an obsolete secret from `staging` without changing your default environment:

```sh
sotto --env staging rm DEMO_OBSOLETE_KEY
sotto --env staging push
sotto --env staging pull
```

`sotto rm` removes the secret from the selected environment in your local vault.
`sotto push` synchronises the removal with the server after `sotto login`; run
`sotto pull` for that same environment on another device to receive the removal.
Run the final command on the other device, not the device that made the change.

In an interactive terminal, `sotto rm` without a name opens the secret picker.
Removal then asks for confirmation, with No as the default; cancelling or declining
does not remove anything. `--yes` (or `-y`) skips confirmation but never skips
the picker when the name is omitted or a required unlock prompt.

Interactive prompts require stdin, stdout and stderr to be terminals, a non-dumb
`TERM`, and `CI` not being active. In non-interactive use, the name is required;
a named removal proceeds without confirmation even if `--yes` is absent.

Removed secrets disappear from ordinary reads and listings, but their version
history remains. This is not secure erasure and does not revoke a credential at
its external provider. Use `sotto history` and `sotto rollback` for recovery.

### Share a secret

`sotto share` encrypts one secret into a one-time link. The key sits after the `#` in the link,
which browsers never send to a server, so the server only ever holds ciphertext. Creating a link
needs `sotto login`; the recipient needs no account. Opening the link does not use a view, and
revealing the secret does.

```sh
sotto share DATABASE_URL                           # choose view and lifetime limits in an interactive terminal
sotto share DATABASE_URL --views 3 --expire 3600   # skip both limit prompts
sotto share DATABASE_URL --passphrase              # also require a passphrase; send it separately
```

In an interactive terminal, each omitted share limit opens its own menu. The view menu offers
1, 2, 3, 5 or 10 views and a custom value; the lifetime menu offers no expiry, 1 hour, 1 day,
7 days, 30 days or a custom number of seconds. Custom values must be 1-100 views or
1-2592000 seconds. Supplying `DATABASE_URL` skips secret selection only: `--views` skips the view
menu and `--expire` skips the lifetime menu independently. Press Esc or Ctrl-C in any selection
menu to cancel without creating a share.

Non-interactive use requires an explicit secret name. When either limit flag is omitted there,
its existing default applies: one view and no expiry. To bypass all three selection menus in an
interactive terminal, supply the name and both limits, for example
`sotto share DATABASE_URL --views 3 --expire 3600`. Unlock and passphrase prompts may still apply.

A server can apply tighter limits to some accounts. When it refuses a request, the CLI prints the
server's reason and creates no link.

Use `sotto share --no-copy` to disable interactive copying, or `--copy` to request it explicitly.
These flags control the clipboard only; `--no-copy` does not skip the share limit menus.

A link stops working when its views run out, when it expires, or when you revoke it. Revoking stops
the server handing out the ciphertext; it does not erase it from the server:

```sh
sotto shares ls              # your 100 most recent links: views used, expiry, revocation
sotto shares ls --json
sotto shares revoke <token>  # the token is the part of the link between /s/ and #
```

`sotto shares ls` never shows the ciphertext or the key in the link.

Note the plural: `sotto share` creates a link, and `sotto shares` manages the links you created.
Both talk to the configured server, so they need `sotto login`; the local vault does not need to be
unlocked. A listing looks like this (synthetic token, illustrative values):

```text
active free links: 1
3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c  class=free  views=1/3  expires 2026-11-01 12:00:00+00
```

Pass the first column to `sotto shares revoke`. It is the public token, not the whole link: for
`https://getsotto.co.uk/s/3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c#...`, pass only `3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c` and never anything after
the `#`. Listing reads metadata only. It does not open a link or use up a view, and it cannot
rebuild a link for you, because the key after the `#` never reached the server.

`sotto shares ls --json` prints an object rather than a bare array: `active_free_count` and `links`,
where each link has `token`, `share_class`, `max_views`, `view_count`, `expires_at`, `revoked_at`
and `created_at`. The server returns only your 100 most recent links, so this is not a complete
history.

Revoking stops future retrievals through the link. It cannot undo a retrieval that has already
happened, so if the recipient may already have revealed the secret, change it at its source.

### Work with a team

Teams share environments through an organisation. Each member receives their own sealed copy of an
environment's key (a grant), and the server checks roles and grants on every request without being
able to read anything.

```sh
sotto org create acme                      # prints the org id
sotto init --org <org-id>                  # an org-owned project
sotto push                                 # create it on the server before sharing
sotto org invite <org-id> dev@example.com  # invite an existing Sotto user; prints their user id
sotto grant <user-id>                      # share the active environment; prints their clone command
```

The member then runs the printed command. This is not the same as **Another device**, which restores your own account on a second machine.
The member uses their own account on the same server: they run `sotto login` and `sotto unlock`,
then run the printed command in a directory that has no `sotto.toml` yet:

```sh
mkdir acme-api && cd acme-api
sotto clone <project-id> <env-id> --org <org-id>
```

Organisation membership and access to an environment are separate steps. `sotto org invite` makes
someone a member; `sotto grant <user-id>` gives them one environment. If you no longer have the user
id, `sotto org members <org-id>` lists it. `sotto clone` grants nothing by itself: it fetches the
member's existing grant, pulls that environment and writes `sotto.toml`. Without a grant it stops
with `you have not been granted this environment (ask an admin to share it)`, and an admin needs to
run `sotto grant` for them. Never hand a teammate your Emergency Kit; it unlocks your account, not
theirs.

Keep `--org` from the printed command, because it records the owning organisation so that later
pushes match the server. `--as <environment-name>` and `--name <project-label>` only set local
labels: by default the environment keeps its real name and the project is labelled `shared`.

To see who has access, remove someone, or rotate an environment's key yourself:

```sh
sotto org members <org-id>                 # members, roles and user ids
sotto org remove <org-id> <user-id>        # remove a member and rotate every environment they held
sotto rotate                               # rotate the active environment's key
```

Removing a member rotates the key of every environment they could access and revokes the machine
tokens they created in the organisation, naming each so you can recreate it. Rotation protects what
is written afterwards. Values the member has already seen stay exposed, so change them at their
source.

#### Legacy organisation entitlements

`sotto org plan <org-id>` reads the configured server's current entitlement view for that
organisation. You must be logged in (`sotto login`); the local vault does not need to be unlocked.
It prints the assigned tier, effective tier, stored trial end, and either member and
organisation-project limits or `limits: none (Team)`. These fields preserve compatibility with
existing organisation-tier records and deployments; they do not describe current Sotto Cloud
person-based pricing or a new Cloud trial offer.

Illustrative output from a legacy organisation-tier record:

```text
tier:      free
effective: free
trial:     ends 2026-07-16T09:00:00Z
limits:    3 members, 1 org project(s)
```

A self-hosted server applies no plan limits, so it reports the effective tier as `team`, whatever
tier and trial end the organisation has stored:

```text
tier:      free
effective: team
trial:     ends 2026-07-16T09:00:00Z
limits:    none (Team)
```

These examples show stored compatibility fields, not current Cloud offers. Real deployments may
show different stored tiers, trial dates, and limits.

#### Organisation audit log

Organisation owners and admins can inspect recent team activity with:

```sh
sotto org audit <org-id>
sotto org audit <org-id> --limit 20
```

Use the ID printed by `sotto org create <name>` or listed by `sotto org ls`. Results are newest first;
the default is 100 events, and the server clamps `--limit` to 1-500. The command shows one line per
event as `timestamp  action  actor`, with optional `target=...`, `env=...`, and `(detail)` fields. It
is a bounded activity/metadata view, not a way to retrieve secret values, and it does not provide
pagination or a complete historical archive.

This command retains the legacy organisation Team-entitlement gate. Self-hosted deployments skip
that plan gate, but the caller still needs the owner/admin role. If no events are available, the
command prints no rows and exits successfully.

### Machine tokens and CI

A machine token lets CI or a service read one environment without a password or Emergency Kit.
With `SOTTO_TOKEN` set, `sotto run` and `sotto export` fetch and decrypt that environment on each
run; set `SOTTO_SERVER` too, as the [GitHub Actions](#github-actions) example shows.

Machine tokens expire. A new token lasts 90 days unless you pass `--expires-in-days` with anything
from 1 to 365, and `sotto token ls` shows when each one ends. Two weeks before that, `sotto run` and
`sotto export` print a warning in the CI log.

Token commands use the logged-in user's access and the selected environment. In an organisation
environment, only an admin or owner can manage machine tokens. Listings contain active-token
metadata only: revoked and expired tokens are omitted, and the credential printed once by
`sotto token create` cannot be retrieved by listing. JSON includes `token_id`, `name`,
`created_by`, `beneficiary_id`, `beneficiary_status`, `expires_at` and `expires_in_days`;
expiry fields may be null. The `token_id` is the ID to revoke, not the token's name or the
`SOTTO_TOKEN` secret.

Use these commands to inspect, replace and revoke a token. The environment override applies to
this command only:

```sh
sotto token ls
sotto token ls --json
sotto --env staging token ls --json
sotto token create --name ci
sotto token revoke <token-id>
```

To replace a CI token, identify the old ID in the listing, create a replacement, update the CI
secret and verify CI uses it, then revoke the old ID with `sotto token revoke <token-id>`.
Revocation immediately stops that token from authenticating to the API; it does not rotate the
environment key. For an organisation environment, see the existing `sotto rotate` guidance
when key rotation is needed. Neither revocation nor rotation can erase plaintext that was already
copied.

### Interactive dashboard

From an initialised project, run `sotto` without a subcommand to browse its local secrets:

```sh
sotto
```

A local identity must exist and be unlocked before the dashboard opens. Sotto prompts for the
master password when needed. The dashboard requires terminals on stdin, stdout, and stderr.
If one is missing, CI is active, or `TERM=dumb`, a bare invocation writes help to stderr and exits
with code 2.
Use `sotto --help` to request help explicitly.

Use the arrow keys or `j` and `k` to navigate. Press `/` to search, `?` for help, and Tab to switch
environments. This switch lasts only for the dashboard session; `sotto env use` changes the
project's default. Press `r` to reveal or conceal the selected value, `c` to copy it to the
clipboard, and `q` to quit. `Esc` closes the current search or dialog; from the main view, it
clears an active search or quits.

Press `t` to browse themes with a live preview. Use the arrow keys to preview each theme, then
press `Enter` to save your choice or `Esc` to cancel. The [theme precedence rules](#output-themes)
explain how command and environment overrides affect the saved preference.

### Inspect local status

Run `sotto status` to see whether a local identity exists, whether the session is unlocked, and
which project and environment the current directory selects. It reads local state. It does not
check server connectivity or prove that every stored secret can be decrypted.

```sh
sotto status
sotto status --json
```

Use `sotto status --json` in scripts. `initialized` reports whether a local identity exists, and
`unlocked` reports whether a master key is cached. `project` contains `name` and `environment`
from the nearest `sotto.toml`, or is `null` outside a configured project. A successful command can
report `unlocked: false`, so scripts must inspect that field if they need an unlocked session.

These JSON examples are illustrative. With a `sotto.toml` but no local identity:

```json
{"initialized":false,"project":{"environment":"dev","name":"demo"},"unlocked":false}
```

Without a `sotto.toml` or local identity:

```json
{"initialized":false,"project":null,"unlocked":false}
```

An absent `sotto.toml` means no project is selected. A malformed or unreadable file is an error.

### Lock, unlock and log out

For an already-initialised local identity, lock the vault, check its state, then
unlock it when you need it again:

```sh
sotto lock
sotto status
sotto unlock
```

`sotto lock` clears the cached local master-key session used by subsequent CLI
commands. It does not delete your identity, encrypted vault or saved sync-server
login. `sotto unlock` uses your master password when no valid cached session is
available. Commands that need local decryption also request unlocking when needed.
See [Inspect local status](#inspect-local-status) for the status fields.

The local session lasts 12 hours from when the key is cached, not from your last
activity. Expiry is checked when the cached key is read. Running `sotto unlock`
while already unlocked does not extend that lifetime. Neither locking nor expiry
clears plaintext already held by running applications.

To clear the CLI's saved sync-server login as well, run both commands:

```sh
sotto lock
sotto logout
```

`sotto logout` removes the CLI's stored server session token, independently of the
local unlock cache. It does not sign out browser sessions or other devices.

### Cloud status

To inspect the configured server and your account's Cloud eligibility, first log in:

```sh
sotto login
sotto cloud status
sotto cloud status --json
```

The command needs an active login but does not unlock the local vault or open a browser. It reports the configured `server`, the server's `server_info`, the account `eligibility`, and a `billing_url` only when the returned state says billing is available. The human-readable output also shows `actions`, `next_actions`, and any optional `paid_through_epoch`, `recovery_until_epoch`, or `export_until_epoch` values. A missing `server_info` or `eligibility` means that an older server does not expose that endpoint; it is not proof that access is allowed or denied.

The JSON form is useful for scripts. Its top-level shape is:

```json
{
  "server": "https://getsotto.co.uk",
  "billing_url": "https://getsotto.co.uk/cloud",
  "server_info": {
    "deployment_mode": "cloud",
    "entitlement_model": "organisation_tiers_v1"
  },
  "eligibility": {
    "model": "person_eligibility_v1",
    "state": "paid",
    "deployment_mode": "cloud",
    "account_initialized": true,
    "billing_available": true,
    "paid_through_epoch": 1798761600,
    "recovery_until_epoch": null,
    "export_until_epoch": null,
    "actions": {
      "setup": false,
      "billing": false,
      "export": false,
      "revoke": true
    },
    "next_actions": ["revoke"],
    "payer": "personal"
  }
}
```

The example is synthetic; `actions` and the optional dates come from the returned account state, and older servers can return null for either endpoint. `sotto cloud status` only reports status and may print a billing URL. It does not start a purchase or change eligibility. For local vault and environment diagnostics, use `sotto status`; that view is separate from the server and account information above.

### Output themes

The CLI ships five built-in themes: `nord` (the default), `sordino`, `terminal`, `monochrome`,
and `tokyo-night`. Manage them with `sotto theme`:

```sh
sotto theme ls        # list available themes; the active one is marked
sotto theme set nord  # save a preference for later commands
sotto theme current   # print the theme this shell resolves to
```

`--theme <name>` picks a theme for one command. `SOTTO_THEME` sets it for a shell, and
`sotto theme set` saves it. Precedence is `--theme`, then `SOTTO_THEME`, then the saved
preference, then `nord`; an unknown name warns on standard error and falls back to `nord`.

Styling applies to interactive terminals only. `--plain`, a non-empty `NO_COLOR` (the
[no-color.org](https://no-color.org) contract), CI environments, piped standard output, and
redirected standard input each suppress colour and decoration while keeping the selected
palette, so scripts and logs read plain text.

Custom themes are TOML files in the `themes` directory inside your platform data directory:
`~/Library/Application Support/sotto` on macOS, `%APPDATA%\sotto` on Windows, and
`$XDG_DATA_HOME/sotto` or `~/.local/share/sotto` on Linux (`SOTTO_DATA_DIR` moves that
directory). Each `*.toml` file needs the tokens below and takes its name from a `name` field,
or from the filename when the field is absent; a file that fails to parse is skipped. Colours
accept hex values, ANSI colour names, `ansi(<index>)` indices, and `default`.

```toml
bg = "#120024"
fg = "#ffffff"
accent = "#ff007f"
success = "#00ff66"
warning = "#ffaa00"
error = "#ff0033"
muted = "#775588"
border = "#331144"
```

Saved as `synth.toml`, that file adds a `synth` theme. Apply it with `sotto theme set synth`,
or for one command with `sotto --theme synth <command>`.

### Restore an earlier secret version

Start in an existing configured project and keep the same environment selected throughout.
You need a logged-in session and an unlocked local store. History comes from the server:
values that have never been pushed are not available there for recovery.

```sh
sotto history DATABASE_URL
```

Choose a readable, positive version number from the history. The example below uses `2`;
replace it with an available version, without the displayed `v` prefix.

```sh
sotto rollback DATABASE_URL 2
sotto push
```

Rollback restores the chosen value as a new local version. It does not erase or rewrite
older history, and the server is not updated until the subsequent push.

For metadata-only JSON, use:

```sh
sotto history DATABASE_URL --json
```

Each row has `version`, `readable` (whether the value could be decrypted) and `size_bytes`
(the plaintext byte count, or null for an unreadable version). No values are printed.
By contrast, `--reveal` prints secret values; it cannot be combined with `--json`.

If there is no server history, check the selected environment and whether it has been
pushed. Pushing now cannot recover older values that were never synced. For an unreadable
version, follow the CLI's suggestion to run `sotto pull` and try again; pulling does not
guarantee that a version can be decrypted with the available keys.

For shell scripts and CI, see the [CLI scripting and exit-status reference](CLI-SCRIPTING.md).

### Environment variables

The CLI reads these variables:

- `SOTTO_SERVER`: the server URL, overriding the one saved by `sotto login`. With `SOTTO_TOKEN`,
  it is required unless `sotto login` saved a server on that machine.
- `SOTTO_TOKEN`: a machine token. `sotto run` and `sotto export` then read the token's environment
  instead of the local vault.
- `SOTTO_PASSWORD`: the master password, for scripts that cannot answer a prompt. Prefer the hidden
  prompt where you can.
- `SOTTO_SECRET_KEY`: the Secret Key from your Emergency Kit, for `sotto setup` without a prompt.
- `SOTTO_DATA_DIR`: moves the local data directory, which holds the encrypted store, saved settings
  such as the server, and custom themes.
- `SOTTO_THEME` and `NO_COLOR`: output styling; see [Output themes](#output-themes).

## Self-hosting

One command brings up a complete deployment - Postgres, the server, and Caddy with automatic
HTTPS - from [`deploy/docker-compose.prod.yml`](deploy/docker-compose.prod.yml); the runbook is
[`deploy/README.md`](deploy/README.md). The pieces also work standalone: serve the web app and API
from **one origin** (so the session cookie and CSP stay same-origin) - the included
[`Caddyfile`](Caddyfile) serves `web/dist` and reverse-proxies the API, with security headers;
[`Dockerfile`](Dockerfile) builds the server image (migrations run on boot).

`SOTTO_DEPLOYMENT_MODE` defaults to `self_hosted`. In that mode the server applies no plan limits
or other hosted-service restrictions, and it needs no Stripe account. The `cloud` mode is for the
service Sotto operates; read the runbook before you set it.

Point the CLI at your server once; later commands reuse it:

```sh
sotto login --server https://sotto.example.com
```

## Architecture

```text
CLI (native) ─────┐
                  ├── sotto-core ── versioned encrypted data
Web client (WASM) ┘                         │
                                            ▼
                                  sync/API server
                                  (ciphertext only)
```

The workspace contains four crates:

- `crates/core` - shared cryptographic types and the complete crypto implementation.
- `crates/cli` - the `sotto` command-line interface and primary native client.
- `crates/server` - the Axum-based synchronisation API.
- `crates/wasm` - `wasm-bindgen` bindings that expose the core to web clients.

The browser client in `web/` is a React and Vite app that runs the core through `crates/wasm`.

## Prerequisites

- [Rustup](https://rustup.rs/) with stable Rust 1.89 or newer
- The `clippy` and `rustfmt` components
- The `wasm32-unknown-unknown` target
- For the web client: Node.js 22 (what CI uses) and `wasm-pack` (CI pins v0.15.0)
- For the server and its database tests: Postgres 16, which `docker compose up -d` provides
- For the script checks: Python 3.11 or newer

The checked-in `rust-toolchain.toml` asks Rustup to install the required components and
target automatically.

## Developing

Clone the repository, then build and test the workspace:

```sh
git clone https://github.com/getsotto/sotto.git
cd sotto

cargo build --workspace
cargo test --workspace
```

`cargo test --workspace` needs no database, so it skips the server's integration tests and the
CLI's end-to-end test without failing. To run them, start the local Postgres and opt in with
`SOTTO_RUN_DB_TESTS=1`; they refuse a `DATABASE_URL` that points anywhere but this machine:

```sh
docker compose up -d
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto SOTTO_RUN_DB_TESTS=1 \
  cargo test -p sotto-server -p sotto-cli
```

Use the CLI locally (no server required):

```sh
cargo run -p sotto-cli -- --help
cargo run -p sotto-cli -- init                 # create an identity + project; prints your Emergency Kit
cargo run -p sotto-cli -- set DATABASE_URL     # hidden prompt
cargo run -p sotto-cli -- run -- your-command  # inject secrets as env vars into a subprocess
```

Secrets are encrypted at rest in a local SQLite store; the master key is cached in the OS keychain
with a TTL. Syncing to a server (`login`/`push`/`pull`/`setup`/`share`) is optional.

### Running the server

The server needs Postgres (a `docker compose up -d` brings one up for local use):

```sh
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto cargo run -p sotto-server
curl http://127.0.0.1:8080/health   # → ok
```

GitHub OAuth login requires `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET`. For any non-local
deployment also set `SOTTO_PUBLIC_URL` to the server's externally reachable origin - it builds the
GitHub callback URL and must match the OAuth app's registered callback (it otherwise defaults to
`http://localhost:8080`) - and, for the web client, `SOTTO_WEB_ORIGIN`. Without OAuth the server
still boots (serving `/health` and running migrations), but login and every authenticated endpoint
(sync, share creation) are unavailable.

### Web client

The browser client runs the same crypto core via WebAssembly (`web/`):

```sh
cd web
npm ci
npm run dev        # dev server (proxies the API to localhost:8080)
npm run build      # production bundle → web/dist (strict CSP + Subresource Integrity)
npm run typecheck  # TypeScript checks; needs the WASM bindings a build generates
npm test           # unit tests (Vitest)
npm run smoke      # load the WASM core under Node and exercise it
```

The Playwright funnel suite (`npm run e2e`) needs a database and a production build first;
[`web/e2e/README.md`](web/e2e/README.md) explains the setup.

## Development checks

Run the same core checks used by CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Supply-chain policy is defined in `deny.toml` and checked in CI with
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny):

```sh
cargo deny check
```

The `supply-chain` CI job audits the whole lockfile with `scripts/check-cargo-audit`. The
[lockfile audit guide](.ci/cargo-audit.md) covers the pinned cargo-audit version, the policy tests,
and the rules for the dormant lockfile exceptions.

The cross-implementation gate proves the native and WASM builds agree - native-produced ciphertext
decrypts byte-for-byte in WASM from shared golden vectors:

```sh
wasm-pack test --node crates/wasm
```

The web build, its dependency audit, and the README translation check (`scripts/check-readme-i18n`)
also run in CI (`.github/workflows/ci.yml`). [CONTRIBUTING.md](CONTRIBUTING.md) links the guides for
fuzzing, Kani proofs, coverage, and the assurance checks.

## Telemetry

The **server** sends one anonymous ping per day (the first 10-20 minutes after boot) to
`https://getsotto.co.uk/telemetry/v1/ping`, so we can count active instances and see which
versions are in the wild. The response names the latest release, and the server logs a line when
it is running an outdated version. This is the **entire** payload - the sending code is
[`crates/server/src/telemetry.rs`](crates/server/src/telemetry.rs), and a unit test pins the
payload to exactly these four fields:

```json
{ "instance_id": "0d0972a6-…", "version": "0.2.0", "os": "linux", "arch": "x86_64" }
```

`instance_id` is a random UUID generated once and stored in your database - derived from nothing,
so it identifies no hardware, host, or account; deleting it makes the instance a fresh anonymous
counter. The ingest side stores no IP addresses and no derived location. There are no org, member,
or secret counts, and no usage events. The **CLI, web client, and WASM never send anything**.

Opt out with `SOTTO_TELEMETRY=off` (or the cross-tool
[`DO_NOT_TRACK=1`](https://consoledonottrack.com)) - when disabled the task is never started and
no request is ever made. `SOTTO_TELEMETRY_URL` redirects the ping (e.g. to aggregate a private
fleet), and records idle for 12 months are purged from the hosted census.

## Security

Sotto's model is zero-knowledge: plaintext secrets and usable decryption keys stay on client
devices, and the server sees only ciphertext plus minimal metadata. This is implemented but **not
yet independently audited** - see [SECURITY.md](SECURITY.md) for the model, the honest metadata
exposure, how the (re-fetched, weaker) web surface is hardened, and how to verify signed releases.
The full adversary model, guarantees, and explicit non-goals are published in
[THREAT-MODEL.md](THREAT-MODEL.md). Report vulnerabilities privately per SECURITY.md.

The scope and evidence prepared for an independent audit are in [SECURITY-AUDIT-SCOPE.md](SECURITY-AUDIT-SCOPE.md).

## Contributing

Sotto is Apache-2.0 and welcomes contributions. Start at [CONTRIBUTING.md](CONTRIBUTING.md);
[good first issues](https://github.com/getsotto/sotto/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
are labelled, and questions that are not bugs belong in
[Discussions](https://github.com/getsotto/sotto/discussions).

Please report vulnerabilities privately per [SECURITY.md](SECURITY.md).

## Licence

Licensed under the [Apache License, Version 2.0](LICENSE) - all crates and the web client.
You may not use this project except in compliance with the License. Unless required by
applicable law or agreed to in writing, software distributed under the License is distributed
on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND.
