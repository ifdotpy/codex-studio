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
and backs it up to a distinct Rust root. No Rust-written row becomes an expected
fixture. Snapshots compare all selected rows, JSON business fields, absence/null,
item creation order, full text, event links, receipt signatures/results, canonical
agent sync payloads and hashes, legacy search rows and next-index token postings.
The raw sync hash is validated before hashing the normalized payload. Only explicit
wall-clock/revision fields are replaced; rows otherwise remain exact.

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

Only the five named deviation groups in `notification_coverage.json` are allowed:
token budget/cache/token-rate captures; async questions; module-table unhandled
kinds; `radio_turn` policy; and the 8,192 open-stream bound. Token and question
cases execute the Python path and assert exact allowed field differences. Unhandled
kind cases deliberately compare only their exact named counter and receipt, using
Python's real put/save_receipt writers; they do not claim equivalence for the omitted
Python effects. At the bound, the Rust entry point returns `StreamCapacity` with no
write or buffer change. Its unit witness consumes the same corpus case data.

Fast cases cover every method, all six orders, every review regression and the
transaction/replay witnesses. Unhandled methods share a single sequence to avoid
repeated state initialization. The full matrix additionally covers all timer masks,
empty/full completions and independent state/policy/corruption combinations. Timers
and generation permutations do not apply to prepared submissions or token usage;
Python represents stopped agents as paused with autoWake false. Radio-turn policy
is refused upstream by the scheduler; plain radio is tested as an ordinary kind.

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
