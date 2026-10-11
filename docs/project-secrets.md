# Project secrets

A project secret is a named value, such as an API key. Studio keeps it in the VM, outside the
project tree. Every agent process of that layr project gets it as an environment variable when
the process starts.

## What it protects

An agent can read each variable that it receives. So a secret is not hidden from the agents of
its project. It stays out of the project repository, its history, the vault, the Mac share and
the other projects. No API reply, error, log, receipt, command line or transcript of Studio
contains a value. The request receipts in the VM keep a SHA-256 digest of each request, and
that digest covers the value. An agent can still print a value into its own transcript or a
file.

## Where the values are

The root broker in the VM keeps one file per project:
`/var/lib/codex-studio/layr-admin/secrets/<project-id>.json`. The file has mode 0600 and its
folder has mode 0700. Only root reads them. The broker writes a new file and renames it over
the old one, so a failed write keeps the old file.

## When an agent gets a secret

The broker starts each agent process through `project_env.py`:

- the lead and the workers, with Codex or Claude;
- the commands that an agent runs through Studio (`layr.exec`, for example monitors);
- the operator commands of `codex_layr_exec.py` (`project.exec`), which run as the lead's user.

The wrapper reads the file when the process starts. It gives the values to the process after
`runuser` drops root. A process that runs already keeps its environment. A secret that you set
or remove reaches the processes that start after the change. To give a change to a running
chat, start its agent again.

## Rules

- A name matches `^[A-Z_][A-Z0-9_]{0,63}$`.
- Studio refuses the names that the agent start sets: `PATH`, `HOME`, `USER`, `LOGNAME`,
  `SHELL`, `LANG`, `LAYR_ROOT`, `LAYR_SESSION`, the provider names (`CODEX_HOME`,
  `CLAUDE_CONFIG_DIR`, `STUDIO_CLAUDE_*`), and each name that starts with `STUDIO_`, `LAYR_`,
  `CODEX_` or `CLAUDE_`.
- A value is UTF-8 text from 1 byte to 16 KiB, without a NUL character.
- A project has at most 64 secrets.
- The provider process gets the secrets too. A variable such as `ANTHROPIC_API_KEY` or
  `OPENAI_API_KEY` can change how the provider signs in. Use another name for your own code
  when you do not want that.

## Set a secret

In the sidebar, click the LAYR chip of the project and choose **Secrets…**. The dialog shows
the names. Use **Remove** to remove one. Type a name and a value, then click **Add**. Add
replaces the value of a name that exists. Studio never shows a stored value.

From a terminal on the Mac, with the Studio Python from the installed app scripts and the VM
running:

```sh
python3 codex_project_secrets.py list PROJECT_FOLDER
python3 codex_project_secrets.py set PROJECT_FOLDER NAME
python3 codex_project_secrets.py remove PROJECT_FOLDER NAME
```

`set` reads the value from stdin. At a terminal it asks for the value and does not show it.
The command never takes a value as an argument. `set` and `remove` print an operation ID.
After a lost reply, run the same command again with `--operation-id ID`; the VM returns its
saved reply and does not apply the change twice.

## Example: the SAP project

On the Mac, `/Users/igor/Projects/SAP/.envrc` sources `.codex-keychain.envrc`. That file reads
`ZULIP_APEIRO_SHOWROOM_API_KEY` from the macOS keychain with `security find-generic-password`.
The VM has no `security` command, so the variable is empty there. Copy the value from the
keychain into the project secrets once:

```sh
security find-generic-password -w -a zulip-apeiro-showroom -s codex-zulip-apeiro-showroom-api-key \
  | python3 codex_project_secrets.py set /Users/igor/Projects/SAP ZULIP_APEIRO_SHOWROOM_API_KEY
```

The command removes the final newline of the input. New SAP agents in the VM then start with
`ZULIP_APEIRO_SHOWROOM_API_KEY` in their environment.

`.codex-keychain.envrc` sets the variable even when `security` fails, so in the VM it replaces
the secret with an empty value when a shell loads it. Keep a value that is already there:

```sh
if [ -z "${ZULIP_APEIRO_SHOWROOM_API_KEY:-}" ] && command -v security >/dev/null 2>&1; then
  export ZULIP_APEIRO_SHOWROOM_API_KEY="$(security find-generic-password -w -a zulip-apeiro-showroom -s codex-zulip-apeiro-showroom-api-key 2>/dev/null || true)"
fi
```

## Interfaces

- Guest broker methods: `secrets.set` (projectId, name, value), `secrets.remove` (projectId,
  name), `secrets.list` (projectId). Each returns `projectId` and the sorted `names`. A
  repeated request ID of `set` or `remove` returns the saved reply.
- Studio API: `POST /api/project-secrets/list`, `/set` and `/remove`. `set` and `remove` take
  a `requestId`.
