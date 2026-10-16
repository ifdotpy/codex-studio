# Native frontend integration, 16 October 2026

## Contract

Integrate the native frontend into current main while retaining server ownership
of every chat action, durable drafts, upload bytes, exact mutation identities,
and backend/VM process continuity. Restore the established desktop icon and
normalize browser defaults before Mantine and component styles.

## Reviewed integration boundaries

The candidate combines native branch `1cf4d7e94` with main `542d3a4db`.
Review is performed by the lead because the session allows no additional workers.
Existing desktop parity and performance evidence remains in
[native parity](native-parity-20261016.md) and
[client interactions](../performance/client-interactions-20261016.md).

- Keep per-server API, sync, transcript, outbox, upload and progress services.
  Integrate main's read retry and deletion reconciliation inside those owners.
- Upload wakeups use an owner-specific event and BroadcastChannel; a peer tab
  wakes only the matching server's recovery queue. Last unsubscribe closes the
  channel. The local legacy channel remains compatible.
- Preserve cancellation of rejected response bodies, reservation before signed
  request transmission, canonical preference request bytes and versioned receipt
  identities. The native response capacity remains 32.
- Keep supervisor frames at the shared 32 MiB limit. Two rejection fixtures now
  derive their oversized input from that owner instead of assuming 1 MiB.
- The Python historical-receipt oracle uses a fixed clock for evidence comparison.
  Host scheduling cannot turn a valid proof into an empty expected result. The
  production acceptance deadline remains unchanged.
- Resolve main's macOS compilation failures using metadata-based mount inspection.
  Symlinks are not mount points, missing paths are detached, and other inspection
  errors propagate before mount/unmount/removal can proceed.
- Use one workspace bytes dependency compatible with both runtime networking and
  the native WebRTC transport. Retain the existing native lock versions; the
  combined lock adds main's new dependencies without a wholesale update.
- CI prepares the pinned WASM binding generator, target, Linux WebView libraries
  and embedded renderer before native workspace checks. Package validation compares
  the bundle icon bytes with the canonical Electron desktop artwork.

## Delivery boundary

The live backend still exposes the earlier API schema. A source merge does not
prove that a newer client can connect to that running backend. Client delivery
must use compatible source, or a separately verified backend handoff, while
preserving active agents, the VM, drafts and signing identity.

## Verification status

The initial native-branch fast run passed 5,800 server tests and failed four Rust
checks. Two fixtures used the former frame bound; the historical receipt oracle
was wall-clock-dependent; the message-ingress Python oracle exceeded its deadline
under parallel execution. The combined candidate is checked with bounded local
Rust test concurrency, without increasing that deadline.

Combined TypeScript passes. The first combined web run passed 1,041 tests, with
one newly imported recovery fixture requiring a server-service mock. Focused
upload recovery checks now pass, including owner-specific channel delivery.
The compatible icon/reset client `1cf4d7e94` is installed on both Macs. Package
signature, canonical icon bytes, API compatibility and hidden startup passed.
Installation receipts preserve backend PIDs 47331 and 69542, VM and tracked agent
processes. The MacBook client was closed and remains closed.

The first combined fast run passed 5,957 server tests and stopped on five
failures. Nested oracle/HTTP socket paths exceeded the macOS limit; the account
fixture omitted passive macOS metadata; the project golden omitted macOS vault
readiness; the local Rust server fixture had not been built. Those fixture
requirements are corrected without changing production behavior. The runtime
suite passed all 78 tests in 32 seconds alone after its parallel deadline failure.
All five focused fixture/harness suites pass (108 tests; seven skipped), including
work writes, sync envelopes and account discovery. The final gate will run without
concurrent application packaging. Full integration verification remains pending.

The next full run passed 5,993 server tests (226 skipped), then exposed an
asynchronous initialization race in the ephemeral Claude session test. The test
now awaits the public metadata listing before inspecting the directory, and still
asserts that no session, journal or metadata file exists. All ten session-store
tests pass.

Reviewed guest fixes are incorporated from cca9235a5, e1cb2c933 and f92bb2845:
explicit pre-start host execution rejection releases both leases; lost replies
retain them. Provisioning repairs Codex release ownership and loads an AppArmor
profile for the exact installed binary, with installer tests in fast discovery.
Guest delivery is restricted to the supported MBP upgrade after providers, host
commands, leases and detached checks are idle. The old LUM VM is excluded.

The remaining gate passed all 63 bridge checks and 2,156 Rust checks, with 37
failures. Focused repairs cover macOS fixture paths, isolated vault HOME and
managed Python cache inputs, deterministic host-memory and disk-space ports,
and response completion before fixture teardown. Linux-only native thread and
supervisor contracts remain Linux-only. Production image space floors are
unchanged; a zero-capacity regression checks refusal before pending build state,
staging or background copying.

Darwin can return EPERM when signalling an exited but unreaped owned process
group. Terminal/provider cleanup now accepts that specific result only after
successful reaping and an ESRCH group-existence probe. Live groups and unrelated
errors still fail. The focused run passed 49 checks; the added low-space check
then passed after its assertion was corrected to allow the existing build lock.
The fake guest keeps its response socket alive until the client finishes reading.

Non-Rust checks pass: 1,044 web tests (one skipped), 63 bridge tests, ten tooling
checks and three TypeScript checks. Strict runtime typing includes the newly
moved sandbox installer fixture; its signatures now carry explicit annotations.
A final complete fast run remains required before merge.

Separately, a live recovery metadata repair on the local Mac replaced the former
server identity in nine project owner/location bindings and five chat bindings.
The repair retained other fields, tombstones and remote locations, saved an exact
backup and a durable idempotent receipt, and replayed no chat requests. Read-only
validation succeeds for all nine projects; the backend process did not restart.

The latest server gate passed 5,961 tests but failed one transfer fixture race
and three 120-second suite deadlines. The transfer fixture now waits for its
owners to finish before checking the durable receipt; reply arrival alone is
not that barrier. No production timeout changed.

Separate profiling passed the three suites. Work-write profiling identified
694,051 lexical path guards allocating every parent directory. Equivalent
separator-bounded prefix checks preserve exact-root, descendant, normalized-path
and symlink refusal; a sibling with the same name prefix remains allowed.
The same profiled suite fell from 43.741 to 18.130 seconds (20 tests, six skipped).
Guard cumulative time fell from 25.406 to 1.842 seconds. These are fixture timing
results, not a claim about live client speed or a passing combined gate.

Reviewed host-slot retirement commit 9ef92c548 is also incorporated. A durable
sibling marker prevents slot reuse after partial deletion; malformed metadata
fails closed. Recovery of the already damaged MBP slot remains a separate user
decision. The compatible payload source includes this fix and the earlier guest
fixes, but none are claimed deployed to the active guest.

The complete fast gate passed on 16 October at 14:36 UTC, on candidate tree
`cab0decab0dfa73d97dfe502eef5230e4a189b72`: 6,003 server tests, 63 bridge tests,
2,192 Rust tests, 1,044 web tests, ten tooling tests and six browser smoke tests.
There were 226 server, ten Rust and one web skips; no failures or browser flakes.
All three type-check stages passed. The full run took 1,311.485 seconds with four
server and Rust workers. The prior three deadline failures completed in 44.88,
23.78 and 18.34 seconds. Strict runtime typing separately passed all 369 files.
The only subsequent candidate edit before recording the merge is this result.
