# Accounts

Each lead and its workers use one Codex account. Different teams can run
at the same time with different accounts. Each account has its own app-server
process, limits cache, approvals, and native profile directory.

Use the account menu beside the model to choose an account for an empty chat.
If its rules exclude the current folder, choose an allowed project in the folder dialog.
The server applies the account and folder together. Cancel or rejection preserves both.
After the first message, the account stays fixed. Set the default in **Manage
accounts** for new chats. Existing chats keep their account.

The application finds native profiles in `~/.codex`, `~/Projects/*/.codex-profile`,
`~/Projects/*/.codex`, `~/.codex/profiles/*`, and `~/.codex-profiles/*`.
`CODEX_HOME` selects the initial default profile. Duplicate native account IDs
appear once during discovery. Add another home directory through the account
manager, or use **Sign in to another account** for native device authorization.

Existing profiles keep their own configuration, skills, plugins, and credentials.
The application does not copy their tokens or change the global login.
Codex handles token refresh in the original profile. If a profile changes to a
different account, new operations fail until its original identity is restored.

New sign-ins use private homes under the application state directory, in
`accounts/`. They start with a copy of the default configuration and links to
its skills, plugins, rules, and `AGENTS.md`. Later default configuration edits
do not change that copy. The registry contains profile metadata, not tokens.

Usage and reset credits belong to the selected account. A reset request checks
the native account ID and exact credit before use. Local cost estimates cover
the local usage sources reported by the cost reader, across accounts. They are
not an account invoice. The application does not rotate accounts automatically.

Use `codex-control accounts` to list account keys from the terminal.
`codex-control create`, `models`, and `limits` accept `--account KEY`.
Without that option, these commands use the saved default account.

## Project rules

Each account can use all projects, a list of project directories, or no projects.
Edit this list in **Manage accounts**. Rules include subdirectories and registered
Git worktrees of the same repository. A rule for a repository subdirectory grants
only that subtree in a linked worktree. Sibling path prefixes and symlink escapes
do not match. Rule edits use a revision to prevent conflicting saves.

The server checks the project before new work, resumed turns, agent delegation,
and managed project operations. Empty chats can exist outside the allowed list
so the user can choose another project. Updated rules apply to subsequent
operations. They do not terminate a model request or command already in progress.

**Dangerously skip rules** is an explicit exception for the current team.
Workers inherit it from their lead and cannot enable it themselves. The exception
can change only when the team is idle. New teams and conversation branches do
not inherit the exception. An orange **Rules off** badge shows an active exception.
The switch does not disable Codex sandboxing, approvals, or account identity checks.

These are account/project admission rules, not operating-system file isolation.
They do not prevent a native command from accessing another directory that its
Codex permissions allow. They do not change vanilla Codex CLI profiles or other
applications. Native filesystem permissions remain a separate boundary.

From the command line:

```sh
codex-control account-rules ACCOUNT --allow /path/to/project
codex-control account-rules ACCOUNT --deny-all
codex-control account-rules ACCOUNT --allow-all
codex-control create 'Task' --account ACCOUNT --cwd /path/to/project --dangerously-skip-rules
```

Checks: `tests/accounts-contract.py`, `tests/runtime-accounts-contract.py`, and
`tests/accounts-ui-smoke.mjs`. Fixture checks do not make paid model requests
or redeem real reset credits.

## Add an account

Open the account menu, select **Add account**, then **Sign in to another account**.
Open the sign-in page and use the new account with the displayed device code.
Studio starts a separate app-server and keeps the current teams on their existing accounts.
The new account appears after Codex confirms authentication. It does not become the default automatically.

The sign-in request survives a page reload. **Check status** reads its saved result.
**Cancel sign-in** cancels that request only. A lost response remains unconfirmed
until Studio can reconcile it with the native profile. A retry with the same request
ID does not start another login. An existing native account appears only once.

Checks: `tests/account-login-contract.py`, `tests/runtime-accounts-contract.py`,
and `tests/accounts-ui-smoke.mjs`. These checks use isolated fixtures.
