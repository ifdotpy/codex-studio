# Updates from a paired server

A Mac that runs Studio from `/Applications/Codex Studio.app` installs a newer build that a
paired server already runs. The paired servers then keep the same build, and their API schema
hashes stay equal ("Other version" does not stay on the server card).

## Build stamp

`package.ts` writes `Contents/Resources/studio-build.json`:

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

[`peer_updates.rs`](../workspaces/runtime/packages/runtime-services/src/peer_updates.rs) owns the
actions and their limits. `update.export` waits up to 5 seconds (`EXPORT_WAIT`) for the archive, so
that a request does not hold the 20-second shutdown grace of the server. After that it answers
`preparing`. The puller asks again every 10 seconds, for up to 20 minutes
(`EXPORT_POLL`, `EXPORT_PREPARE` in `peer_update_pull.rs`). The archive is at most 1 GiB
(`MAX_ARCHIVE_BYTES`). `update.release` deletes the archive when every server that exported it
released it. The server also deletes archives older than a day (`EXPORT_AGE`) when a puller asks
for an offer or an export. The receipt of an `update.chunk` keeps no archive data. Every action is a
read, so the puller uses a new request ID for each call.

An older peer refuses the actions. It answers "Supply a remote-parent link" (`not_applied`),
because it sends an unknown action to the remote-parent checks. The puller treats any refusal of
`update.offer` as no offer and records no failure.

The Rust backend does not serve a QUIC export stream, so a puller receives the archive in
`update.chunk` answers.

## Pull and verify (backend)

`peer_update_pull.rs` (runtime-services) runs in the Rust backend of the installed app. `runtime-server`
starts it only when the process runs from `/Applications/Codex Studio.app`. One timer task makes every
pass, 2 minutes after the start and then every 10 minutes. A pass runs only when the setting `autoUpdate` is on:

1. Ask each paired server that is online for `update.offer`.
2. Take the newest offer with `builtAt` greater than the local stamp. Skip a build that failed
   verification before (kept in `<state>/peer-update.json`).
3. Export, pull every chunk, and check each chunk hash and the whole file hash. The archive
   comes as `update.chunk` answers only. A stream over the QUIC link (`docs/peer-link.md`) is not
   built: the Rust backend serves no export stream route. Every step is an entry of the shared
   outbox (`RemoteEffect::UpdateOffer`, `UpdateExport`, `UpdateChunk`, `UpdateRelease`, restart rule
   `TargetDedupes`: the peer only reads its app). A chunk entry holds the archive offset, and the
   next chunk replaces it in one transaction. A paired server that does not answer leaves the
   entry; the next pass (or a restart) sends the same request ID again and continues at that
   offset. Chunks that were saved are not sent again.
4. Extract with `ditto -x -k` into a new private folder `<state>/updates/.extract-*`. The
   folder must then hold only `Codex Studio.app`. Another program can write into the
   `<builtAt>` folder at any time (Finder writes `.DS_Store` into a folder that a window shows),
   so the check uses a folder that no other program knows.
5. Verify the extracted app:
   - `codesign --verify --deep --strict`;
   - its designated requirement (`codesign -d -r-`) equals the one of the installed app, so the
     same certificate signed it;
   - its `studio-build.json` equals the offered stamp.
6. Move the app to `<state>/updates/<builtAt>/Codex Studio.app`. Write
   `<state>/pending-update.json`: `{id, app, build, from}`. Replace the last chunk entry with
   an `update.release` entry. It goes out once, and a paired server that is away gets it at the
   next pass. A failed pull releases the export in the same way.

Only one pull runs at a time, because one timer task makes the passes. A failure is recorded
with its reason in `peer-update.json` (`failures`, keyed by `builtAt:commit`; kept 24 hours) and
does not repeat the same build for 1 hour. `peer-update.json` also holds `checkedAt` and `lastPull`.
A failed install receipt (below) also skips that build
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
3. Note whether the desktop window runs, and close it. When `desktop-recovery.json` records
   `closeSignal: "SIGTERM"` with the PID, start time and executable of the running window of
   the installed app, send SIGTERM to that PID: the window closes through its save check, as for
   a user close. An older window gets the Apple Event quit (`osascript`, by bundle ID). AppKit
   can refuse that quit without user input ("User canceled. (-128)"), for example while a sheet
   or a modal panel blocks the main window.
4. `ditto` the staged app to `Codex Studio.next.app` and verify it. Verify the backend process
   against the installed app that started it: its packaged process reader, script path and,
   for Rust, executable hash. Then move the installed app to `Codex Studio.previous.app`
   (replace the older one) and move `.next.app` into place. After the move the installer
   imports no packaged module: the new build can need names that the old modules lack.
5. Send SIGTERM to the verified backend process when the same process (PID and start time)
   still reports this state directory and owns the listener. The recovery loop starts the new
   backend. If the backend cannot be verified, the new app stays in place and the old backend
   runs until its next restart.
6. Write `<state>/update-receipt.json` (`{id, build, from, at, outcome, reason, backend, window}`,
   with `installedAt` after an install), remove the pending file and the staged copy, and open
   the window again with `open -g` if it ran.
7. The recovery agent exits after an install. launchd starts it again from the new app.

A failed step before the swap leaves the installed app unchanged. A failed swap moves the
previous app back. After a failure the installer opens the window again if it quit it. The VM
guest is not upgraded: a VM upgrade restarts VM chats and needs the user's consent.

## Setting and status

`autoUpdate` is in the access settings next to `autoPair` (Settings, Servers). It is on by
default and stored in the table `runtime_peer_update_settings`. A `settings` request of
`POST /api/multi-server` sets `autoPair`, `autoUpdate` or both.

## Trust

Only a paired server can offer a build: the channel checks its Ed25519 signature and its
Tailscale owner. Only a build signed by the same certificate as the installed app is installed.
An app with changed files (for example a live source patch) fails the strict signature check, so
a peer that runs one cannot pass its build on.
