# Native browser discovery and recovery

Status: recovery verified; original Twoj Startup discovery cause unresolved.

## Confirmed behavior

Twoj Startup thread `01a08a79-ea43-7f10-af88-220f16226188` repeatedly returned
`Browser is not available: chrome`, including a fresh explicit probe. A separate
new Studio session could connect. Unsubscribing and resuming the idle Twoj thread
with current configuration restored Chrome. The same thread then read its existing
authenticated registration tab and emitted a screenshot.

Codex `rust-v0.153.4` source, `thread_processor.rs::resume_running_thread`, explains
why merely sending new configuration is insufficient: a subscribed loaded thread
can ignore resume overrides. An idle thread without subscribers can be replaced
by a fresh session that reads its saved history. This does not create a new chat.

The installed `browser-service.mjs` refreshes discovery on browser selection and
list commands. Repeated selection does not establish that an empty browser list
was cached permanently. That hypothesis is not a confirmed cause.

The retained native logs for the affected thread and process contain no matching
low-level native-pipe failure details for the incident window. The original
connection failure is not diagnosed. A future occurrence needs the native transport
failure details correlated with the recorded thread, item, connection, and request
IDs. Do not close that root-cause investigation based only on successful recovery.

## Implemented recovery

`codex_browser_recovery.py` observes completed native `node_repl.js` calls. Only
exact failed discovery results trigger recovery. Successful text containing an
error quote, generic timeouts, site permission denials, unrelated MCP servers,
and stale turn events do not trigger it.

Recovery waits until the turn, active commands, monitors, and workspace operations
end. A pending recovery prevents the scheduler from starting the next turn first.
Two dedicated recovery threads are allowed; native waits do not occupy the
coordination/status executor.

The recovery sends native `thread/unsubscribe` and `thread/resume` with current
settings and a connection-generation instruction. Native request IDs and response
receipts are stored in the agent record. Callback events drain before Studio marks
the thread loaded again. Epoch, account, thread, and connection checks prevent a
late result from resuming stopped or transferred work.

A new diagnostic event asks the same model thread to verify browser access with
read-only operations. It does not replay the previous tool or user message. Old
JavaScript bindings may need recreation. Existing browser actions with unknown
outcomes must be checked, not repeated.

Successful native Chrome discovery marks recovery verified. A failed verification
stops the automatic loop. Unknown reconnect responses hold further dispatch and
preserve the exact request ID. A process restart does not replay a persisted
incomplete recovery. Explicit disable settings remain effective.

## Verification

97 Python contract tests passed:

- Browser recovery: 15.
- Browser configuration: 13.
- Runtime: 50.
- Account isolation: 11.
- Role skills: 8.

Actual Studio test agent: `319edf6f-0f38-437d-af5c-3427e41c8bc1`.
Native thread: `01a08b24-caae-75f0-8dac-fb0d6bb0aec4`.
Account: Lumina.

The first test deliberately excluded Chrome in the initial native thread's backend
configuration. The real model received a discovery error, ended its turn, and
recovered automatically in the same thread. No example.com tab was available in
that first check, so it proved discovery and list access, not page preservation.

For the second check, the agent created a task-owned example.com tab and marked it
as a deliverable. Test-only configuration then excluded Chrome from that existing
idle thread. The next actual model turn failed discovery. Automatic recovery
`9e20982d-4aab-4a12-8c5a-11194fd2e499` resumed the same thread; native resume
request `41` received its result. The model read tab `1455651055` with URL
`https://example.com/` and title `Example Domain`. The final check made no page
changes, navigation, or submissions. Test configuration hooks were removed.

This was controlled configuration fault injection. It proves the automatic
recovery path; it does not reproduce the unknown original native transport cause.
The final executor-isolation change is covered by contract tests. The preceding
live check covers the unchanged native request and verification sequence.

The update was installed in the running Studio server. Its four account
connections remained unchanged. Twoj Startup and other user agents were not
restarted for this update. Runtime evidence stays in the external maintenance
state directory, not in the repository.
