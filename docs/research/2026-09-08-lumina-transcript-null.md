# Lumina transcript reconnect loop, 2026-09-08

The Lumina transcript request closed without an HTTP response. The server log
identified `RequestMixin.transcript_tool_result`: it iterated over a native
`dynamicToolCall` whose `contentItems` was null. The same exception stopped
`SyncStore.pull`. Repeated browser retries could not recover from this input.

The projection now scans content only when it is a list and ignores non-object
entries during the scan. It preserves the original payload and requires the exact
durable receipt before it reports tool completion. A missing receipt does not
become success.

The existing recovery regression failed before the change. All 11 transcript
contract tests passed afterward. Added cases exercise SyncStore with null, empty,
and malformed content, without changing stored evidence.

The live process received only the reviewed method replacement. Bytecode guards
matched the previous source. Account connections and executor pools were preserved.
No agent or monitor was restarted. The activation receipt is
`/tmp/studio-lumina-null-activation.json`.

After activation, the live Lumina transcript returned 120 items in 31 ms. Its sync
pull returned one document in 20 ms. These measurements establish recovery of
this failure, not the absence of unrelated connection defects.
