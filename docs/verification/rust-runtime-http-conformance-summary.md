# Rust HTTP conformance summary

Measured with `just rust-http-conformance-ledger`; the full per-operation and per-test ledger is written under `$XDG_CACHE_HOME/codex-studio/` (or `~/.cache/codex-studio/` when unset), not tracked in the repository.

At schema hash `0a0f2963dcb7971d359a0a33359cb023e530083bc2fd77b153ff4b709daa4e9a`: 160 documented operations; 3 implemented; 157 return 501. The seven discovered consumer suites contain 3 passing tests and 18 failing tests. The command output reports the operation-level blockers and verifies their 501 error envelopes.

The tightened-baseline extractor differential generated 10,701 cases. Its 22
permanent mismatches comprise 14 exact Python model/route-rule refusals (400 in
Python versus 200 in Rust) and 8 opaque `JsonValue` integer precision values
normalized only at their exact leaf. Two further opaque precision predicate
cases were agreement-on-refusal at 400/400. There are no temporary
pre-tightening allowances.
