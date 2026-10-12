# Rust HTTP conformance summary

Measured with `just rust-http-conformance-ledger`; the full per-operation and per-test ledger is written under `$XDG_CACHE_HOME/codex-studio/` (or `~/.cache/codex-studio/` when unset), not tracked in the repository.

At schema hash `522675281ab6c09ba15e99c10299c1ba43cc503539de57ff92c8e8ccf377492f`: 159 documented operations; 3 implemented; 156 return 501. The seven discovered consumer suites contain 3 passing tests and 18 failing tests. The command output reports the operation-level blockers and verifies their 501 error envelopes.
