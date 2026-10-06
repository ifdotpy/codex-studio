# Runtime load benchmark (retired)

The `run.mjs` benchmark was removed because its `--check` path failed before
measurement (`CanvasServer` had no `RequestHandlerClass`) and it depended on the
legacy `/api/state` snapshot. Historical measurements remain in
[`docs/verification/2026-10-01-runtime-latency.md`](../../../docs/verification/2026-10-01-runtime-latency.md).
