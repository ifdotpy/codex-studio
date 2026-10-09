# Move an existing installation to the workspace layout

This procedure stages the relocated source in a separate checkout, lets active
work finish on the existing backend, and then starts one backend against the
same state directory. Do not stop agents, monitors, terminals, or waves to make
the source move happen. Keep the existing backend and its service running while
the new checkout and dependencies are prepared.

## Prepare the new checkout

Create or update a separate checkout at the relocated revision. Install its
dependencies there from the repository root:

```sh
pnpm install --frozen-lockfile
```

Keep the existing state-directory path unchanged. Update service and installed
command references to the launchers in the new layout, while retaining the
temporary root `scripts/<installed command>` compatibility launchers needed
by existing installed links or service units. M-02.11 removes these shims; use
installed command names on `PATH` for new invocations.
New supervisor units should reference
`workspaces/runtime/apps/server/src/codex-supervisor`. Confirm launcher paths
and service configuration on the target platform before changing them; this
guide does not verify a live service installation.

## Hand off after work is idle

1. Keep the old backend serving the existing installation while work is active.
   Use read-only diagnostics and the existing idle checks to confirm agents,
   monitors, terminals, waves, queued events, and server operations have
   finished naturally. If work remains, leave the old backend in place.
2. Once idle, let the existing backend exit through its supported service or
   recovery path. Do not signal it as a shortcut. Confirm that the old process
   has exited and that the runtime lock and, when applicable, supervisor owner
   lease are free.
3. If either lock is held, or ownership is uncertain, do not launch the new
   backend and do not signal the owner. Leave the existing service configuration
   alone and retry after the current owner has exited.
4. Start exactly one backend from the relocated checkout using the same state
   directory. The desktop takes a nonblocking runtime lock before launching a
   backend. The supervisor has its own lock and can wait for its exact owner
   lease; do not interpret a waiting instance as permission to start another
   backend.
5. Verify `/api/diagnostics`, the state identity reported by `/api/desktop`,
   and the client UI before admitting new work.

## Why this needs an idle handoff

The backend source identity hashes sorted source-relative names and file bytes,
not absolute checkout paths. Moving otherwise byte-identical files therefore
does not change identity by itself. The relocated tree adds
`workspaces/runtime/apps/server/src/codex_layout.py`, so its complete source
identity does change, as it does for any changed source file.

Live updates require a manifest for the complete exact source set and hashes,
and apply only allowlisted patches matching loaded module paths and functions.
The old process keeps its original source root and update lock. A relocated
source tree is not a valid live patch for that process. Use the idle handoff
above instead; do not use live update to switch the checkout layout.

## Verification limits

The lock behavior and source-identity rules above were confirmed from the
current source by the runtime owner. This procedure has not been exercised
against a live installation or verified for every platform's service manager.
The exact service-edit and graceful-exit commands remain platform-specific and
must be checked with the owning operator instructions before use.
