# Multi-server UI

The shell has one project list, message search, unread count, and notification list.
Each server has a label and a separate connection status.
The selected server view stays active.
A hidden idle view unloads after five minutes.
Selection restores its drafts, cache, and open chat from its own storage.
A view stays active during a turn, upload, dictation, or write.
A view also stays active when local draft or dictation storage fails.
Drafts, sends, uploads, caches, and API schema gates have a separate server identity.
The local server keeps its existing storage names.

## Automatic discovery

Open **Studio settings > Servers** to manage server access.
The local server appears first.
Each row shows its name, address, access status, and relative last contact time.
The full contact time appears in a tooltip.
Active servers appear before discovered, unreachable, and revoked servers.
The section uses the same tabs and fields as the other settings sections.

The local server discovers servers from the same Tailscale user.
**Pair servers automatically** changes that server's setting for new server pairs.
Existing credentials stay active when the setting is off.
**Find servers now** starts a bounded discovery pass.
The UI allows 105 seconds for that request, including the server's 90-second pass and cleanup.
A failed or interrupted request keeps its saved request identity.

The UI reads the local access snapshot every 30 seconds.
Offline failures increase the interval to a maximum of 120 seconds.
For a paired server without a UI credential, it requests `ui_invite` from the local server.
It then completes the normal signed UI pair flow without a user step.
The invitation must match the peer's server ID, origin, and public key.
Revoked servers cannot gain automatic UI access.
Server frames can open the Settings section but cannot perform its management actions.

The shell saves both request identities before it requests an invitation.
Its attempt store contains invitation metadata, without the token.
On recovery, `ui_invite` returns the same invitation through its stable request identity.
Client keys and pair bodies remain in the existing credential stores.
The browser credential store uses IndexedDB. Desktop credentials use native safeStorage.
Startup upgrades the attempt store before it checks any peer state.
The upgrade removes legacy tokens even for revoked, excluded, or registered peers.
It checks the existing credential store for each old pair identity.
Only a saved credential draft or pair receipt marks a legacy pair as started.
If that check fails, the token is already removed and automatic access waits.
Saved attempts use the local server ID and peer credential generation.
A lost response keeps the same invitation, client key, client ID, body, and request identity.
An expired invitation gets a new attempt only when pairing has not started.
An unknown pair result keeps its identity even after invitation expiry.
Web Locks serialize automatic access across browser tabs.
Management actions use another shared lock and separate request records.
An uncertain request blocks a different management action until its result is known.
A response removes only its own request record.
The shell reads the current peer snapshot before pairing and before adding UI access.
Revoke cancels automatic access after the management lock accepts the request for dispatch.
A blocked request does not cancel access.
**Add to this UI** clears a local cancellation and retries the normal access flow.
Frames cannot send management writes or use pairing routes through either transport bridge.
A failed server attempt backs off for up to five minutes.
Other servers can still gain access.

**Remove from this UI** deletes its credential and saves an exclusion in this UI profile.
Automatic access does not add that server again.
**Add to this UI** clears the exclusion and retries the normal flow.
Removal does not revoke the server pair.
**Revoke** asks for confirmation and revokes the peer credential on the local server.
**Allow again** removes that revocation. A new discovery pass must prove the peer.
These actions apply only to the local server's peer records.
Use the target server's UI access controls to revoke a particular UI client.

UI-only hosts and phone pages through Serve keep manual invitation pairing.
They do not call local discovery or `ui_invite`.
A UI-only host has no local server or automatic server setting.

## Pair a UI

1. Open **Studio settings > Servers** on the target computer.
2. Select **Add with an invitation** to open **Manual pairing**.
3. Select **Create pairing invitation**.
4. Open **Studio settings > Servers** on the UI computer.
5. Select **Add with an invitation**.
6. Enter the target Tailscale Serve HTTPS address.
7. Paste the whole invitation.
8. Select **Pair server**.

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
Removal stops the frame's streams, subscribers, and transcription.
Navigation and frame destruction also stop transcription and remove its listeners.
A duplicate stream identifier cannot remove the original stream's cleanup record.

A browser shell on loopback also uses a separate origin for each server view.
Browser keys stay in the shell's IndexedDB.
Frames receive no key.
A message broker matches the live frame window, its origin, and its server before each request.
The browser broker refuses requests for another server.
Preferences and theme cross these origins through validated messages.

The shell polls `GET /api/ui-summary` for unloaded servers every 30 seconds.
Each poll has a 15-second deadline.
Offline failures increase the interval to a maximum of 120 seconds.
The response contains projects, chats, unread flags, notification summaries, and a busy flag.
The endpoint reads the durable entity projection without changes to read receipts.
Remote polls use the same paired signature boundary.
The native bridge permits only this exact method and path for shell reads.
It rejects query strings, fragments, request bodies, and calls from server frames.
The browser shell builds the same fixed request.
Its frame broker rejects summary actions.
Search restores unloaded views and waits for their stores before it sends the search command.

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
pnpm --filter codex-agents-web run build
pnpm --filter codex-agents-desktop run start:ui
```

The UI-only host serves assets on `127.0.0.1:4621`.
Set `CODEX_UI_PORT` for another port.
Set `CODEX_DESKTOP_PROFILE` for a separate native profile.
The host rejects local API requests and an occupied port.
It starts no Python backend or recovery service.

Create a package outside the checkout:

```sh
CODEX_DESKTOP_PACKAGE_OUT=/tmp/studio-ui-package pnpm --filter codex-agents-desktop run package:ui
```

The package has an explicit UI-only marker.
It includes Electron, the web assets, and the speech helper.
It excludes the Python runtime source, backend recovery helper, role skills, and Linux VM helper.
The existing package command still creates the combined application.

Run a hidden check with a separate profile and an ephemeral port:

```sh
node workspaces/client/apps/desktop/ui-only-smoke.mjs
CODEX_UI_EXECUTABLE='/path/to/Codex Studio.app/Contents/MacOS/Codex Studio' node workspaces/client/apps/desktop/ui-only-smoke.mjs
```

## Verification

The browser fixture verifies Ed25519 signatures and rejects reused nonces or remote canvas tokens.
It uses identical workspace, project, and chat identifiers across three servers.
The test covers a lost pair response across reload, draft isolation, send ownership, merged search, reconnect, notifications, unread counts, theme, shortcuts, and mobile controls.
The native checks use hidden windows and separate state directories and ports.

Product measurements on 2026-10-08 use the hidden packaged UI-only Electron application.
Each run has one chat per server and a separate native profile.
The process measurement uses `ps` after 60 seconds without a debugger on the server workers.
Resident memory includes all Electron helper processes, including renderers.
It excludes the main process and test runner.
The native preload disables service workers in server views.
The measurement confirms zero running service workers.
The fixture replaces key storage only in its disposable profile.
Actual native Ed25519 signatures authenticate each HTTP request.

| Servers | Mounted helper memory | Idle helper memory | Mounted renderer count | Idle renderer count |
| ------- | --------------------- | ------------------ | ---------------------- | ------------------- |
| 2       | 703,610,880 bytes     | 508,592,128 bytes  | 3                      | 2                   |
| 3       | 846,446,592 bytes     | 509,427,712 bytes  | 4                      | 2                   |

Idle removal releases 195,018,752 bytes with two servers and 337,018,880 bytes with three servers.
The selected frame and shell keep two renderer processes.
Run the same measurement against a package:

```sh
CODEX_UI_EXECUTABLE='/path/to/Codex Studio.app/Contents/MacOS/Codex Studio' node workspaces/client/apps/desktop/multi-server-memory.mjs
```

Browser diagnostics use Chromium with debugger sessions attached to each renderer.
The reload measurement ends when all merged chat rows appear.
The heap measurement sums the shell and separate server renderer processes.
Resident memory includes all processes of the test browser.

| Servers | Reload to chat rows | Mounted JavaScript heap | Idle JavaScript heap | Mounted browser memory | Idle browser memory |
| ------- | ------------------- | ----------------------- | -------------------- | ---------------------- | ------------------- |
| 2       | 1128 ms             | 70,024,536 bytes        | 50,895,896 bytes     | 1,781,481,472 bytes    | 1,787,920,384 bytes |
| 3       | 669 ms              | 87,013,724 bytes        | 55,883,176 bytes     | 2,150,973,440 bytes    | 2,153,906,176 bytes |

Debugger sessions keep browser service worker renderer processes alive after frame removal.
These browser numbers do not measure the packaged product's memory reduction.
The browser test also checks unread updates and notifications after removal.
It checks draft restoration and blocks removal when both draft stores reject writes.

The browser fixture uses isolated loopback servers behind a test-only HTTPS address adapter.
The signed path, query, and body remain unchanged.
The final server branches must be integrated before a live Tailscale check.
Regenerate the combined API schema after that integration.

Check the final server's export header list after integration:

```sh
python3 workspaces/runtime/apps/server/tests/multi-server-ui-cors-contract.py
```
