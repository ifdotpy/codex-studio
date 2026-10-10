# studio-operations

## Change Contract

- **Responsibility:** Pure decision logic for one scoped mutating request from
  reservation through dispatch, uncertain outcome, and evidence-based recovery.
- **Public interface:** Named request and scope IDs, closed state/event/decision/
  error enums, and `transition(state, event) -> decision`.
- **Boundaries:** No Python, application, provider, subprocess, SQLite, filesystem,
  network, clock, randomness, or global-state access. The caller validates
  untrusted bytes, persists a decision and its effect intent with the expected
  revision, then performs the intent.
- **Invariants:** Request identity is immutable and compacted settled requests
  retain typed tombstones; a request cannot dispatch twice;
  stale or duplicate events cannot regress state; unknown outcomes require
  positive evidence to settle; cancellation after dispatch preserves uncertainty.
- **Configuration owner:** The Rust workspace owns toolchain, shared lints, and
  lockfile. The caller supplies time, process generation, account/thread epoch,
  and canonical content identity as data.
- **Dependencies:** Optional `serde` derives support the in-process PyO3
  binding; `serde_json` parses the shared fixture in Rust tests and carries the
  same typed structures through the binding.
- **Focused check:** `cargo test -p studio-operations --locked`; the Python
  caller contract is `just test server operations-native-contract.py` after the
  native module build/install step in [workspace testing](../../../../docs/testing.md).

See [the behavior model and Python mapping](docs/model.md) for the source-derived
contract, fixture cases, and confirmed lifecycle decisions.
