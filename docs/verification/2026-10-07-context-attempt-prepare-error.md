# A local wait changed the context attempt comparison

## Cause

Context repair saved the full `startAttempt` before it waited for a native response.
After that wait exceeded its limit, Runtime added `prepareError` to the same attempt.
The late response then failed the full snapshot comparison.
No `turn/start` reached the model, and the original input stayed pending.

The baseline test reproduced this on commit `4a87471e`.
The strict snapshot comparison came from `4c3babfc`.
The actor, input, epoch, settings, and native identity still matched.
Only the new local wait string differed.

## Change

Ignore a new `prepareError` string only when the captured attempt had no such key.
Compare every other field with the original snapshot.
Reject a changed existing error, a non-string value, or any other field change.
Preserve the original actor and operation records.
Preserve connection, native generation, historical input, and Stop checks.

## Checks

- Focused attempt and late-fork contract: five pass.
- Context repair contract: 34 pass.
- Late callback contract: 11 pass.
- Native action contract: two pass.
- Private update contract: eight pass.
- Installed caller contract: 41 pass.
- Independent source and update review: pass.

The late-fork test sends the exact saved input once after one fork response.
The baseline fails both the added-string test and the late-fork test.
The test keeps all delivery and identity assertions.

The released context source SHA-256 is
`302570bc4034dd14c892a37a7a45f37f6f268b1a757f33638b9ed92735547869`.
The private release package is `/private/tmp/studio-context-attempt-20261007/`.

## Installed result

The update changed one existing function on backend PID `92215`.
Application receipt: `1791349794.23823` (UTC Unix time).
Manifest SHA-256: `2693ebd99b3f3fee7f2dd68251a0729d13ff6210d4966defe1a04708b097a876`.
The state directory, active agents, and original USDZ monitor were preserved.
The server still returned HTTP 200 after application.
The manifest and helper were retired, and the application signature passed verification.

The installed fixture proves the late-fork behavior.
No new repair was forced in a live chat for this check.
The separate USDZ user message remained delivered in its original accepted turn.
