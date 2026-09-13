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

Review and test the patch against each supported live baseline. Install all
source files under an exclusive flock on `scripts/.studio-update.lock`.
Publish the manifest last, with the Python version used by the backend:

```sh
python3 scripts/codex-publish-update \
  --patch codex_message_intent_update.py \
  --id message-intent-v1 \
  --scope 'Message intent in the chat transcript'
```

Use `--scripts` to select an installed scripts directory. The command acquires
the publication lock, hashes the complete Python source set, and atomically
replaces `studio-live-update.json`. Installers must release their exclusive
lock before they run this command.

The updater holds a shared publication lock during validation and application.
An incomplete installation, unknown source, or unsupported Python version does
not apply a patch. Failed attempts retry with a bounded delay. A changed
manifest starts a new attempt without waiting for the previous retry delay.

## Verify

Read `live-update.json` in the existing state directory. The receipt records
the patch result or its error. New backend instances also expose this data in
`/api/desktop` under `liveUpdate`. A successful receipt belongs to its process.
After a restart, the updater checks the live implementation again.

`backendBuild` remains the source identity from process startup. A successful
patch receipt proves only the named patch scope. It does not claim that every
loaded module matches every installed file.

An existing backend needs a one-time installation of the updater. Later server
starts install it automatically. A successful update does not restart the
backend, close native connections, or change accepted message identities.

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
