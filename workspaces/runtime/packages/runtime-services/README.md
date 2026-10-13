# `studio-runtime-services`

## Change Contract

- **Responsibility:** the runtime's application rules: what a request or an
  event does to state, in which order, and what it returns. One leaf module
  per service.
- **Public interface:** each service module's functions and port traits.
- **Allowed dependencies:** `studio-runtime-store`, `studio-runtime-providers`,
  `studio-runtime-process`, `studio-operations`.
- **Forbidden dependencies:** `studio-runtime-http`, Axum, Tokio, the server
  application, the Python runtime.
- **Invariants:** synchronous code only; no service holds a store transaction
  across a call into a provider or process port; an outcome that cannot be
  observed is reported as unknown and never retried here.
- **Focused check:** `cargo nextest run -p studio-runtime-services`.

## Pattern

Transcript search uses Unicode lowercase matching, so `ß` and `SS` do not match as Python casefold would.

A service module separates pure decisions from effects. Pure functions take
snapshots and explicit clocks and return decisions; they are tested directly.
Effects go through the store's repositories or through a narrow port trait
declared in the module, with an in-memory fake for tests. A route module in
`studio-runtime-http` converts the request DTO to the service input, calls the
service inside `tokio::task::spawn_blocking`, and converts the result or error
to the response DTO.

Add each service with an explicit `pub mod` line in alphabetical order; a
directory parity test checks that module declarations match the source files.

- No build-script scanner: explicit declarations keep modules visible to Rust tooling.
- No `automod`: its upstream is archived, and rustfmt skips macro-declared modules.

Event writes persist durable pending or cancelled rows and signal the shared HTTP wake handle after commit.
Event consumption (scheduler) is a separate service and a cutover gate.
Active event-rule delivery (`rule-driver/event-rules`) is also a separate service and cutover gate.

Dependency direction:

```text
operations ← store, providers, process ← services ← http ← runtime-server
```

Profile text uses Rust `trim()`; it intentionally does not emulate Python `str.strip()` for U+001C–U+001F.

## Progress panel files

The progress panel operations mirror the Python file checks: progress and agent
directories must be ordinary directories
(`workspaces/runtime/apps/server/src/codex_progress.py:22-57`); the progress
file must be a regular file and is bounded and checked for a stable read
(`workspaces/runtime/apps/server/src/codex_progress.py:115-149`); layout files
are checked as regular files before and after opening
(`workspaces/runtime/apps/server/src/codex_progress_layout.py:36-59`). Layout
writes flush the temporary file and recheck the progress revision before
replacement (`workspaces/runtime/apps/server/src/codex_progress_layout.py:203-231`).
This Rust implementation uses ordinary path operations and does not use
anchored `openat` access. On Windows, progress
read and layout write report the named 501 dependency
`resource-policy/windows-file-identity` until compatible file identity support
is implemented.

The workspace file service resolves relative paths under the agent's `cwd`.
It also accepts absolute paths and follows symlinks without checking that the
resolved file stays inside `cwd`, matching Python's existing
[`workspace_path`](../../apps/server/src/codex_workspace.py#L567-L572)
rule. Keep this behavior deliberate; tightening it requires a separate
compatibility decision. The service pins it in
`workspace_files::tests::local_file_metadata_and_content_follow_symlinks`.

The plan that assigns services to slices is
[`docs/plans/rust-runtime-work-breakdown.md`](../../../../docs/plans/rust-runtime-work-breakdown.md).

Git changes use `std::process::Command` to preserve the Python runtime's system-Git status and patch output without a Git-library dependency.
`gix 0.89.0` (MIT/Apache-2.0; 199 normal dependency-tree entries; 9.8s clean check) and `git2 0.21.0` (MIT/Apache-2.0; 7 entries; 8.5s including libgit2 build) can inspect status and diffs but require custom rendering and can change CLI output.

## Remote transport

The outbound multi-server and federation clients keep their Python wire formats
separate. `multi-server/identity.json` stores `privateKey` as PKCS#8 PEM
(`BEGIN PRIVATE KEY`) and `publicKey` as Ed25519 SubjectPublicKeyInfo PEM
(`BEGIN PUBLIC KEY`), with `serverId` and `label`. The
`runtime_federation_identity` row with `id='local'` stores the same PEM formats
in `privateKey` and `publicKey`, with `stateId` and `label`. Federation response
wrappers are verified with the paired Ed25519 key; multi-server responses retain
Python's optional `serverId` and `publicKey` identity checks. Rust requires an
Ed25519 SubjectPublicKeyInfo algorithm identifier for stored and paired public
keys, which is stricter than Python's multi-server response field comparison.
Signature text uses strict, padded standard Base64. Federation response state is
checked when the caller supplies an expected state; a missing or null state is
accepted when there is no expected state.

Python's HTTPS clients use normal system CA trust and hostname verification;
they do not pin TLS certificates. Rust uses the platform verifier for the same
system roots. Peer identity is checked at the signed protocol layer with the
paired Ed25519 key. Multi-server identity-file loading returns the named
`UnsupportedPlatform` error on Windows.

The runtime-server binary does not install a `log` backend. ureq and
ureq-proto emit their wire traces through the `log` facade, so those records are
discarded in this binary; the service does not install or add a logging backend.
If a backend is added later, its target filters must exclude `ureq` and
`ureq_proto` because their trace output can contain request and response data.

The transport explicitly selects rustls' `ring` crypto provider through ureq's
`unversioned_rustls_crypto_provider` API. That API is unstable; the workspace
pins ureq to `=3.4.2` to keep this call aligned with the supported configuration.
ureq uses `rustls-no-provider` plus `platform-verifier`, so the lock file omits
the otherwise enabled `webpki-roots` bundle. The platform verifier brings
`rustls-platform-verifier`, `rustls-native-certs`, `openssl-probe` and
platform-specific certificate support crates into the lock graph.

## Account registry and limits read contract

This slice loads the persisted account registry and workspace limits once at
startup. Its state is immutable; request handlers project the validated
snapshot. `GET /api/accounts` is a persisted read with an authentication
recovery overlay from `runtime_usage_resumes`. Keeping the registry fresh
(refresh, discovery, and login receipts) belongs to the later account-refresh
slice. Until that exists, Rust shows the registry as Python last saved it. An
absent registry fails startup with a located error; this slice does not
synthesize account data.

Python's GET calls `Runtime.accounts_snapshot()` and
`AccountStore.snapshot(refresh=False)` (`codex_usage_resume.py:179-192`,
`studio_api/accounts/router.py:168-169`). The registry is loaded into Python's
in-memory `AccountStore` at construction and this GET does not refresh auth
files. Other Python calls can mutate the in-memory view without saving it:
`AccountStore.refresh` updates `status`, `error`, `accountId`, `email`, and
`plan` (and its private `_credentialIdentity`) at `codex_accounts.py:161-202`
without `_save`; `confirm_native_auth` changes `status`, `plan`, `error`, and
private `_authErrorKind` at `:238-253` without `_save`. The persisted Rust
projection can therefore lag those in-memory fields until Python next saves
the registry. The Rust read service does not open auth files, decode tokens,
or resolve profile directories.

| `GET /api/accounts` field              | Source                                                                                                                                                                                        | Absent, malformed, or refresh behavior                                                                                                                                                   |
| -------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `accounts[]`                           | Account rows from the startup registry, in persisted map order; underscore-prefixed private fields are removed.                                                                               | Missing registry fails startup. Malformed or schema-invalid registry fails startup with file and JSON pointer, without instance values. External changes are not observed until restart. |
| `accounts[].canAttemptNativeProof`     | Persisted Claude identity/status and keychain-error metadata plus deleted/disconnected/duplicate flags.                                                                                       | Omitted unless the saved row meets the proof eligibility conditions. No provider is started.                                                                                             |
| `accounts[].authenticationRecovery`    | A matching `runtime_usage_resumes` row with `status=scheduled`, `cause=auth`, and `failedAt` older than the Python `AUTH_WAIT_NOTICE_SECONDS` threshold (`codex_usage_resume.py:32,182-191`). | Omitted when there is no matching old scheduled auth resume. The message is `AUTHENTICATION_RECOVERY_MESSAGE` in the service.                                                            |
| `archivedAccounts[]`                   | Persisted rows with `deleted: true`, projected without private fields.                                                                                                                        | Empty when no archived rows exist.                                                                                                                                                       |
| `defaultAccountKey`                    | Registry `defaultAccountKey`.                                                                                                                                                                 | Missing or malformed registry fails startup.                                                                                                                                             |
| `logins[]`                             | Persisted login receipts; map key supplies `requestId`.                                                                                                                                       | Empty when absent. Invalid receipts fail startup.                                                                                                                                        |
| `supportsDisconnect`, `supportsDelete` | Static Rust API capabilities.                                                                                                                                                                 | Always `true`.                                                                                                                                                                           |

Each public account row is validated against the Python-exported registry schema and
the `Account` response model (`studio_api/accounts/models.py:57-77`): `id`,
`home`, `label`, `source`, and `status` come from required persisted row
fields; `provider`, `accountId`, `email`, `plan`, `error`, `disconnected`,
`deleted`, `duplicateOf`, and `claudeOptions` are optional persisted fields;
`canAttemptNativeProof` is projected from persisted Claude identity and
eligibility fields; and `authenticationRecovery` is the database overlay above.
Public extension fields are retained. Any malformed or missing required
registry field fails startup at its JSON pointer. Archived rows use the same
persisted public fields, including `deleted`, and omit private underscore
fields. Login receipt fields are read from the persisted receipt value, with
`requestId` supplied by its map key; a malformed receipt fails startup.

Limits restoration is startup-only, matching `Runtime.restore_rate_limits`
(`codex_runtime.py:9120-9138`). The Python route reads its in-memory cache at
`studio_api/accounts/router.py:220-228`; it does not query SQLite per request.
SQLite reads only the existing `sync_entities` workspace row. The storage
format is `AccountRateLimitsDto` and its nested models in
`studio_api/sync/models.py:1070-1117`; Python API restoration then validates
`UsageLimitsResponse` (`studio_api/accounts/models.py:362-369`). The reader
consumes the exported schemas in `runtime-schema/src/schemas/account-registry.schema.json`
and `runtime-schema/src/schemas/workspace-usage-limits.schema.json`; the latter validates
selected entries through its `UsageLimitsResponse` model.

| `GET /api/limits` field                             | Source                                                                                                                                 | Absent, malformed, or refresh behavior                                                                                                                                                                                      |
| --------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `accountKey`                                        | The matching persisted account cache entry; legacy `rateLimits` is the default entry only when `rateLimitsByAccount` has no `default`. | A missing or mismatched cache is uncached.                                                                                                                                                                                  |
| `data`                                              | Persisted `AccountRateLimitsDto.data`, accepted for serving only when it also passes Python's `UsageLimitsResponse` shape.             | Missing or null data is not restored. An invalid restored snapshot is treated as absent, as in Python's restore validation; it carries a safe diagnostic with workspace row, account key, JSON pointer, and schema keyword. |
| `at`, `readAt`, `checkedAt`, `processedAt`, `error` | Optional persisted cache metadata.                                                                                                     | Wrong types make that account cache absent and produce a value-free diagnostic.                                                                                                                                             |

A cached request can return a valid persisted response. A request that needs a
refresh returns 501 with dependency `provider-session/account-rate-limits`;
missing cache and intentional `data: null` are normal uncached states. An
unknown account with no valid cache returns JSON 400 `Unknown Codex account`.
Invalid workspace JSON/schema is reported as a safe `workspace/current`
diagnostic; persisted documents share the store-owned 64 MiB and 128-level
bounds, with one iterative depth scanner before recursive parsing. No debug
formatter retains or prints registry or limits values.

The exported `runtime-schema/src/schemas/account-registry.schema.json` is the registry
authority. The registry is one fail-closed authority:
invalid JSON or a schema violation stops startup with the path and first
location, without field contents. A literal unpaired surrogate produced by
Python's `set_name` writer is the one known product-writable registry shape
this Rust reader refuses rather than lossily decoding. Schema-tightening's
read-only pre-cutover state check is the guard; if it finds one, the lead owns
the one-time state upgrade. No Python truthiness or malformed-file
compatibility rules are reproduced.

`runtime-schema/src/schemas/workspace-usage-limits.schema.json` is a projection schema: it
validates the workspace usage fields this slice reads, while intentionally
ignoring unrelated workspace fields. It is not an allow-list for the complete
workspace format.

The limits restore policy intentionally differs from Python's
`Runtime.restore_rate_limits` (`codex_runtime.py:9125-9138`): Python's
single exception boundary aborts the full restore loop on the first invalid
entry. Rust isolates a refused account entry and continues restoring other
valid entries. The legacy `rateLimits` snapshot is considered only when the
raw `rateLimitsByAccount` map has no `default` key (`codex_runtime.py:9128-9129`);
a present but refused default entry does not revive legacy data.

The producer corpus includes a registry above the old 1 MiB ceiling, written
through `AccountStore.set_name` with its untrimmed rename receipt
(`codex_accounts.py:385,394,403`).

### Later account-refresh boundary

Python resolves Codex homes and rejects only a resolved `.claude` path
component (`codex_state.py:22-26,39-40`). Claude configuration is selected by
`profile_options` and `subscription_env`, with the `~/.claude` fallback
(`codex_claude.py:61-80,150-161; codex_claude_login.py:228-229`). This slice
intentionally defines no HOME/cwd allow-list or auth-file symlink policy; the
refresh owner must use the product's supplied configuration roots and own the
final auth-file boundary. Python permits external explicit/profile paths and
follows filesystem resolution before its `.claude` exclusion.

The Python account mutation points that a later owner must port are `refresh`,
`confirm_native_auth`, `default`, `disconnect`, `delete`, `reconnect`,
`set_name`, `register`, `register_claude`, `update_claude`, `discover`,
`start_login`, `login_receipts`, `cancel_login`, `login_completed`, and
`set_rate_limits`. This read slice implements none of them.

`GET /api/models` remains unimplemented with dependency
`provider-session/model-list`. Python connects the account and sends paginated
`model/list` requests with `limit: 100` (`router.py:176-209`,
`codex_catalog.py:130-170`); `ModelCatalogCache` is process memory with a
300-second successful-snapshot TTL and a five-second wait
(`codex_catalog.py:32-43`). Pending and unavailable replies are HTTP 400
(`router.py:198-207`). The Rust route stays at 501 until a provider session
owns connection, pagination, cache, timeout, and pending/unavailable states.

The producer test uses Python `AccountStore` writes and the real
`Runtime.store_rate_limits` → `codex_sync_entities.patch`/`put`/`project` path
inside a disposable root and SQLite database. It does not start a provider or
make a network request. Run it with
`cargo nextest run -p studio-runtime-services --test account_policy_differential`.

### Account driver process boundary

The account driver's Claude metadata reader runs `claude auth status --json`
with an eight-second timeout. `process-wrap` places the command in a POSIX
process group, so timeout and output-limit termination cover its child
processes as well. Windows retains direct-child termination and is outside this
Rust cutover. The command's output is parsed in memory and is never logged.
On macOS, Rust currently reports the named reason
`macos-passive-auth-unavailable` and leaves the previous account status
unchanged. Python can read passive Keychain authentication through its
Security.framework helper; this slice does not add platform APIs or a new
dependency to reproduce that behavior.

## Monitor runtime

[`monitor_policy`](src/monitor_policy.rs) checks Python-written monitor and agent
records before producing pure state transitions. [`monitor_runtime`](src/monitor_runtime.rs)
commits monitor rows, approval expiry, canonical sync projections, and exit/stall
notifications in one transaction. [`monitor_driver`](src/monitor_driver.rs) calls
an injected native transport only after that transaction commits. A bounded striped
lock serializes launch, input and cancellation through submission; a concurrent
request is refused while a native operation is pending, before any write. Preparation
supplies the exact actor, monitor, operation and permission-bearing native params;
a changed actor or monitor rejects the prepared submission.

The account worker implements `MonitorNative::submit`, validating its current
connection ticket and serializing native I/O with stop. `Accepted` means transport
admission; callbacks persist output through `MonitorOutput::append`, input replies
through `settle_input`, and definitive exits through `monitor_receipts::finish`.
Output uses exact byte offsets: a duplicate receipt compares existing log bytes
without extending the log or its committed cursor. Results are fsynced before the
monitor/event transaction and acknowledged only after commit. Recovery consumes
exact receipts without repeating commands. Lost input acknowledgements remain
blocked until their exact ticket is resolved.

Periodic callers run `tick` and consume returned probe effects after commit.
Workspace preparation and native sandbox policy apply to probes as to commands;
their result calls `complete_probe`, which rejects a stale activity generation.
`reattach` requires exact operation proof from the current supervisor connection
and cancels only a pending provisional disconnect notice. Never reconstruct
process ownership from persisted PIDs.

The rules integrator implements `RuleFinish` using
`rules_tick::finish_in_transaction` in the same transaction as the monitor result.
That module is not on this base. Native preparation/session, rule finish and
cross-server effects retain named refusals until their adapters are installed.
HTTP cancellation currently supports terminal and prelaunch local monitors;
running cancellation and input remain partial. A scripted transport or harmless
local child test is not evidence of a live provider run.

Targeted checks are `cargo nextest run -p studio-runtime-services --test monitor_corpus
--test monitor_recovery` and the `rust-monitors-http-contract.py` suite against
both backends. The differential oracle imports real Python writers behind the
shared provider-home/process guard. `STUDIO_RUNTIME_CORPUS_FULL=1` includes the
complete store-boundary matrix. Its mutation manifest is
[`tests/monitors/mutations.json`](tests/monitors/mutations.json), consumed by the
shared `tests/turn_lifecycle/mutation_check.py` runner with `--manifest` and
`--filter 'binary(/monitor_(corpus|recovery)/)'`.
