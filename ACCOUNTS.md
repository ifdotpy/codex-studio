# Accounts

A team normally uses one Codex account. During a transfer, members can use
different accounts until their active work ends. Different teams can run
at the same time with different accounts. Each account has its own app-server
process, limits cache, approvals, and native profile directory.

Use the account menu beside the model to choose an account for an empty chat.
If its rules exclude the current folder, choose an allowed project in the folder dialog.
The server applies the account and folder together. Cancel or rejection preserves both.
For an existing chat, select another account to transfer the team. The chat keeps
its identity. Set the default in **Manage accounts** for new chats.
A new chat uses the destination project's saved account, or the global default
when the project has no choice. It does not inherit another chat's account.
An explicit account choice still takes priority and remains subject to project rules.

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

## Transfer an existing team

Select the destination in the chat's account menu. Studio moves each member after
its current turn, native background commands, and managed monitors finish.
It does not interrupt commands. New input stays queued. A confirmed failed turn
continues from its saved context after transfer. Completed work and explicitly
paused agents do not start another model turn. Workers created during
the transfer join it automatically. Other teams continue independently.

The chat, agent IDs, parent links, files, settings, panel, tasks, complaints, and
message receipts stay in place. Native Codex forks each saved context in the
selected account's app-server. Its paginated history ancestry and dynamic tools
remain available. This does not promise a shared prompt cache across accounts.
The original native session remains available in the transfer receipt.

The menu shows progress and the current wait reason. **Cancel remaining** keeps
completed transfers and releases the remaining agents on their original accounts.
A late fork response is saved but does not change a cancelled transfer.
**Retry** retries a confirmed rejection or a read failure. A native request with
an unknown outcome is never submitted again automatically. Its late response can
complete the transfer. After a server restart, an unknown receipt requires review
or cancellation; it is not evidence that no fork was created.

Canonical JSONL history is copied under the destination profile's
`sessions/.studio-imports/`. Native Codex can read these files. The local cost
scanner skips that hidden directory, so imported history is not charged to the
destination estimate. Credentials, account databases, and other chats are not
copied. Missing, compressed, or conflicting history fails visibly without
replacing the source. Native projections are rebuilt by Codex.

`POST /api/agents/account-transfer` takes `id` (the lead), `account_key`, and a
UUID `request_id`. Repeating the same request returns its saved receipt. The
same request ID with different content is rejected. `action` can be `retry` or
`cancel` with the original `request_id`. These routes require the session token.
The lead's `accountTransfer` field provides progress after a page reload.

Checks: `tests/account-transfer-contract.py`, `tests/account-transfer-native.py`,
and `tests/account-transfer-ui.mjs`. The native test uses isolated profiles and a
local Responses provider. It makes no cloud model requests.
