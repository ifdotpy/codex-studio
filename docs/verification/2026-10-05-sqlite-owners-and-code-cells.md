# SQLite owners and code-mode cells

## The 282-second transaction

The saved diagnostic identifies `Runtime.db`, process 97075, and a maximum
transaction duration of 282180.956666 milliseconds. The snapshot time is
2026-10-02 19:04:34 UTC. It is a completed transaction maximum, not a current
transaction timer.

The source at that time records only the site, maximum duration, and 256 duration
samples. It records no transaction identifier, start time, owner, or SQL statement
type. Commit `98b9aeed` adds owner records after this snapshot. The saved maximum
cannot identify the old owner. It also cannot prove that this transaction blocked
another writer.

Local evidence: `/private/tmp/studio-monitor-terminal-live-before.json`,
`diagnostics.sqliteContention.transaction.sites.Runtime.db.longestTransactionMs`.
The current log contains `database is locked` errors, but they contain no matching
transaction identifier.

## Loss of newer owner records

The previous implementation stores one current journal and one previous journal.
Each new nonempty backend session replaces the previous journal. A third session
removes the first session's owner records.

The regression uses three private processes. Session A records a transaction of
282180.957 milliseconds. Sessions B and C record shorter transactions. The old
implementation loses A. Commit `1b8e0e2f` preserves A in the history archive.
This test uses a simulated clock and private databases. It does not reproduce the
old live transaction.

The fix retains three archive snapshots and preserves the current and previous
journal names. It preserves exact bytes, owner identifiers, and frame locations.
Each file has a 64 MiB limit. The archive count is bounded. A failed archive or
rotation preserves the original journals for retry.

Verification: 13 owner-history cases, five SQLite thread-safety cases, and six
private update cases pass. The old implementation fails the three-session case.
The update changes only `_archive_previous.__code__`. The persistence lock
prevents an old journal operation from crossing publication.

## Code-mode cell capacity

The selected native release is Codex 0.160.0. Its code-mode host binary has SHA256
`679eedaea70529aa1cffc9bc0a0788c186412663544fa76c09d63b57f383a65a`.
The official release source is commit
`a956835d020762cb2b570053af06f643a11c0ecc`.

One host permits 128 cells across its sessions. The host rejects a new execution
before JavaScript starts if no permit remains. This explains why a fresh agent
can receive the error while other sessions hold the capacity.
See the [host limit and rejection](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/code-mode-host/src/lib.rs#L418).

After a yield, a cell can finish JavaScript and retain its result without an
observer. The runtime keeps this cell in the `Completed` phase. The host retains
the permit until `CellClosed` or disconnect. Thus the capacity includes completed
results that the agent did not collect through a final `wait`.
See [completion without an observer](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/code-mode-runtime/src/cell_actor/types.rs#L224)
and the [permit lifetime](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/code-mode-host/src/peer.rs#L418).

An ordinary turn completion does not close these cells. Native unsubscribe removes
the client's subscription. It does not confirm immediate session shutdown. The
native idle timeout is 60 seconds by default. Studio checks its tracked idle
agents for release after 900 seconds. These delays can extend retention. The old
incident does not contain the identities of all 128 cells.

## Native binary reproduction

Run the standalone fixture with the selected host binary:

```sh
python3 docs/verification/code-mode-cell-retention.py --host /path/to/codex-code-mode-host
```

The fixture checks the exact binary digest before it starts a private host. It
uses two private sessions and a pure fake delegate. It makes no model request and
sends no command to a live Studio process.

Observed results:

- Two sessions accept 128 finite executions.
- Execution 129 returns `code-mode host has too many active cells`.
- A final `wait` returns `Result` and permits another execution.
- All 128 finite executions return `Result` without an execution error.
- Termination and session shutdown also release permits.

The recorded fixture completes in 0.492 seconds. This proves the capacity and
retention mechanism on the selected binary. It does not identify the live cells
that occupied the host during the original incident.

## Studio observation boundary

This rejection becomes a `CustomToolCallOutput` with `success=false`. It does not
become a native turn failure. The native server exposes this path through
`rawResponseItem/completed` only with `experimentalRawEvents`. Studio does not
enable that option. An observer for an ordinary `mcpToolCall` completion cannot
reliably detect this error.
See [failure conversion](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/core/src/tools/parallel.rs#L303)
and [raw event selection](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/app-server/src/request_processors/thread_lifecycle.rs#L329).

A native `idle` status does not prove that every code-mode cell finished. Forced
cleanup can cancel a cell that still waits for a tool result. The diagnosis does
not authorize the replay of unknown commands or messages. No live native session
was reset or stopped for these checks.
