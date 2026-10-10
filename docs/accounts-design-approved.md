# Approved design: Settings > Accounts and Settings > Servers (user approval 2026-10-08 20:37 UTC)

Style: match the accepted Settings > Servers style (existing ActionButton, SettingsSection/SettingsRow, compact rows, status badges, light/dark/mobile).

## Settings > Accounts (account-centric list, as today)

- Header row: "Accounts" and a primary "Add account" button.
- One row per account identity (provider + email): kind label (Codex / Claude), name, "Default" badge where relevant, ••• menu (Rename, Make default, Sign out, Remove).
- Second line: email · plan.
- Third line: server chips, one per paired server (and this Mac):
  - green "✓ <server>": signed in on that server;
  - yellow "⚠ <server> · sign in again": needs sign-in again (click starts re-sign-in on that server);
  - dashed "+ <server>": not signed in there; click starts sign-in of this account on that server (email hint = this account's email).
- Accounts on different servers are grouped by provider + email.
- Hint line under the list: "Green: signed in on that server. Yellow: sign in again. Dashed: click to sign in on that server."
- Offline servers: chip shows the server grey and disabled.

## Add account (dialog)

- Title "Add account". Choice cards: Codex (ChatGPT subscription, one-time code) / Claude (Claude subscription, link + paste code).
- Server selector (which server to sign in on; default: this Mac).
- Name field (shown in chats and the account picker). Email (optional) with hint "Studio checks that you sign in with this account."
- Cancel / Continue.

## Codex sign-in (dialog)

- Title "Sign in to Codex · <name>" (+ server).
- Step 1: open auth.openai.com/codex/device (Open button); hint: use a private window if the browser is signed in to another ChatGPT account.
- Step 2: large code with Copy button; "Expires in mm:ss".
- Live status line: "Waiting for sign-in on <server>…". Cancel.

## Claude sign-in (dialog)

- Title "Sign in to Claude · <name>" (+ server).
- Step 1: Open sign-in page (Open, Copy link); hint "Sign in as <email>".
- Step 2: paste code field (accepts code#state). Cancel / Finish sign-in.

## Errors

- Invalid code: "The code is not valid. Copy the full code from the Claude page, then paste it again."
- Wrong account: "You signed in as X. This account expects Y. Studio did not save the sign-in." Actions: Start again / Keep X.
- Expired: "The sign-in expired. Start a new sign-in to get a new link."

## Settings > Servers (cards)

- Top: auto-pair switch with one-line help, "Find servers now" button.
- Grid of server cards (2 columns desktop, 1 on mobile): name, status badge (Active / Unreachable / Discovered / Revoked), OS · host[:port], Accounts count by provider, Agents running, Seen (relative time). Actions on non-local cards: Remove, Revoke (with confirm); revoked: Allow again.
- Manual pairing collapsed at the bottom.

In a chat, the account picker shows only accounts of that chat's server.
