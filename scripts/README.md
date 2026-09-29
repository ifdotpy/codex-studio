# Backend code

## Change Contract

For backend contributors: keep entry points stable and group implementation by
responsibility as it changes. The server owns SQLite and orchestration; pure
processing modules must not start a runtime or access account state. Keep runtime
configuration and defaults in their owning modules. Run the component's checks
and the affected integration contracts before delivery.

- [Analytics](analytics/README.md): rollout translation, local tests, and benchmarks.
- `codex_canvas.py`: HTTP entry point and request handling.
- `codex_runtime.py`: agent lifecycle and provider coordination.
- [Root README](../README.md#checks): backend and application checks.
- [Live updates](../docs/live-updates.md): source publication and runtime verification.

New component folders should have an explicit import path, a short README, and
focused `tests/`. Add `benchmarks/` when there is a repeatable workload worth
measuring. Import production functions directly; do not copy them into tests or
benchmarks. Package `__init__.py` files should not assemble a second public API.

Move a bounded responsibility together with its callers and checks. Do not create
empty layers or reorganize unrelated modules. Keep cross-component contracts in
the root `tests/` directory. Production package sources must participate in the
backend identity and live-update source checks; test and benchmark files do not.

Keep benchmark environments, results, temporary databases, and other runtime
state outside this checkout. Fixtures committed here must be synthetic and must
not contain account data or real conversation history.
