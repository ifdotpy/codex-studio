# Runtime migration policy

This page defines the contract for maintainers changing Studio-owned persisted
formats. Operator delivery steps belong in [Upgrade](upgrade.md); shared type
ownership belongs in [Type contracts](type-contracts.md).

## One migration owner

Server migrations belong together under
`workspaces/runtime/packages/runtime-store/src/migrations/`. One typed
`Migration` trait and an explicit ordered catalog own their execution. A dated
module names each historical transition. The current format keeps its existing
schema, codec, and validator owner; a migration calls that owner rather than
copying its destination representation.

Each entry must identify its stable date and name, supported source format,
destination, target storage, preconditions, and removal condition. Target and
phase are typed values. A migration must not receive an optional bag of unrelated
database and file handles. Dates describe transitions; they are not expiration
timers.

Canvas and analytics record a fingerprint of their ordered SQL migration IDs and
the SQLite schema version in their existing metadata, in the same transaction as
the target's migrations. An unchanged catalog and schema skip those bodies on
reopen; changing either runs the target again. Keep completed migration IDs and
behavior stable, and add a new ID for a later transition. Receipt families retain
their own phase checkpoints.

Convert known old owned formats before normal readers and workers use them.
Remove the corresponding compatibility branch from the normal path. A malformed
current-format record is an error, not permission to reapply historical defaults.
Unsupported source versions must be refused without guessing their meaning.

## Startup and ownership

The runtime must hold its existing state-directory lock and validate migration
preconditions before changing existing state. Successful migration and strict
current-format validation are prerequisites for request serving and worker
admission. All normal server launch paths use the same startup barrier. While
startup is in progress, the server answers `GET /health` with a `migrating`
status and the current receipt-family source and rowid position (`done` and
`total` are rowid positions, not row counts). Other endpoints return HTTP 503
until migration and startup complete. A restart resumes committed receipt
batches; it does not serve commands against partially migrated data. Serving
commands during data migration would require compatibility for every affected
reader and writer and remains a future architecture follow-up.

The supervisor owns its journal under its own lock. It uses the same migration
contract before its own recovery and listener startup. The server must not
migrate a journal owned by a running supervisor. Fresh-state creation remains
distinct from upgrading an existing state directory.

## Failed steps roll back; completed steps remain

Canvas, analytics, and separate files do not form one atomic transaction. The
rollback guarantee is per transactional step:

1. Validate the known source and destination prerequisites.
2. Apply a SQL step in its owning transaction, or a file step through its owning
   atomic replacement operation.
3. Roll back an unsuccessful uncommitted SQL step. Keep previously committed
   steps and block startup when a later step fails.
4. On restart, validate completed steps and safely continue the remaining work.

A failed directory synchronization after file replacement leaves durability
uncertain. A lost commit response also does not prove rollback. Diagnostics must
distinguish these outcomes from a confirmed rollback; restart inspects retained
state before proceeding. No step may replay a native command, user message, or
charge to resolve migration uncertainty.

There is no automatic restoration of the whole state directory and no rollback
over newer application writes. Release rollback changes executable artifacts;
it does not reverse committed data migrations. An older executable must either
understand the current format or refuse to open it.

## Preserve evidence and storage boundaries

Conversions preserve exact request identities, immutable receipt meanings,
historical evidence, and unsettled outcomes. Missing historical evidence cannot
be replaced with an invented identity, delivery result, or completion. Recovery
that requires fresh provider or supervisor evidence remains a typed runtime
operation rather than a guessed startup conversion.

| Data owner                                           | Correct conversion boundary                                               |
| ---------------------------------------------------- | ------------------------------------------------------------------------- |
| Studio server databases and account registry         | Locked runtime startup before normal access                               |
| Supervisor journal                                   | Supervisor startup under its own lock                                     |
| Browser or native client private storage             | That client's storage initialization or explicit receipt-backed import    |
| Provider-owned native history and external wire data | Validated provider adapter; Studio does not rewrite the vendor's evidence |
| Independent project stores and mounted workspaces    | Their own store and mount owners                                          |

Remote server startup cannot migrate storage that exists only on a client.
Explicit profile, credential, and workspace imports remain explicit operations
with their own identity and permission checks.

## Removing a migration

An entry can be removed only after supported installations and supported restore
sources no longer require its source format. Document the new minimum supported
source and keep a refusal test for retired formats. Remove the catalog entry,
historical conversion helper, and obsolete conversion tests together. Keep the
current validator and any immutable receipt codec still needed by retained
evidence. Never delete data or enable replay merely because an entry is old.

## Required evidence

Exercise the actual store or process startup caller using disposable old-format
fixtures. Verify destination readers, a second unchanged open, and failures at
transaction and file publication boundaries. Check that a failed step preserves
its source, earlier completed steps remain valid, and startup stays unavailable
until all required steps succeed. Retained records, journals, request receipts,
and uncertain operations must keep their meaning. Source inspection and fixture
checks do not establish a successful migration of a live installation.
