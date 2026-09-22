# Live updates

Studio checks the installed live update manifest every two seconds. A separate
worker applies the reviewed patch. HTTP requests and agent connections continue.
Chats do not show a permanent server update notice.

The manifest identifies one patch and the exact source files it requires.
The patch must validate the supported live methods before any change. It must
preserve existing objects and callbacks, reject unknown implementations, and
accept a repeated call without repeating work. A patch cannot send messages,
restart agents, or repeat external operations as part of an update.

## Publish

Create one temporary patch for the current release outside the repository.
Review and test it against the exact running backend. Do not ship previous
release patches or chains of historical implementations.

Copy the patch and current source into the release scripts directory under an
exclusive flock on `scripts/.studio-update.lock`. Release that lock, then
publish the manifest with the Python version used by the backend:

```sh
python3 scripts/codex-publish-update \
  --scripts /path/to/release/scripts \
  --patch codex_release_update.py \
  --id current-release \
  --scope 'Exact methods and behavior changed by this release'
```

The command acquires the publication lock, hashes the complete Python source
set, and atomically replaces `studio-live-update.json`. Sign the application
after publication, then install the complete artifact.

The updater holds a shared publication lock during validation and application.
An incomplete installation, unknown source, or unsupported Python version does
not apply a patch. Failed attempts retry with a bounded delay. A changed
manifest starts a new attempt without waiting for the previous retry delay.

## Verify

Read `live-update.json` in the existing state directory. The receipt records
the patch result or its error. New backend instances also expose this data in
`/api/desktop` under `liveUpdate`. A successful receipt belongs to its process.
Check the receipt's process identity, release identifier, status, and scope.
Verify the changed behavior before removing the temporary deployment files.

After verification, remove the manifest and temporary patch together under the
exclusive publication lock. Sign the application again after that change.
Keep the receipt in the state directory. New backend processes use the current
source directly and do not need an old deployment patch.

`backendBuild` remains the source identity from process startup. A successful
patch receipt proves only the named patch scope. It does not claim that every
loaded module matches every installed file.

The backend starts the updater automatically. A successful update does not
restart the backend, close native connections, or change accepted message identities.

## Limits

This mechanism applies reviewed live patches. It does not reload arbitrary
Python modules. A changed constructor, object layout, transport, or database
schema needs its own validated transition. Without that transition, the
updater records an error and preserves the running work.

Long-running frames can finish their previous implementation. A patch must
account for that behavior before it can be published. A failure after an
external operation cannot be repaired by an automatic replay.

Run `python3 -B tests/live-updates-contract.py` and `node desktop/test.mjs`
after changes to this mechanism. The desktop check uses an isolated state
directory and a hidden window.
