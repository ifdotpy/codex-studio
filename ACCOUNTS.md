# Accounts

A team normally uses one Codex account. During a transfer, members can use
different accounts until their active work ends. Different teams can run
at the same time with different accounts. Each account has its own app-server
process, limits cache, approvals, and native profile directory.

Each project has one default account. Open the project's menu and select
**Project account** to change it. New chats in that project use this account.
The setting is available on desktop and mobile. Existing chats keep their account.

Use the account menu in Settings to select any available account for an empty
chat. This choice affects that chat only. For an existing chat, select another
account to transfer its team with the saved context.
The global default in **Manage accounts** applies to projects without a saved choice.

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
Without that option, `create` uses the project account. `models` and `limits` use
the global default account.

## Project accounts

Studio uses the project path to select a default account and group chats.
Models can use files and skills outside that path. Account selection adds no
folder allowlist. Native sandbox and approval settings still apply.

Project defaults use canonical paths. A nested directory uses the nearest saved
ancestor until it has its own setting. Account changes use a revision to detect
conflicting edits. A repeated successful save preserves the same revision.

The update preserves existing project and chat identities. It keeps an explicit
project default when one exists. Otherwise it selects the latest non-deleted lead's
account for that exact project, an unambiguous old account rule, or the global default.
Old account rules remain only as migration data. They no longer restrict operations
or appear in model instructions. Existing native profiles and credentials remain unchanged.

From the command line:

```sh
codex-control project-account /path/to/project
codex-control project-account /path/to/project --account ACCOUNT
codex-control create 'Task' --cwd /path/to/project
codex-control create 'Task' --cwd /path/to/project --account ACCOUNT
```

Checks: `tests/project-accounts-contract.py`, `tests/account-project-runtime.py`,
`tests/runtime-accounts-contract.py`, and `tests/account-project-ui.mjs`.
Fixture checks do not make paid model requests or redeem real reset credits.

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

## Multiple project accounts

Select project accounts in **Project account** and choose one default. Linked accounts
appear first in the chat account menu with a **Project** label. Other accounts remain
available. This selection adds no folder restrictions or automatic account rotation.
Existing chats keep their account. For parallel work, create separate chats and select
a linked account before the first message.

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
