# Updates from a paired server

A Mac that runs Studio from `/Applications/Codex Studio.app` installs a newer build that a
paired server already runs. The paired servers then keep the same build, and their API schema
hashes stay equal ("Other version" does not stay on the server card).

## Build stamp

`package.mjs` writes `Contents/Resources/studio-build.json`:

```json
{
  "version": 1,
  "builtAt": 1791980000.0,
  "commit": "<git sha>",
  "commitTime": 1791979000,
  "branch": "release/vault-menu"
}
```

`builtAt` is the packaging time on the build Mac. A build is newer when its `builtAt` is
greater. Only a backend that runs from `/Applications/Codex Studio.app` uses the stamp. A
backend from a source checkout or from another copy of the app does not offer or take updates.

## Offer and transfer

The peer channel is the signed orchestration channel (`/api/servers/orchestration`). Four
actions, all read-only on the offering server except their export file:

| Action           | Parameters         | Result                                                                                                                                                                |
| ---------------- | ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `update.offer`   | none               | `{build}` (the stamp) or `{build: null}`                                                                                                                              |
| `update.export`  | `{builtAt}`        | `{export, size, sha256}`: a `ditto -c -k --keepParent` archive of the installed app in `<state>/server-exports/`, made once per build; or `{export, preparing: true}` |
| `update.chunk`   | `{export, offset}` | `{offset, nextOffset, data, sha256}`, 512 KiB of data                                                                                                                 |
| `update.release` | `{export}`         | `{released: true}`                                                                                                                                                    |

`codex_peer_updates.py` owns the actions and their limits. `update.export` waits up to 90 seconds
for the archive; after that it answers `preparing` and the puller asks again every 10 seconds, for
up to 20 minutes. The archive is at most 1 GiB. `update.release` deletes the archive when every
server that exported it released it; the hourly prune deletes archives older than a day. The
receipt of an `update.chunk` keeps no archive data. Every action is a read, so the puller uses a
new request ID for each call.

An older peer refuses the actions. It answers "Supply a remote-parent link" (`not_applied`),
because it sends an unknown action to the remote-parent checks. The puller treats any refusal of
`update.offer` as no offer and records no failure.

## Pull and verify (backend)

`codex_peer_updates.py` runs in the backend of an installed app. Every 10 minutes, and 2 minutes
after the start, when the setting `autoUpdate` is on:

1. Ask each paired server that is online for `update.offer`.
2. Take the newest offer with `builtAt` greater than the local stamp. Skip a build that failed
   verification before (kept in `<state>/peer-update.json`).
3. Export, pull every chunk, and check each chunk hash and the whole file hash.
4. Extract into `<state>/updates/<builtAt>/` with `ditto -x -k`.
5. Verify the extracted app:
   - `codesign --verify --deep --strict`;
   - its designated requirement (`codesign -d -r-`) equals the one of the installed app, so the
     same certificate signed it;
   - its `studio-build.json` equals the offered stamp.
6. Write `<state>/pending-update.json`: `{id, app, build, from}`. Release the peer export.

Only one pull runs at a time. A failure is recorded with its reason in `peer-update.json` and
does not repeat the same build for 1 hour. A failed install receipt (below) also skips that build
for 1 hour. A pending update for a build that is not newer than the installed app is removed.
The backend never replaces the installed app itself.

## Install (recovery LaunchAgent)

`recover_backend.py` checks `pending-update.json` on its ticks, at most once a minute, and runs
`install_update.py` from the installed app as a separate process. The installer exits with 0
(installed), 75 (waiting, try again later) or 1 (failed or obsolete; the pending file is removed):

1. Wait while `supervisorEnabled` is not true: without the supervisor, a backend restart ends
   the agents. Wait while a VM upgrade holds `linux-vm/maintenance.lock`; hold that lock for the
   rest of the install.
2. Stop as obsolete when the installed app is this build or a newer one. Verify the staged app
   again, as in step 5 above.
3. Note whether the desktop window runs. Quit it (`osascript`, by bundle ID).
4. `ditto` the staged app to `Codex Studio.next.app`, verify it, move the installed app to
   `Codex Studio.previous.app` (replace the older one), move `.next.app` into place.
5. Send SIGTERM to the verified backend process. The recovery loop starts the new backend. If
   the backend cannot be verified, the new app stays in place and the old backend runs until its
   next restart.
6. Write `<state>/update-receipt.json` (`{id, build, from, at, outcome, reason, backend, window}`,
   with `installedAt` after an install), remove the pending file and the staged copy, and open
   the window again with `open -g` if it ran.
7. The recovery agent exits after an install. launchd starts it again from the new app.

A failed step before the swap leaves the installed app unchanged. A failed swap moves the
previous app back. After a failure the installer opens the window again if it quit it. The VM
guest is not upgraded: a VM upgrade restarts VM chats and needs the user's consent.

## Setting and status

`autoUpdate` is in the access settings next to `autoPair` (Settings, Servers). It is on by
default and stored in the table `runtime_peer_update_settings`. `ServerDiscovery.__init__` in
`codex_server_discovery.py` creates that table with the other access tables. A separate table
keeps `runtime_access_settings` equal to the Rust store schema. A `settings` request of
`POST /api/multi-server` sets `autoPair`, `autoUpdate` or both. `GET /api/desktop` shows
`peerUpdate`: the local build (`build`), the time of the last check (`checkedAt`), the last pull
(`lastPull`), the staged update (`pending`) and the last install (`lastInstall`), each with its
outcome and reason.

## Trust

Only a paired server can offer a build: the channel checks its Ed25519 signature and its
Tailscale owner. Only a build signed by the same certificate as the installed app is installed.
An app with changed files (for example a live source patch) fails the strict signature check, so
a peer that runs one cannot pass its build on.
