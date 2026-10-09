# Studio server

## Change Contract

This app owns the Python backend, orchestration, SQLite writes, supervisor,
command entry points, runtime prompts, and server tests. Its public interfaces
are the HTTP API and installed CLI. The renderer must use HTTP; provider
credentials and native privileges stay with their owning processes. Preserve
state-directory identity, request IDs, receipts, and unknown outcomes after a
lost response. Runtime configuration belongs to the server source and its
deployment units; command scripts are owned by the [root manifest](../../../../package.json).
Focused check: `pnpm run test:server:python -- --filter workspaces/runtime/apps/server/tests/runtime-contract.py`.

## Navigation

- [`src/`](src/): implementation and flat Python import root; see the
  [source guide](src/README.md).
- [`tests/`](tests/): runner-discovered server and runtime contracts.
- [`prompts/`](prompts/): runtime prompts.
- [Test selection](../../../../docs/testing.md) explains suite ownership and
  focused filters.
