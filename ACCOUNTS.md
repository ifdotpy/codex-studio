# Accounts

Each lead and its workers use one Codex account. Different teams can run
at the same time with different accounts. Each account has its own app-server
process, limits cache, approvals, and native profile directory.

Use the account menu beside the model to choose an account for an empty chat.
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

Checks: `tests/accounts-contract.py`, `tests/runtime-accounts-contract.py`, and
`tests/accounts-ui-smoke.mjs`. Fixture checks do not make paid model requests
or redeem real reset credits.
