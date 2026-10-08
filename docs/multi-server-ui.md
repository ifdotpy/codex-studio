# Multi-server UI

The shell has one project list, message search, unread count, and notification list.
Each server has a label and a separate connection status.
Chat selection keeps each server view active.
Drafts, sends, uploads, caches, and API schema gates have a separate server identity.
The local server keeps its existing storage names.

## Pair a UI

1. Open **Studio settings > Server access** on the target computer.
2. Select **Create pairing invitation**.
3. Open **Servers** or **Manage** on the UI computer.
4. Enter the target Tailscale Serve HTTPS address.
5. Paste the whole invitation.
6. Select **Pair server**.

A phone at its configured Tailscale Serve address keeps the existing session and local token flow.
Only the explicit `studio-ui-only=1` flag selects the UI-only manager.
The invitation grants full access to that server.
**Remove from this UI** deletes this UI's key.
Use **Revoke access** on the server to remove the server's permission.

A lost pair response keeps the same private key, body, client identity, and request identity.
Enter the same invitation after a reload to retry that attempt.
Unresolved API writes also keep their request identities.
A definite pre-handler HTTP failure releases an implicit identity for the next user action.
A network failure, timeout, server error, expired receipt, or earlier uncertain attempt keeps the identity.
Ordinary remote POST requests have a 15-second default deadline.
Each retry has a new timestamp, nonce, and signature.

Browser private keys use nonextractable WebCrypto keys in IndexedDB.
Desktop private keys use Electron safeStorage in the native profile.
The renderer receives no desktop private key.
Remote API requests use the v1 signature contract.
Remote event streams use signed fetch requests.
Local canvas tokens remain local.

Each desktop server view has a separate `studio-<base32-server-id>.localhost` origin.
The preload sends requests directly from that frame.
The main process derives the owner from `event.senderFrame`, its URL, and its parent.
Caller arguments cannot change that owner.
A frame cannot reach the shell bridge or a sibling frame.
Native keys and signatures stay in the main process.
The shell can pair or remove servers, but cannot request signatures for a frame.
Removal stops the frame's streams and subscribers.

A browser shell on loopback also uses a separate origin for each server view.
Browser keys stay in the shell's IndexedDB.
Frames receive no key.
A message broker matches the live frame window, its origin, and its server before each request.
The browser broker refuses requests for another server.
Preferences and theme cross these origins through validated messages.

A browser shell on a non-loopback host cannot serve these loopback origins to another computer.
Its frames keep the shell origin.
The message broker still checks request ownership.
This does not isolate a malicious script from the shell or its IndexedDB keys.
Use the desktop application or a loopback browser shell when script isolation between servers is required.
Remote file views use the remote API.
**Save As** can save an explicit remote download on the UI computer.
Finder actions remain available for the local server.

## UI-only desktop

Build the web assets first.

```sh
npm --prefix web run build
npm --prefix desktop run start:ui
```

The UI-only host serves assets on `127.0.0.1:4621`.
Set `CODEX_UI_PORT` for another port.
Set `CODEX_DESKTOP_PROFILE` for a separate native profile.
The host rejects local API requests and an occupied port.
It starts no Python backend or recovery service.

Create a package outside the checkout:

```sh
CODEX_DESKTOP_PACKAGE_OUT=/tmp/studio-ui-package npm --prefix desktop run package:ui
```

The package has an explicit UI-only marker.
It includes Electron, the web assets, and the speech helper.
It excludes the Python runtime source, backend recovery helper, role skills, and Linux VM helper.
The existing package command still creates the combined application.

Run a hidden check with a separate profile and an ephemeral port:

```sh
node desktop/ui-only-smoke.mjs
CODEX_UI_EXECUTABLE='/path/to/Codex Studio.app/Contents/MacOS/Codex Studio' node desktop/ui-only-smoke.mjs
```

## Verification

The browser fixture verifies Ed25519 signatures and rejects reused nonces or remote canvas tokens.
It uses identical workspace, project, and chat identifiers across three servers.
The test covers a lost pair response across reload, draft isolation, send ownership, merged search, reconnect, notifications, unread counts, theme, shortcuts, and mobile controls.
The native checks use hidden windows and separate state directories and ports.

Measurements on 2026-10-08 use the production renderer, Chromium, and one chat per server.
The reload measurement ends when all merged chat rows appear.
JavaScript heap measurements sum the shell and each separate renderer process for the persistent server frames.
The resident memory measurement includes all processes of the test browser.
It excludes Electron and a real Tailscale network.

| Servers | Reload to chat rows | JavaScript heap  | Browser resident memory | Server frames |
| ------- | ------------------- | ---------------- | ----------------------- | ------------- |
| 2       | 665 ms              | 61,229,868 bytes | 1,722,302,464 bytes     | 2             |
| 3       | 1398 ms             | 83,799,756 bytes | 2,045,280,256 bytes     | 3             |

The third server adds 322,977,792 bytes of browser resident memory in this run.
Separate origins require separate renderer processes in Chromium.

The browser fixture uses isolated loopback servers behind a test-only HTTPS address adapter.
The signed path, query, and body remain unchanged.
The final server branches must be integrated before a live Tailscale check.
Regenerate the combined API schema after that integration.

The full resource event unit suite has two baseline failures in this image.
The same failures occur with the original `origin/main` resource event source.
The supervisor recovery fixture also times out without changes to its source.
The runtime type check reports five errors in three unchanged files.
These failures remain outside this UI change.

Check the final server's export header list after integration:

```sh
python3 tests/multi-server-ui-cors-contract.py
```
