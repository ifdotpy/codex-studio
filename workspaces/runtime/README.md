# Runtime workspace

Owns the Studio backend, command entry points, VM guest service, and runtime
prompts. The server owns SQLite and orchestration; the renderer accesses it over
HTTP, and native privileges stay in the isolated Electron main process.

## Apps and navigation

- [`apps/server/`](apps/server/): Python backend and command entry points; see
  its [Change Contract](apps/server/README.md) and [source guide](apps/server/src/README.md).
- [`apps/server/tests/`](apps/server/tests): server, protocol, and provider
  contract tests; suite discovery is owned by the [server runner](apps/server/src/test-server.py).
- [`apps/server/prompts/`](apps/server/prompts): model-facing runtime prompts.
- [`apps/vm-guest/`](apps/vm-guest): Linux guest service and provisioning; see
  its [operator guide](apps/vm-guest/README.md).

There are no runtime packages yet. The server app consumes provider integrations
through their app interfaces. Shared packages must not import apps or acquire
runtime state without a real caller and a focused test.

For one hypothesis, use `just test server <suite-filter>`; for a server change,
use `just check server`. The root [`justfile`](../../justfile) is the discoverable
entry point; the Python runner owns suite discovery and filtering. The pre-PR
server/client gate is documented in [docs/testing.md](../../docs/testing.md).
