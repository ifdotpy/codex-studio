# Pending browser recovery blocked saved input

## Cause

The USDZ lead kept two events pending while its web-server monitor ran.
Browser recovery waited for that monitor to stop.
Dispatch and context preflight waited for browser recovery.
The monitor had no stall timeout and could run indefinitely.

The browser operation had stage `pending` and no submitted native request.
Its identity was `287ebc21-8f46-4acd-9f15-356eff6d5153`.
The lead was `b080ae6b-fc93-40cf-892a-67cf42d99c5f`.

## Change

- Allow saved input while browser repair waits before native submission.
- Give current input priority over optional browser repair.
- Keep `reconnecting` and submitted native requests blocked until their receipt is known.
- Let an exact successful browser discovery verify an unsubmitted pending repair.
- Keep the agent runnable when browser configuration is unavailable before any native call.
- Make the displayed queue reason match the dispatch rule.

The reconnect stage is saved before the native request starts.
It therefore preserves the hold if submission occurs before the request record reaches SQLite.
The change does not replay a browser action, user message, or monitor command.

## Checks

- Existing browser contract: 18 pass.
- New pending-command contract: 10 pass.
- Context repair contract: 34 pass.
- Private update contract: nine pass.
- Installed caller contract: 10 pass.
- Independent source and update review: pass.

The old dispatch reserved zero events in the causal control.
The new dispatch delivered both exact event identities through one native turn.
The context preflight control also rejected pending browser repair before the change.

The separate late-fork contract failed on the unchanged baseline.
Its `prepareError` snapshot mismatch was fixed in the
[context attempt check](2026-10-07-context-attempt-prepare-error.md).

## Live result

The update reached backend PID `92215` without a restart.
The state directory remained `/Users/igor/.local/state/codex-agents`.
Application receipt: `1791349022.588844` (UTC Unix time).
Manifest SHA-256: `e21edb4def6871332c4c6ec77bccc52f77993e104a721b95534544fb7b4d8bb1`.

These existing events became `delivered` in turn `01a114b8-e6a6-7380-95d6-fc6f5c27104b`:

- User message: `3c3a2905-0130-441d-91a1-fc232674d05a`.
- Monitor result: `monitor:d4063a0f-bf57-5e6e-bbd3-8286a9b80dd1`.

The lead then produced assistant output and native command results.
The original web-server monitor `f50bf690-29b4-5d58-a640-29973ca30630` remained `running`.
The update manifest and helper were retired after the exact application receipt.
The application signature passed verification.

This proves that the chat resumed.
It does not prove that the FieldView scene or native browser connection works.

Private receipts are in `~/.local/state/codex-agents/diagnostics/usdz-queue-20261007/`.
The version-specific release package is `/private/tmp/studio-browser-recovery-live-20261007/`.
