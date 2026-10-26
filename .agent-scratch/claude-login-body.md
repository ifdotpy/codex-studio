## Problem

A Claude agent whose account has no readable login stays in its start with no reason (seen in a rehearsal on another Mac). The owner sees "Waiting for the provider session" and the agent never starts.

## Cause

Studio writes a Claude account row's saved identity (`accountId`, `email`, `_credentialIdentity`) when it reads the login. Without a readable login the row has none, so the host launch (`claude_launch`) refuses with `claude/account-identity`. `AccountRegistry::dispatch_account_page` then returned `StartFailed`. The registry logged "account registry reconciliation deferred" and retried with backoff, and the agents only showed `startWait {reason: provider_session}`. No account worker started, so `account_auth_gate::require_before_start`, which shows "Sign in with claude auth login first", never ran.

## Change

- `provider_adapter::CLAUDE_ACCOUNT_IDENTITY_REFUSAL` names the refusal code (one owner).
- `account_auth_gate::launch_hold_reason` maps that launch refusal to the Claude sign-in message. Any other launch failure maps to nothing and keeps the existing retry.
- `account_auth_gate::hold_unstarted` holds queued starts as the gate does: status `failed`, error "Sign in with claude auth login first", the start wait removed, and the pending input kept. An agent that moved on (another epoch, a claim, a stop or a delete) is left alone. The existing `release_held` requeues held agents once the account is Ready, so the input starts once after sign-in.
- The registry calls them when the launch fails.

## Tests

`wave7_p01_registry::claude_account_without_a_readable_login_holds_its_start_for_sign_in`: the launcher refuses with the identity code. Red before the change: `left: ("queued", None, Some("provider_session"))`. After the change the agent shows `failed` with the sign-in message, its input stays `pending`, and no provider runs.

Focused: `cargo nextest run -p studio-runtime-server -p studio-runtime-services --lib --bin studio-runtime-server --test integration -E 'test(/wave7_p01_registry::/) | test(/account_auth_gate/) | test(/auth_hold::/) | test(/claude_preparation::/) | test(/provider_adapter::/) | test(/account_start_gate/)'`: 89 passed, 1 failed. The failure is `running_app_server_started_with_the_frozen_command_reattaches`, which needs `STUDIO_SUPERVISOR_BINARY` from the fast recipe and does not run in a plain nextest call. Clippy on both crates: no diagnostics in the changed files.

Full tiers: in the coordinator's batch gate.
