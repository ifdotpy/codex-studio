# Studio server

## Change Contract

This app owns the Python backend, orchestration, SQLite writes, supervisor,
command entry points, runtime prompts, and server tests. Its public interfaces
are the HTTP API and installed CLI. The renderer must use HTTP; provider
credentials and native privileges stay with their owning processes. Preserve
state-directory identity, request IDs, receipts, and unknown outcomes after a
lost response. Runtime configuration belongs to the server source and its
deployment units; command scripts are owned by the [root manifest](../../../../package.json).
Focused check from the repository root: `pnpm run test:server:python -- --filter workspaces/runtime/apps/server/tests/runtime-contract.py`.

## Persisted state schemas

The persisted-state JSON schemas are committed source in
[`runtime-schema/src/schemas`](../../packages/runtime-schema/src/schemas). To validate a disposable state
directory without opening it for writes, run
`python3 workspaces/runtime/apps/server/src/codex_python.py --exec -m studio_api.state_check --state-dir <state-directory>`.
This checker validates the account registry and the workspace usage-limit
projection in `canvas.sqlite3`; it does not compare Rust sync tables.

Cross-language values are owned by Rust in
[`runtime-schema/src/shared_values.rs`](../../packages/runtime-schema/src/shared_values.rs).
`pnpm run contracts:generate` renders the TypeScript, Swift, and VM guest copies;
`pnpm run contracts:check` rejects drift. `codex_shared_values.py` is a Python
copy that remains until the Python server is removed.

## Navigation

- [`src/`](src/): implementation and flat Python import root; see the
  [source guide](src/README.md).
- [`tests/`](tests/): runner-discovered server and runtime contracts.
- [`prompts/`](prompts/): runtime prompts.
- [Test selection](../../../../docs/testing.md) explains suite ownership and
  focused filters.
