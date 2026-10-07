# Context wait scope blocked a chat with a server monitor

The USDZ chat waited for monitor `f50bf690-29b4-5d58-a640-29973ca30630`.
The monitor kept the Next.js server on port 3217 alive.
The server returned HTTP 200 from `/healthz`.

A previous native history wait had scope `native`.
On retry, `claim_context_wait` found the active monitor and replaced the error text.
It kept the previous scope.
The existing path for normal input during background work requires scope `local`.
Thus the chat waited for a server that was meant to remain active.

The retry now copies scope from the new typed wait error.
An error without classification keeps the previous scope.
Exact input, account, epoch, action, submission, and unknown receipt checks remain in place.

The new regression fails on the previous source: `native` differs from `local`.
After the change, 47 context wait tests pass.
An independent source review passes six focused checks.
The private update passes seven update checks and eight installed caller checks.
An independent update review also passes those 15 checks.

The update changed only `claim_context_wait` on backend PID `92215`.
Its receipt time is `1791351743.797219` (UTC Unix time).
Its manifest SHA-256 is `bddcce1278ce1224213b46bb5a602adb737fa0698e70e22e11490ea286e59745`.
The installed context source SHA-256 is `46bc47ab8f9b66e76da5255f6594a7435aabf68a6cdac5c3deaa24da75c876a9`.

The exact pending event was delivered with its original start attempt and native thread.
The model replied and checked the server before considering a restart.
The monitor remained active, and server PID `43569` did not change.
No server restart or native fork was required.

Evidence stays in `/private/tmp/studio-context-wait-scope-20261007/`.
The manifest and helper were removed after the exact application receipt.
The application signature passed verification.
