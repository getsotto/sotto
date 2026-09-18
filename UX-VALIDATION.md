# UX validation session

Use this template to record a repeatable manual check of the released Sotto journey. It complements automated unit, integration, browser and assurance tests; it does not replace them.

## Safety rules

- Use synthetic secrets that have no value outside the test session.
- Never use customer credentials, recovery material, production accounts or production tokens.
- Prefer an isolated test server and test accounts. If a mock is used, say so explicitly.
- Redact plaintext secret values, session cookies, bearer tokens, Emergency Kits and recovery material from logs and screenshots.
- Keep enough evidence to reproduce an outcome without publishing sensitive material.

A safe example value is `synthetic-not-a-secret`. Do not replace it with a real credential.

## Session metadata

Fill this in before testing.

| Field | Value |
| --- | --- |
| Date and tester | |
| Sotto version or commit | |
| Operating system | |
| Install method | |
| Test account type | |
| Server | isolated real server / mock / local only |
| Server version or commit, if applicable | |
| Browser, if a web surface is exercised | |

## Evidence levels

Record the strongest level actually demonstrated. Do not treat one level as proof of another.

1. **Screen rendered** - the UI or terminal output appeared.
2. **Request succeeded** - the expected server operation completed successfully.
3. **Secret decrypted** - the intended recipient recovered the synthetic plaintext locally.
4. **Application command completed** - a benign command ran with the expected synthetic value injected.

For example, a successful page load is not proof that a secret decrypted, and a successful API response is not proof that an application command received the value.

## Synthetic-secret journey

Use the commands that match the surface under test and record deviations. The placeholders below are not credentials.

### 1. Initialise an isolated project

```sh
sotto init
```

For an organisation-owned project, use the existing team flow instead:

```sh
sotto org create ux-validation
sotto init --org <org-id>
```

Record whether initialisation completed and whether any waiting or recovery state appeared.

### 2. Add a synthetic value

Set or import only synthetic material, for example `UX_VALIDATION_TOKEN=synthetic-not-a-secret`.

```sh
sotto set UX_VALIDATION_TOKEN
```

Alternatively, import a test-only `.env` file with `sotto import`. Record the method used and confirm the value is available to Sotto without copying a real secret into the validation record.

### 3. Run a benign command

Use `sotto run -- <command>` with a harmless command that proves the expected synthetic value was injected. Record the command separately from its observed outcome. Do not include plaintext production values in captured output.

### 4. Publish or sync, where applicable

When validating a real isolated server, authenticate and sync using the supported flow:

```sh
sotto login
sotto push
```

Record whether authentication completed, whether the sync request succeeded, and any waiting or retry state. For a local-only or mocked session, mark this step not applicable instead of reporting it as successful.

### 5. Complete the teammate path

For an organisation-owned environment, exercise the supported invite and grant path with a test teammate account:

```sh
sotto org invite <org-id> teammate@example.test
sotto grant <user-id>
```

The granting command prints the project/environment identifiers needed by the recipient. The teammate then follows the emitted `sotto clone ... --org ...` command.

On the teammate side, record these separately:

- invite or membership became ready;
- clone or access request succeeded;
- the synthetic secret decrypted;
- a benign `sotto run -- <command>` completed with the expected synthetic value.

## Recovery and failure checks

Exercise the rows that apply to the surfaces changed by the contribution. Do not force unrelated failures merely to fill the table.

| Case | Expected outcome | Observed outcome | Recovery action | Result |
| --- | --- | --- | --- | --- |
| Offline use | Local operations that do not require the server remain understandable; networked operations fail clearly | | | |
| Expired session | The user is told to authenticate again rather than appearing to succeed | | | |
| Missing access | The recipient cannot decrypt or use an environment they were not granted | | | |
| Empty project | The empty state is clear and does not imply secrets exist | | | |
| Clipboard or command-copy failure | The UI still exposes a recoverable manual path where that control exists | | | |

For every failure, record whether retrying, logging in again, requesting access, or copying the command manually restored the journey.

## Expected and observed outcomes

Keep expectations and observations separate. Add one row per meaningful step.

| Step | Expected | Observed | Evidence level | Pass / fail / waiting |
| --- | --- | --- | --- | --- |
| Initialise | | | | |
| Set or import synthetic secret | | | | |
| Benign command | | | | |
| Login and sync, if applicable | | | | |
| Teammate access ready | | | | |
| Teammate decrypt | | | | |
| Teammate command | | | | |

A waiting state is a result, not a pass. Record what the user was waiting for and what made the journey ready to continue.

## Safe logs and screenshots

Before attaching evidence:

- crop or redact secret values, tokens, cookies, Emergency Kits and recovery material;
- prefer synthetic account names and `example.test` addresses;
- include the command name, timestamp and relevant status without exposing plaintext secret content;
- check terminal scrollback and browser developer tools for copied tokens before sharing a screenshot;
- describe omitted sensitive fields rather than replacing them with realistic-looking credentials.

Name evidence by step, for example `03-run-success.txt` or `05-teammate-access.png`, so another contributor can map it back to this record.

## Session summary

- **Overall result:** pass / fail / partial / blocked
- **First failing or waiting step:**
- **Recovery that worked:**
- **Automated coverage also run:**
- **Follow-up issue or PR:**

If a manual session finds a product defect, file or link the issue separately. Keep the validation record factual and do not claim that a rendered screen, successful request, or mocked response proves end-to-end secret recovery.
