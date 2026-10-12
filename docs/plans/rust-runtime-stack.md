# Rust runtime stack proposal

## Decision frame

This is the M-08.1 proposal for [issue #37](https://github.com/ifdotpy/codex-studio/issues/37).
It selects a Rust runtime shape without changing code, dependencies, state, or
wire behavior. The PyO3 bridge stays frozen; the HTTP boundary (163 routes and
its canonical schema hash, with contract suites behind a backend harness) is
the executable specification; then the Python runtime moves as one unit and
opens the existing SQLite state in place. Cutover waits for the gates in
[`rust-workspaces.md`](rust-workspaces.md).

Owner priorities: modular implementation; simple, compact, readable code with
little duplication; strict types with one schema authority; then performance
and least custom code. Each subsection ends with an independently approvable
recommendation. Crate facts below were checked against crates.io and upstream
documentation on **2026-10-12**; current release means the latest non-yanked
published version found that day. MSRV is registry metadata unless marked
undeclared. Dependency counts are normal direct dependencies (optional ones
included), not a resolved graph. No security audit or cross-target build was
run. A missing fact is called out instead of inferred.

## Runtime requirements from the repository

- **HTTP:** one Uvicorn worker serves loopback HTTP and an owned Unix socket;
  middleware enforces trust, API hash, request identity, and error rules. One
  SSE stream (`GET /api/sync/stream`), streaming file reads, and raw JSON/audio
  bodies are used. No WebSocket, long-poll, or multipart endpoint was found.
  Pairing/invitations and tokens are application controls. Public HTTPS is
  supplied by external Tailscale Serve; notify is resource invalidation, not a
  general relay. Sources: [`server.py`](../../workspaces/runtime/apps/server/src/studio_api/server.py),
  [`middleware.py`](../../workspaces/runtime/apps/server/src/studio_api/middleware.py),
  [`sync/router.py`](../../workspaces/runtime/apps/server/src/studio_api/sync/router.py),
  [`io/router.py`](../../workspaces/runtime/apps/server/src/studio_api/io/router.py),
  [`federation/router.py`](../../workspaces/runtime/apps/server/src/studio_api/federation/router.py).
- **Concurrency and OS work:** one process owns listeners, worker pools,
  scheduler/maintenance threads, child-process supervisors, terminal sessions,
  and an OS file watcher. Preserve cancellation, event order, shutdown and
  uncertain-outcome behavior. Sources: [`codex_runtime.py`](../../workspaces/runtime/apps/server/src/codex_runtime.py),
  [`codex_process_supervisor.py`](../../workspaces/runtime/apps/server/src/codex_process_supervisor.py),
  [`codex_terminals.py`](../../workspaces/runtime/apps/server/src/codex_terminals.py),
  [`codex_progress_watch.py`](../../workspaces/runtime/apps/server/src/codex_progress_watch.py).
- **SQLite:** main, analytics/history and auxiliary databases use Python
  `sqlite3`; WAL, FULL durability on durable write paths, explicit
  `BEGIN IMMEDIATE`, concurrent readers and a serialized writer path are in
  use. Startup creates missing tables and applies migrations. The database
  directory identity and existing rows stay intact. Sources: [`codex_runtime.py`](../../workspaces/runtime/apps/server/src/codex_runtime.py),
  [`codex_sync.py`](../../workspaces/runtime/apps/server/src/codex_sync.py),
  [`codex_process_supervisor.py`](../../workspaces/runtime/apps/server/src/codex_process_supervisor.py),
  [durability decision](../decisions/2026-10-11-runtime-commit-durability.md).
- **Types and packaging:** Pydantic models produce FastAPI OpenAPI; schema
  canonicalization and SHA-256 identity live in [`schema.py`](../../workspaces/runtime/apps/server/src/studio_api/schema.py)
  (the current canonicalizer is version 3); `generate_types.py` runs pinned
  `openapi-typescript` for the web client. The 163-route inventory and hash
  baseline must be captured at M-08.2. Sources: [`generate_types.py`](../../workspaces/runtime/apps/server/src/studio_api/generate_types.py),
  [schema-gap inventory](../verification/api-schema-gap-inventory-2026-10-11.md).
- **Platform paths:** the product guide names a signed macOS Apple-silicon
  Electron desktop package; Linux and WSL use CLI/server/service setup; Windows
  server/install/supervisor scripts exist. These are preservation paths, not
  evidence that all targets are currently released or CI-tested. Desktop
  packaging stages Python source and `requirements.txt`, not a Python
  interpreter ([`package.mjs`](../../workspaces/client/apps/desktop/package.mjs));
  launch resolution uses configured, managed, then equipped system Python
  ([`codex_python.py`](../../workspaces/runtime/apps/server/src/codex_python.py),
  [`backend.cjs`](../../workspaces/client/apps/desktop/backend.cjs)). CLI setup
  creates a venv from a selected base interpreter. Windows install receives a
  Python path ([`install-windows-server.ps1`](../../workspaces/runtime/apps/server/src/install-windows-server.ps1)).
  Product/platform references: [product guide](../product-guide.md),
  [Linux/WSL setup](../linux-server-setup.md), [desktop README](../../workspaces/client/apps/desktop/README.md).

## Decisions and candidate facts

### Async runtime

| Candidate                                       | Version | Released   | License           | MSRV | Status           |
| ----------------------------------------------- | ------: | ---------- | ----------------- | ---: | ---------------- |
| [Tokio](https://crates.io/crates/tokio)         |  1.53.2 | 2026-10-03 | MIT               | 1.71 | Active           |
| [async-std](https://crates.io/crates/async-std) |  1.13.2 | 2025-08-15 | Apache-2.0 OR MIT | 1.63 | Quieter releases |

Tokio has 16 normal deps (`pin-project-lite`, `bytes`, `socket2`, optional
`mio`); async-std has 21 (`async-channel`, `async-io`, `async-lock`,
`async-process`). Both target common desktop/server platforms; Tokio supplies
multi-thread executor, timers, async process/signals/I/O. async-std has less
integration with the selected HTTP option.

Tokio aligns with Axum and common async process support, at the cost of async
boundaries and blocking-pool rules around SQLite. A synchronous core with only
an async HTTP edge can more closely match today's thread-based code and make
domain functions easier to read; Axum still needs a Tokio executor. **Recommend
Tokio as the HTTP edge only initially; keep domain/store/process interfaces
synchronous and use explicit blocking workers until measurements or SSE/load
needs justify more async.** Owner choice: Tokio multi-thread runtime with
rusqlite on blocking threads, or synchronous thread model plus a small async
edge? Each can be simpler if applied consistently; avoid async-through-every-layer.

### HTTP server

| Candidate                                       | Version | Released   | License           | MSRV | Status |
| ----------------------------------------------- | ------: | ---------- | ----------------- | ---: | ------ |
| [Axum](https://crates.io/crates/axum)           |   0.8.9 | 2026-04-14 | MIT               | 1.80 | Active |
| [Actix Web](https://crates.io/crates/actix-web) |  4.15.0 | 2026-08-21 | MIT OR Apache-2.0 | 1.88 | Active |

Axum has 33 normal deps (17 non-optional) in Tower/Hyper; Actix Web has 36
(31 non-optional), including `actix-http`, `actix-server`, `actix-rt`. Both use
Tokio and support streaming/SSE/WebSocket on Linux/macOS/Windows through their
dependencies. Axum middleware order is explicit; Actix uses framework-specific
extractors/middleware.

Both support the observed HTTP needs. Axum’s Tower layers and modular route
composition fit domain packages; Actix is more integrated but introduces
framework concepts that current routes do not require. **Recommend Axum.**

### OpenAPI and Rust DTO authority

| Candidate                                                          |        Version | Released   | License           |       MSRV | Status                                  |
| ------------------------------------------------------------------ | -------------: | ---------- | ----------------- | ---------: | --------------------------------------- |
| [Utoipa](https://crates.io/crates/utoipa)                          |          6.0.0 | 2026-09-22 | MIT OR Apache-2.0 |       1.88 | Active                                  |
| [Aide](https://crates.io/crates/aide)                              | 0.16.0-alpha.4 | 2026-04-14 | MIT OR Apache-2.0 | Undeclared | Pre-release; stable 0.15.1 (2025-08-19) |
| [openapi-to-rust](https://crates.io/crates/openapi-to-rust/0.22.0) |         0.22.0 | 2026-10-06 | MIT               |       1.88 | Active                                  |
| [typify](https://crates.io/crates/typify/0.10.0-alpha.2)           | 0.10.0-alpha.2 | 2026-10-05 | Apache-2.0        | Undeclared | Pre-release                             |

Utoipa has 5 normal deps (3 required: `indexmap`, `serde`, `serde_json`), no
runtime, OpenAPI 3.1 and Axum/Actix integration. Aide has 15 (7 required,
including `schemars`, `tracing`, `thiserror`); its Axum path uses Tokio.
`openapi-to-rust` has 17 required normal deps (`jsonschema`, `serde_json_path`,
`syn`, `prettyplease`, `thiserror`), supports OpenAPI 3.0/3.1, and needs no
runtime for generated DTOs. typify has 2 required normal deps (`typify-impl`,
optional macro) but consumes JSON Schema, so OpenAPI component projection is
required.

**Feasibility probe (2026-10-12):** isolated crate and venv under
`/var/tmp/codex-runtime-stack-probe`; no repository manifest or lockfile was
changed. Python schemas came from the actual `SupervisorIdentity`,
`ErrorResponse`, `ProjectWriteRequest`, and `ResourceRef` models. Utoipa 6.0.0
derived corresponding Rust projections; both sides were wrapped as OpenAPI
components and normalized by the repository's `canonical_openapi_json`
(version 3), then compared structurally per model. **All four differed:**

- `SupervisorIdentity`: Utoipa omitted the Python type/field titles and
  `additionalProperties: false`; it added `format: int64` to `generation`.
- `ErrorResponse`: nullable fields used `type: [T, null]` rather than Python's
  `anyOf`; Utoipa omitted `default: null`/titles and the closed-object marker.
  Its `metadata`/`details` mapped to unconstrained JSON rather than the
  recursive Pydantic `JsonValue` schema.
- `ProjectWriteRequest`: same nullable/default/title differences; `action`
  used a referenced Rust enum instead of Python's inline literal alternatives.
- `ResourceRef`: Python emitted referenced variant components and a
  discriminator mapping; Utoipa flattened tagged enum variants into a
  different oneOf shape and did not reproduce the component refs/mapping.

Canonicalization removes docs and sorts union members; it does not normalize
these structures. The output schemas and bounded per-model path-diff report
remain under `/var/tmp/codex-runtime-stack-probe` for review; no repository
test was added. This was a first-pass derive, not an
exhaustive attempt to tune every Utoipa attribute or write custom schemas, so it
does **not** prove parity impossible. It does refute assuming parity by default.
The first attempt to derive the recursive `JsonValue` as an untagged Rust enum
overflowed the schema generator's stack; the comparison used `serde_json::Value`
for that field, which emitted an unconstrained `{}` schema. A manual recursive
schema or alternate generated representation is therefore part of the
feasibility work, not yet demonstrated.

For schema-first, retain the exact frozen canonical OpenAPI output from Python
as the only schema source during migration; generate Rust request/response
DTOs from it (or generate then compile/test-check route DTOs against it). Keep
`openapi-typescript` and its output unchanged. `openapi-to-rust` is OpenAPI-aware
but brings a larger codegen surface; `typify` is a smaller JSON-Schema route
after projecting component schemas. Review generator support for all 51
open-by-design routes, recursive `JsonValue`, nullable unions and aliases
before selecting it. **Recommend the frozen OpenAPI artifact plus
openapi-to-rust for DTO generation** because it consumes OpenAPI directly;
reject typify for this use unless the projection and alpha release prove less
maintenance. Reject default Utoipa/Aide output as schema authority until an
exact-hash prototype passes. After Python deletion, schema-first means the committed
OpenAPI contract remains authoritative and Rust DTOs plus TypeScript stay
generated; Utoipa-first means Rust types become authoritative and emit OpenAPI
and TypeScript. **Recommend schema-first through cutover: hash equality is
preserved by construction while handler compatibility is checked, instead of
making a cross-framework renderer reproduce schema quirks immediately. Keep
the committed OpenAPI artifact as the single authority afterward unless the
owner separately accepts a measured Rust-type-to-identical-hash transfer.**

### SQLite access

| Candidate                                     | Version | Released   | License           |       MSRV | Status |
| --------------------------------------------- | ------: | ---------- | ----------------- | ---------: | ------ |
| [rusqlite](https://crates.io/crates/rusqlite) |  0.40.2 | 2026-08-08 | MIT               | Undeclared | Active |
| [SQLx](https://crates.io/crates/sqlx)         |   0.9.0 | 2026-05-21 | MIT OR Apache-2.0 |       1.94 | Active |

rusqlite has 20 normal deps (5 required), including `libsqlite3-sys`; it is
synchronous and has optional bundled SQLite. SQLx has 5 top-level normal deps,
but a much larger selected feature graph; its async API needs Tokio or
async-std. Both use SQLite's serialized writer; target library/bundling needs
an explicit platform choice.

The lead verified this Linux host’s managed Python 3.14.8 `_sqlite3` dynamically
links `/usr/lib/libsqlite3.so.0`, SQLite 3.53.4. The desktop packager stages
source/requirements and uses a selected host Python; it does not bundle Python.
The CLI similarly builds a venv from a selected base interpreter. Windows
scripts pass/use an interpreter path. **The exact SQLite library/version in a
macOS or Windows user's selected Python was not verified**; those cannot be
assumed to match a separately linked Rust system library. `rusqlite` system
mode may share Linux's dynamic library on this host, but per-install Python
linkage differs elsewhere and Windows system-library presence is not
established. `bundled` gives Rust a controlled SQLite version without relying
on a host development library, but Rust may then use a different version from
Python; it does not automatically make rollback safer.

SQLite 3 database files are cross-platform and backwards-compatible across
SQLite 3 releases, but forwards compatibility is incomplete. WAL adds a `-wal`
file that is part of persistent state and must travel with the database until
clean close/checkpoint; WAL readers require same-machine shared memory and it
allows many readers but one writer. WAL support begins at SQLite 3.7.0. The
SQLite project documents a WAL-reset bug fixed in 3.51.3+; the lead-verified
3.53.4 is newer. Sources: [file portability/compatibility](https://www.sqlite.org/onefile.html),
[WAL behavior and reset fix](https://www.sqlite.org/wal.html),
[format changes](https://www.sqlite.org/formatchng.html). **Recommend rusqlite
with a pinned `bundled` SQLite build** for reproducible release artifacts,
subject to M-08.2 capturing every supported Python interpreter's
`sqlite_version()` and proving the proposed engine reads/writes old-format
fixtures and permits fallback. If that proof fails, keep the old runtime as
rollback target and require an owner decision; do not silently switch system/
bundled mode by platform. Reject SQLx because its async query layer adds a
runtime abstraction without increasing SQLite's single-writer capacity or
improving the existing synchronous transaction model.

### JSON serialization

| Candidate                                         | Version | Released   | License           |       MSRV | Status |
| ------------------------------------------------- | ------: | ---------- | ----------------- | ---------: | ------ |
| [serde_json](https://crates.io/crates/serde_json) | 1.0.154 | 2026-10-11 | MIT OR Apache-2.0 |       1.71 | Active |
| [sonic-rs](https://crates.io/crates/sonic-rs)     |  0.5.10 | 2026-09-11 | Apache-2.0        | Undeclared | Active |

serde_json has 7 normal deps (5 required), is synchronous and portable; it is
already configured with arbitrary precision. sonic-rs has 13 (`ahash`,
`bumpalo`, `bytes`, `faststr` and SIMD support); its target matrix is not
verified for Studio.

Serde is shared by HTTP DTOs, Utoipa and current Rust crates; arbitrary JSON
and numeric-format behavior need fixtures. A second implementation adds
complexity without a measured bottleneck. **Recommend serde_json.**

### Process and PTY supervision

| Candidate                                             | Version | Released   | License |       MSRV | Status |
| ----------------------------------------------------- | ------: | ---------- | ------- | ---------: | ------ |
| [Tokio](https://crates.io/crates/tokio)               |  1.53.2 | 2026-10-03 | MIT     |       1.71 | Active |
| [portable-pty](https://crates.io/crates/portable-pty) |   0.9.0 | 2025-02-11 | MIT     | Undeclared | Quiet  |
| [pty-process](https://crates.io/crates/pty-process)   |   0.5.3 | 2025-07-12 | MIT     | Undeclared | Quiet  |

Tokio process/signals are async, cross-platform, and already selected; they do
not provide PTYs. portable-pty has 15 normal deps (13 required, including
`nix`, `libc`, `filedescriptor`, `anyhow`, `log`) and synchronous Unix/Windows
PTY support. pty-process has 2 (`rustix`, optional Tokio), async PTY API and
Unix-oriented docs; Windows parity is unverified.

Ordinary child supervision should reuse Tokio; PTY needs a narrow adapter and
platform validation. **Recommend Tokio process/signal plus portable-pty only
for PTYs**, behind a local `PtyHost` interface; reject pty-process because
Windows behavior is not established and ordinary process handling would
duplicate Tokio.

### File watching

| Candidate                                       |    Version | Released   | License    | MSRV | Status                                 |
| ----------------------------------------------- | ---------: | ---------- | ---------- | ---: | -------------------------------------- |
| [notify](https://crates.io/crates/notify)       | 9.0.0-rc.5 | 2026-08-30 | CC0-1.0    | 1.88 | Pre-release; stable 8.2.0 (2025-08-03) |
| [watchexec](https://crates.io/crates/watchexec) |      8.4.4 | 2026-10-06 | Apache-2.0 | 1.61 | Active                                 |

notify 9 has 7 normal deps, OS backends for Linux/macOS/Windows, callback API,
no runtime; event coalescing/rename behavior is OS-dependent. watchexec is a
higher-level cross-platform watcher built on notify/Tokio with a larger
dependency graph and more orchestration than one progress-file watch needs.
Stable notify 8.2.0 is CC0-1.0 with MSRV 1.77; the 9.0 release candidate is
also CC0-1.0 and raises MSRV to 1.88.

Current code needs one owned OS-backed progress watcher, not a generic command
watch service. **Recommend stable notify 8.x until 9.0 is stable**, isolate
backend events behind `ProgressWatch`, and contract-test rename/overflow/rescan
behavior on shipped platforms.

### Logging

| Candidate                                                         | Version | Released   | License           | MSRV | Status |
| ----------------------------------------------------------------- | ------: | ---------- | ----------------- | ---: | ------ |
| [tracing](https://crates.io/crates/tracing)                       |  0.1.44 | 2025-12-18 | MIT               | 1.65 | Active |
| [log](https://crates.io/crates/log)                               |  0.4.34 | 2026-08-22 | MIT OR Apache-2.0 | 1.71 | Active |
| [tracing-subscriber](https://crates.io/crates/tracing-subscriber) |  0.3.23 | 2026-03-13 | MIT OR Apache-2.0 | 1.65 | Active |

tracing has 5 normal deps and provides backend-neutral spans/events; log has 1
dependency but no structured span context by itself. tracing-subscriber has 18
normal deps and supplies filters/formats on supported platforms.

**Recommend tracing plus tracing-subscriber** for request/agent spans and
consistent structured logs; reject `log` alone because it loses correlation
context already valuable in concurrent jobs.

### Error handling

| Candidate                                       | Version | Released   | License           | MSRV | Status |
| ----------------------------------------------- | ------: | ---------- | ----------------- | ---: | ------ |
| [thiserror](https://crates.io/crates/thiserror) |  2.0.21 | 2026-09-23 | MIT OR Apache-2.0 | 1.77 | Active |
| [anyhow](https://crates.io/crates/anyhow)       | 1.0.104 | 2026-07-18 | MIT OR Apache-2.0 | 1.68 | Active |

Both are synchronous and portable with one normal dependency each. thiserror
derives typed standard errors; anyhow adds contextual dynamic errors but does
not define stable public variants.

Public package and HTTP errors need typed variants; app startup/CLI boundaries
benefit from context. **Recommend thiserror at package boundaries and anyhow
only in the server app.**

### Test runner

| Candidate                                                                |     Version | Released   | License           |      MSRV | Status                    |
| ------------------------------------------------------------------------ | ----------: | ---------- | ----------------- | --------: | ------------------------- |
| [cargo-nextest](https://crates.io/crates/cargo-nextest/0.9.148)          |     0.9.148 | 2026-10-08 | Apache-2.0 OR MIT |      1.91 | Active; repo pins 0.9.146 |
| [`cargo test`](https://doc.rust-lang.org/cargo/commands/cargo-test.html) | Rust 1.99.0 | 2026-10-01 | MIT OR Apache-2.0 | Toolchain | Stable built-in           |

nextest has 31 required normal deps, notably `guppy`, `clap`,
`nextest-filtering`; it publishes Linux/macOS/Windows x86_64 binaries and
documents slower execution on Windows. Cargo's built-in runner adds no
dependency and follows the pinned toolchain target matrix, with fewer suite
controls. Nextest release facts checked 2026-10-12 against [crates.io](https://crates.io/crates/cargo-nextest/0.9.148)
and [platform docs](https://nexte.st/docs/installation/pre-built-binaries/);
Rust toolchain date is from the [1.99.0 release](https://blog.rust-lang.org/2026/10/01/Rust-1.99.0/).

Avoid a duplicate runner; `just test <package> <name>` should map to focused
nextest selection and finish without external providers/database when the
package is pure. **Recommend retaining cargo-nextest; use built-in cargo test
for minimal local smoke/debug only.**

## Recommended crate layout

Add the runtime under the existing root Cargo workspace and the established
`workspaces/<domain>/{apps,packages}` structure from
[`rust-workspaces.md`](rust-workspaces.md). Keep the PyO3 package frozen and
remove it only at whole-runtime cutover.

| Package                                             | Public interface and ownership                                                                                                                                                                                                                                                                                                                                                                                 |
| --------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `workspaces/runtime/packages/operations` (existing) | Pure IDs, state transitions, reservation/settlement and uncertain-outcome rules. No routes or tables. Add only extracted pure domain functions; no new parallel core crate.                                                                                                                                                                                                                                    |
| `workspaces/runtime/packages/runtime-store`         | `Store::open(existing_state)`, typed repositories and explicit transaction APIs. Sole owner of SQLite handles/pragmas and all existing tables. Module/table families: sync; analytics/insights; voice; terminals; history/search/checkpoints; IO/file/payload; then agents, work/orchestration, accounts/access, pairing and remaining legacy tables. Inventory every actual table once before implementation. |
| `workspaces/runtime/packages/runtime-providers`     | Provider traits/adapters, protocol framing/event decoding, account discovery and replay fixtures. No routes or tables.                                                                                                                                                                                                                                                                                         |
| `workspaces/runtime/packages/runtime-process`       | Child supervisor, command monitor, shutdown/signals, PTY `PtyHost`, progress `ProgressWatch`. No route registration or SQL; terminal persistence via store API.                                                                                                                                                                                                                                                |
| `workspaces/runtime/packages/runtime-http`          | Axum router/middleware, request/response DTOs, error conversion, frozen OpenAPI serving and contract entry point. Route modules own existing groups: core/system; agents; work/orchestration; accounts; sync; IO/terminals; analytics/history/search; voice; federation; multi-server/pairing. It calls public core/store/provider/process APIs; no SQL.                                                       |
| `workspaces/runtime/apps/server`                    | Configuration, one state lock, listener/socket lifecycle, tracing, dependency assembly, launcher/CLI and shutdown. Only app chooses implementations and starts the process.                                                                                                                                                                                                                                    |

Allowed dependency direction (arrows point toward dependencies):

```text
runtime-store ───────────→ operations
runtime-providers ───────→ operations
runtime-process ─────────→ operations + provider/store ports as needed
runtime-http ────────────→ operations + store + providers + process
server app ──────────────→ all runtime packages; owns wiring
```

Packages do not depend on the app, import sibling private source, or form
cycles. Route tests use in-memory repository/provider fakes; process behavior
uses fake child/PTY/watch ports. A pure operation test invokes one function
directly. This is the intended fast hypothesis loop:
`just test operations <name>`; it needs no database, provider, listener or app
startup. Package integration suites explicitly select the fakes or fixtures
they need.

## Frozen contract, platform matrix, packaging and rollback

### Frozen-schema rule

At M-08.2 record the canonical OpenAPI document/hash (163-route and
method/route-set assertions), database `sqlite_schema` rows, SQLite
version/compile options, and representative existing-format DB fixtures.
During the move do not change tables, columns, indexes, triggers, stored JSON,
routes, methods, request/response fields or types, status codes, error
envelope, headers, SSE frames, or idempotency semantics. Any correction is a
separate approved migration with its own rollback path.

The contract harness runs the same suites over HTTP against Python and Rust,
checking headers/bodies/SSE and generated TypeScript determinism. Tests compare
the frozen OpenAPI artifact's canonicalizer-v3 bytes/hash and route set, and
compare database schema fingerprints/fixture readability; do not update the
golden hash or regenerate client types to make a mismatch pass. For schema-first
DTOs, build/test verifies that generated Rust request/response types and handler
bindings compile against the frozen document. Utoipa parity is a separate
candidate only after a scratch/CI proof reaches exact canonical bytes.

### Platform and packaging matrix

| Target            | Current path supported by source/docs                                                                                                                                    | Proposed package and required evidence                                                                                                                                    |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| macOS arm64       | Signed Electron desktop, CLI/host services, Linux VM helper and FSEvents helper. Full desktop target confirmed by product guide.                                         | Bundle a versioned native Rust server beside Electron assets; preserve bundle identity/signing/state path. Verify packaged startup, SQLite, process and watcher behavior. |
| Linux x86_64      | CLI/server, systemd user supervisor, provider children and PTY; Linux VM guest.                                                                                          | Native server/CLI binary and atomic versioned links; verify signals, state lock, process groups, PTY and SQLite.                                                          |
| WSL2/Linux x86_64 | Linux setup/service plus Windows interop and firewall behavior.                                                                                                          | Same Linux binary; verify WSL detection, environment and Windows interop.                                                                                                 |
| Windows x86_64    | Windows server/install/supervisor scripts exist; no full Windows desktop target is documented. Exact currently supported CLI entry points/release artifact not verified. | Preserve established paths; test process/job and PTY behavior before claiming parity. M-07 inventory decides release promise.                                             |

Desktop package currently includes Python source and requirement lock, not a
Python interpreter; CLI and Windows setup use a selected base interpreter and
venv. The new release must package its Rust executable directly, leave the
Electron app and Claude Node bridge in place, and only remove Python after all
production launchers choose Rust. Platform source paths do not equal CI or
release proof.

### Rollback

Install Rust as a separate versioned artifact and retain the Python executable,
environment and launcher until readiness passes. At the idle cutover boundary,
stop Python and verify its state lock is released; start exactly one Rust
process on the same state directory. On failure, stop Rust, verify all DB
handles/locks are closed, restore the Python launcher and open the same state
directory. Keep failure logs/receipt. With unchanged schema and value semantics,
SQLite 3's stable file format supports cross-platform use, but older engines
are not completely forward-compatible. Preserve WAL sidecars until clean close;
never copy only the main DB while a WAL may contain committed data.

This rollback is impossible after a forward migration, incompatible stored JSON
or required enum/value, or a committed Rust action the old runtime cannot
interpret. A pre-cutover backup alone would discard user writes after cutover.
If any such change is required, stop and get a separate owner-approved
migration/recovery plan before rollout.

## Owner decisions still needed

1. **Schema authority after Python removal:** keep a committed OpenAPI document
   as authority with generated Rust DTOs and unchanged TypeScript pipeline, or
   require Rust types to regenerate the exact same canonical hash? **Recommend
   OpenAPI source for the cutover and ongoing authority** until an actual Rust
   parity prototype proves an identical hash with less maintenance.
2. **Execution model:** one Tokio multi-thread runtime with rusqlite on blocking
   workers, or synchronous thread-based internals with an async HTTP edge?
   **Recommend async edge plus synchronous domain/store ports** for the smallest
   mental model; promote more operations to async only where required.
3. **SQLite distribution:** approve pinned bundled SQLite for reproducible
   artifacts, conditional on capturing supported Python engine versions and
   proving fallback fixtures; otherwise require per-target system linkage?
   **Recommend bundled with a hard rollback compatibility gate.**
4. **Windows release promise:** should M-09 ship a Windows server binary or
   preserve only currently exercised Windows paths? **Recommend inventorying
   exact entry points in M-07 before promising release parity.**
5. **Rollback window:** can M-08.2 prohibit forward migrations and incompatible
   writes until rollback is formally retired? **Recommend yes**, with no
   exceptions inside that window.

Breaking changes proposed: none. A future intentional API hash, platform, or
database compatibility change needs a separately announced owner decision.
