# Local agent task workflows

Sotto can expose project-declared tasks to a local coding agent through an MCP server over
standard input and output. The agent can discover task names and request an approved task to run;
it cannot provide a command, arguments, secret names, or secret values. This guide covers the
current CLI behaviour. The commands must be available in your installed Sotto release; check
`sotto --help` if a command is missing.

## Security boundary

Task approval is permission to run the current project code with plaintext secrets from one chosen
Sotto environment. A task can read those secrets and can send, print, transform, or otherwise
disclose them. Sotto does not sandbox a task. Approve only project code and dependencies you trust,
and do not approve an untrusted checkout.

Sotto stores approvals in local user data outside the project checkout. An approval is specific to
the project, task declaration, resolved executable, and one environment. Editing execution-relevant
task fields or resolving the executable to a different path requires approval again. The checked-in
task file declares what can be requested; it does not itself grant access to secrets.

The task process receives the selected environment's secrets as environment variables. Sotto
discards task stdout and stderr and returns a generated task result, but this is not secret
redaction: project code can disclose secrets through other means. The task file and approval
database should never contain secret values.

## Declare a task

If this directory is not already a Sotto project, run `sotto init` first. Create
`sotto.tasks.toml` at the project root. This example makes the project's existing `npm test`
script available in the `dev` environment:

```toml
version = 1

[[task]]
name = "test"
description = "Run the project test suite"
executable = "npm"
args = ["test"]
environments = ["dev"]
timeout_seconds = 600
```

The file is TOML with `version = 1` and one or more `[[task]]` entries. Each task has a unique
lowercase name, description, fixed executable, ordered `args` array, and one or more allowed
`environments`. An optional `working_directory` may be absolute or relative to the Sotto project
root; the default is the project root. An optional `timeout_seconds` must be between 1 and 3600; if
omitted, the task may run for up to 600 seconds.
Unknown fields and invalid declarations fail validation. Keep commands and arguments visible in
code review; do not put secrets in this file.

Validate the declaration before approving it:

```sh
sotto task validate
```

List declared tasks in the project:

```sh
sotto task ls
```

## Approve and run from the CLI

Approval is a local, interactive action. Sotto displays the project identity, resolved executable,
each argument, working directory, environment and timeout, then asks for confirmation. Read that
display and the trust warning before accepting:

```sh
sotto task approve test --env dev
```

Unlock Sotto in a separate terminal, then run the named task:

```sh
sotto unlock
sotto task run test --env dev
```

The selected environment must be listed by the task. The CLI prints a generated status and summary;
it does not forward the child process output. A non-zero exit, timeout, cancellation, or launch
failure is reported as a task failure.

Review and revoke local approvals from the project directory:

```sh
sotto task approvals
sotto task approvals --env dev
sotto task revoke test --env dev
```

Revoking an approval prevents further CLI or MCP runs for that task/environment until it is
approved again. Changing the task declaration or resolved executable also makes the old approval
unusable.

## Connect an MCP host

Configure your local MCP host to launch `sotto mcp` with the Sotto project root as its working
directory. MCP hosts use different configuration formats; the relevant process settings are:

```text
command: sotto
arguments: ["mcp"]
working directory: /absolute/path/to/your/project
transport: stdio
```

Start the host from the project where `sotto.tasks.toml` lives. Run `sotto unlock` separately in a
terminal before asking the agent to run a task. The server does not read a password from its MCP
input stream. Task discovery works while locked; task execution fails with unlock guidance if the
session is locked or expires.

Sotto exposes only two tools:

- `list_tasks` returns declared task names, descriptions and allowed environments. It does not
  reveal approval state, secret names or values.
- `run_task` accepts a declared `task` and an allowed `environment`. It accepts no command,
  arguments or secret values. A successful response contains `task`, `environment`, `status`,
  `exit_code` and a Sotto-generated `summary`; child output is discarded.

The server uses stdin and stdout for MCP protocol messages. Do not wrap it in a launcher that writes
ordinary output to stdout. Press Ctrl+C or stop the host to shut it down; active tasks are cancelled
and Sotto waits for them to stop before exiting.

## Troubleshooting

- **No Sotto project found:** run the command from the project directory or one of its
  subdirectories, and confirm `sotto init` created the project configuration.
- **Task file missing or invalid:** create or correct `sotto.tasks.toml`, then run
  `sotto task validate` or `sotto task ls`. Unknown fields, duplicate names, and unsupported file versions are
  rejected.
- **Inspect project declarations vs. approvals:** `sotto task ls` lists declared tasks in `sotto.tasks.toml`, while `sotto task approvals` lists which of those declarations have received local developer approval for specific environments.
- **Environment not allowed:** add the environment to that task's `environments` array, review
  the change, and approve the task/environment pair again.
- **No current approval:** inspect the displayed command and run `sotto task approve <name> --env
  <environment>` in an interactive terminal. Changes to the declaration or resolved executable
  require another approval.
- **Vault locked or session expired:** run `sotto unlock` in a separate terminal and retry. MCP
  cannot prompt for the password on its protocol stream.
- **Approval command has no terminal:** run it directly in an interactive terminal; Sotto does not
  create an approval from a non-interactive command or MCP request.
- **Task timed out:** the default limit is 600 seconds and the maximum is 3600 seconds. Set
  `timeout_seconds` in the declaration if the task needs a different limit, then review and approve
  the changed declaration.
- **MCP host cannot find the project or executable:** set the host's working directory to the
  project root and ensure a bare executable name such as `npm` resolves on the host's `PATH`.

The CLI and its task runner are supported on the platforms listed for the current Sotto release.
The MCP server uses local stdio; this guide does not certify any particular graphical agent host.
The process runner terminates the process group on Unix. On Windows, it stops and waits for the
direct child; descendant cleanup is not guaranteed.
