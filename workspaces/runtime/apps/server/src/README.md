# Backend code

## Change Contract

For backend contributors: keep entry points stable and group implementation by
responsibility as it changes. The server owns SQLite and orchestration; pure
processing modules must not start a runtime or access account state. Keep runtime
configuration and defaults in their owning modules. Run the component's checks
and the affected integration contracts before delivery.

- [Analytics](analytics/README.md): rollout translation, local tests, and benchmarks.
- [Native notifications](native_notifications/README.md): route notices to the
  right account and chat without reading every agent.
- [Transcript storage](transcript_storage/README.md): keep full text durable while
  limiting repeated search-index work.
- [UI synchronization](sync/README.md): refresh the data that changed and preserve
  reconnect and offline behavior.
- [Message delivery scenario](sync/benchmarks/message_delivery/README.md): measure
  synthetic agent-message delivery through sync invalidation and HTTP reads.
- [Runtime load scenario](studio_api/benchmarks/runtime_load/README.md): exercise
  the production HTTP and browser paths with 256 synthetic active workers.
- Other benchmark scenarios remain available at commit `3507feea`.
- `codex_canvas.py`: HTTP entry point and request handling.
- `codex_runtime.py`: agent lifecycle and provider coordination.
- [Root README](../../../../../README.md): backend and application checks.
- [Live updates](../../../../../docs/live-updates.md): source publication and runtime verification.

Set `CODEX_CANVAS_STARTUP_MEMORY=1` to log current RSS and peak RSS at startup
stage checkpoints. Add `CODEX_CANVAS_STARTUP_TRACEMALLOC=1` to log the top Python
allocation sites at those checkpoints.

New component folders should have an explicit import path, a short README, and
focused `workspaces/runtime/apps/server/tests/`. Add component-local benchmarks only when there is a repeatable
workload worth measuring. Import production functions directly; do not copy them
into tests or benchmarks. Package `__init__.py` files should not assemble a
second public API.

Move a bounded responsibility together with its callers and checks. Do not create
empty layers or reorganize unrelated modules. Keep cross-component contracts in
the root `workspaces/runtime/apps/server/tests/` directory. Production package sources must participate in the
backend identity and live-update source checks; test and benchmark files do not.

Keep benchmark environments, results, temporary databases, and other runtime
state outside this checkout. Fixtures committed here must be synthetic and must
not contain account data or real conversation history.
