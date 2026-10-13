# Notification differential corpus

`notification_dispatch_corpus.rs` and the lifecycle runner share `oracle.py`.
Notification seed and transition hooks live in `notification_dispatch.py`;
`notification_cases.json` is the durable input corpus. No provider is launched.
The oracle installs its provider-home and network guard before importing Runtime,
then reexecutes with the HTTP harness's disposable environment. The guard test
checks open, stat, lstat, listdir and scandir against both forbidden homes.

Run the default witnesses with an equipped, read-only `CODEX_AGENTS_PYTHON`:

```sh
cargo nextest run -p studio-runtime-services --test notification_dispatch_corpus
```

Run the full matrix explicitly as a manual heavy run:

```sh
just test-rust-runtime-corpora
```

Both commands require disposable `HOME`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, cache
and target directories. Remove provider-home directories from PATH and unset an
inherited `CODEX_BIN`. The oracle further isolates its own process. Never point
these tests at an existing runtime state directory. `NOTIFICATION_CASE` can select
an id substring; a filter matching nothing fails. `NOTIFICATION_CORPUS_REPORT`
selects the JSON field-difference report outside the checkout.

## Contract and fixtures

The JSON-lines seed command carries `command`, Python `root`, `rust` root and a
`case` with `module: "notification_dispatch"`. Its response contains `before` and
`after` snapshots. The snapshot command reads a committed Rust root through the
same Python normalizer. A close command drains the isolated oracle.

Python initializes the schema and creates the baseline agent once. Seed classes
are keyed only by state-producing fields; real Runtime.put/enqueue/item writers
prepare each class. SQLite memory backups retain those Python-written pre-states.
Each case restores one pre-state, resets the decoded agent cache and stream buffer,
and backs it up to a distinct Rust root. The guarantee is SQLite backup equivalence:
the complete, unnormalized SQL dumps must match, including stored JSON text and
tables outside the comparison inventory. Database-file bytes may differ because
SQLite backup changes bookkeeping. No Rust-written row becomes an expected
fixture. Snapshots compare all selected rows, JSON business fields, absence/null,
item creation order, full text, event links, receipt signatures/results, canonical
agent sync payloads and hashes, legacy search rows and next-index token postings.
The raw sync hash is validated before hashing the normalized payload. Every opaque
time/revision field is checked before replacement: finite nonnegative clocks or
valid timezone-aware ISO strings, and nonnegative integer revisions. Rust must
preserve the Python field's stored shape. Invalid input fields remain unnormalized
so refusal witnesses can still compare them. Changed sync content must advance its
revision; revisions cannot go backwards. These services preserve legacy
`sync_versions` rows exactly, including their validated SHA-256 hash strings.

Submissions run real Runtime.start until immediately before Runtime.item. The
captured call is then applied on Python while Rust receives PreparedSubmission on
the cloned boundary state. `submission_boundary.json` lists and checks the settings,
lifecycle and metadata fields already written at that point. Lifecycle owns the
earlier preparation; notification owns the item, projection links and receipt.

Notifications use real Runtime.notification and StreamBuffer with manual timers.
D/S/C means delta/start/completion; S carries the snapshot. There are 96 permutation
cases: six orders, two completion snapshots (empty/full), eight timer masks. The
53 original review scenarios remain verbatim. State, stale/null turn, connection
generation, metadata replacement, both search phases, submission, authorization
and corruption cases extend that seed. The coverage manifest contains exact counts
by method, submission kind and order, and maps every A finding in all three reviews.

Every Rust step first succeeds in a transaction that is rolled back, then redelivers
against the unchanged buffer and commits. Only after commit does the caller apply
the returned buffer update. Saved replay must return its previous outcome, perform
zero SQLite changes and return no update; conflicting full content must refuse.
Timer flushes follow the same rollback/commit/apply sequence. Submission replay
also checks full text beyond 20,000 characters, deleted-agent state, and a fresh
process after commit before acknowledgement. Rollback-only steps have no committed
Python equivalent and are omitted from the expected Python sequence.

One account connection owns the buffer. Timer callers flush each affected agent;
other agents' entries remain untouched. The two-agent witness exercises that rule.
The buffer is lost on restart, like Python; saved delivery receipts still prevent
replayed frames from writing or recreating buffered deltas.

## Deviations and tiers

The notification registry now retains only `radio-policy` and the 8,192
open-stream bound. Token-usage, async-question and unhandled-kind deviation
groups are removed. Every notification fixture calls real `Runtime.notification`.
The token fixture records the budget owner's actual Python capture and injects
only that agent patch into Rust; budget tables belong to the budget corpus.
Notification-owned agent/context usage, transcript, tasks, plans, requests,
complaints, events, sync, search and receipt rows remain in this comparison.

Missing account, login, rate-limit, provider-generation, realtime, monitor and
thread-recovery services return `RequiresService` with a typed dependency.
The oracle still executes Python and compares its stored rows. This typed outcome
is a transport contract; it is not a claim that the missing effect ran.
The default production dispatcher turns these outcomes into an error and rolls
back its savepoint, preventing the worker from acknowledging an absent service.
The injected dispatcher may only be acknowledged after its caller handles the
named dependency in the owning transaction. Token usage defaults to a named
budget-capture refusal until the worker supplies the budget adapter.

Turn-start and terminal-turn notices belong to the scheduler and account worker.
Their real Python comparison remains in `turn_lifecycle_corpus.rs` (`started_notice`
and terminal transition witnesses); they no longer appear as unhandled inventory.
At the stream bound, the dispatcher returns `StreamCapacity` without changing
stored rows or the caller's stream buffer.

Fast cases cover every method, all six orders, every review regression and the
transaction/replay witnesses. Remaining notification methods share a single sequence to avoid
repeated state initialization. The full matrix additionally covers all timer masks,
empty/full completions and independent state/policy/corruption combinations. Timers
and generation permutations do not apply to prepared submissions or token usage;
Python represents stopped agents as paused with autoWake false. Radio-turn policy
is refused upstream by the scheduler; plain radio is tested as an ordinary kind.

Both corpora use the sole declaration list in `deviations.json`: the lifecycle's
three classes and notification's two. Per-case field patches must reference a
declared class and exact field inventory. An absent, equal, or otherwise stale
declared difference fails. The `rust_receipts` declaration owns the return contract:
Python first-call effects are compared as rows; Rust entry-point results are pinned
as case data. Rust receipt replay must return its saved result and write nothing,
and changed content must refuse without writing. Python callbacks have no such
receipt contract; the oracle does not pretend they do. Timer flushing has no
delivery identity; it is checked through staged buffer equality and stored effects.

Foreign-account/thread fixtures call Python's real routing path and assert no
stored changes. The late-failure fixture aborts receipt insertion after earlier
service writes, catches that error inside the caller's transaction, and commits
unrelated caller work. All service rows and the input buffer must remain unchanged.
Located refusals match exact errors, including the stored table and ID, and cannot
include the planted private request/stored sentinels. Prepared submission fixtures
include nonempty per-input assets and an ordered aggregate with a repeated asset.

`just test-rust-runtime-corpora-mutations` runs the permanent manual heavy mutation
check. It copies sources below an external target directory and never patches the
checkout. The unchanged baseline must pass; each mutation must compile and fail a
default corpus test. Infrastructure/build failures are invalid evidence. The
inventory in `mutations.json` includes concrete fast witnesses and lifecycle
transition/state combinations; those witnesses cannot silently leave the fast set.
See `docs/testing.md` for invocation options. Neither full matrices nor mutations
run automatically in the fast tier, and neither creates synthetic load.

The initial 255-case run took 90.9 seconds. Reusing seed states and the production
connection-reuse mode reduced it to 31.4 seconds. Avoiding recursive duplicate
path resolution in the isolation guard reduced a default witness run to 7.955
seconds without weakening guard checks. Full-matrix timing is reported separately
because it is a manual heavy run; no SQLite durability setting is weakened.

A subsequent busy-host measurement recorded 13.292 seconds wall, 0.970 seconds
Rust process CPU (0.930 user + 0.040 system), and 1.371 seconds oracle CPU
(1.223 user + 0.148 system). Phase wall times were 9.618 seconds for Python
seed/transition/backup, 3.469 for Rust open/apply, and 0.195 for final snapshots.
The combined CPU cost was 2.341 seconds; other observed default wall times were
7.955 and 26.861 seconds. Full-matrix wall times were 61.864 and 117.691 seconds.
Each run now writes a `.timing.json` sidecar with CPU, phase and wall measurements;
Linux Rust CPU comes from `/proc/self/stat`, and Python uses `getrusage`.

## Notification-kind integration

`dispatch_notification_with_effects` uses the same provider delivery identity,
receipt and staged-buffer contract as `dispatch_notification`. The worker's
`account_worker/store.rs` is intentionally not changed by this leaf. Its normal
notification call must invoke `dispatch_notification_with_effects` with its
transaction, agent ID, state path, stream buffer, identity, frame, receipt clock
and `&WorkerNotificationEffects`. Keep existing scheduler routing for
`turn/started`, `turn/completed`, and native user-message delivery acknowledgements.
A `RequiresService` result must be handled or converted into a transaction error
before acknowledging the provider cursor.

The agreed effect implementation is:

- Usage: `budget_service::capture(tx, agent, params, at,
budget::CaptureSource::Live, at)`; map its result to `()` and its located error
  into the worker error. This is the single budget accounting authority.
- Move: `agent_move::capture_cache(tx, agent_id, params, at)`; use its result in
  the same receipt transaction when an active move requires cache capture.
- Analytics: prepare and capture through the analytics owner outside the writer
  transaction. Do not let analytics failure consume lifecycle success.

`notices::error_message` temporarily delegates to the existing message catalog in
`account_worker::history::message`; the integration commit changes only its
visibility. When moving the catalog, the integrator moves that existing function
body to `notification_dispatch/notices.rs::error_message` and replaces the history
call `message(&error)` with
`crate::notification_dispatch::notices::error_message(&error)` (adjusting the
borrow at the existing call site). There must be one catalog, never two copies.
The task-window adapter reuses `sync_policy::maintain_task_window` through its
approved crate-visible accessor.

New witness families cover native warnings/hooks/safety/errors, exact native
request resolution, repeated and child async questions, command output with
Unicode truncation and repeated starts, failed tool completion, malformed stored
state and caught late-write errors. Full mode additionally injects SQLite trigger
failures before and after writes to each notification table. Every caught failure
must leave the raw rows unchanged even when unrelated caller work commits.
`notification_kinds_mutations.json` is the separate mutation inventory for these
rules; run it with the shared runner's `--manifest` option and notification-corpus
`--filter`. Infrastructure failure is not mutation detection.
