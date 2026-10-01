# JavaScript modernization acceptance contract

Status: implementation authorized on 2026-10-01; verification pending.
Audience: implementers and independent reviewers of this change.

## Delivery and comparison

The original baseline is `2470fb5ac877559c70a74a8c76b3617db66cf141`
on `feat/skill-autocomplete`. Draft migration and UI changes are delivered to
that working branch after review. TanStack Query is a separate experiment on
`work/js-tanstack-query`, in the sibling `codex-studio-worktrees/js-tanstack-query`
worktree. Its comparison uses the immutable baseline, not the moving UI branch.
The Query experiment stays separate through the independent Astra review.

No live backend restart, user-state migration, publication, or real model request
is part of development verification. Tests use disposable state and hidden or
headless browsers. Existing state-directory identities and active work survive.

## Draft storage

Replace per-edit serialization of the full draft map with per-chat persistence.
Existing installations migrate automatically, with no manual storage reset.
The migration must be repeatable and recover after interruption or failed writes.
An empty or cleared newer draft must not resurrect from the legacy map.
Legacy data must not be removed before its replacement is durably recoverable.
The implementation must state how old and new tabs coexist and when any retained
legacy reader can be removed.

Text is caller-controlled; workspace identity comes from verified sync state.
Draft versions and journal entries are stored-state-derived. Local version times
are not server audit provenance. Preserve conflict handling and workspace/device
separation. Persist each accepted edit before its input handler returns; show a
storage failure without discarding the in-memory text. Immediate send must read
the latest text, and clearing after enqueue must preserve newer typing.

Checks cover legacy-only data, newer target data, clear/tombstone precedence,
repeated migration, interruption, quota failure, reload, multiple tabs, workspace
switching, offline reconciliation, IME and immediate send. Record synchronous
write count/bytes with the existing many-draft fixture, alongside measured timing
limits. Preserve the existing render-isolation guarantees.

## Component development

Storybook lives inside `web`; there is no new npm workspace or published UI
package. Use the existing React/Mantine stack and one npm lockfile. Disable
Storybook telemetry; no cloud service is required.

Organize production components by feature with reusable presentational pieces,
explicit typed props, colocated stories and interaction tests. The application
must consume the same components exercised in stories. Components should not
acquire hidden global API/storage dependencies merely to render a story.
Avoid forwarding-file hierarchies and one-off abstractions with no useful owner.

Cover composer, transcript/message variants, agent state, questions and dialogs.
Preserve keyboard/IME, focus return, scroll anchors, streaming, mobile layout,
attachments and error behavior. Component checks supplement application tests;
they do not prove the full delivery path.

## Query and durable outbox

Inventory all request sites and migrate every suitable lifecycle to Query;
document retained native transports, replication and persistence responsibilities.
Give each datum one authoritative durable owner. Do not mirror replicated RxDB
state into a competing persistent Query cache. Query must replace lifecycle code,
not merely wrap a second complete scheduler.

Query keys include verified workspace and relevant agent/resource/filter scope.
Preserve cancellation, stale-response guards, refresh/error semantics and request
ordering. Automatic retries must not replay non-idempotent actions.

Message text, attachments and delivery choice are caller-controlled. The stored
message ID identifies one logical command and is reused after response loss;
business content cannot silently change under that ID. Workspace and credentials
are verified context; refreshed credentials do not create a new logical command.
Server receipts and audit provenance remain server-owned.

The durable commit point precedes composer clearing and HTTP transmission.
Restore queued work after reload without relying on a throttled cache snapshot.
Preserve per-room ordering, offline resume, pause/cancel/edit boundaries,
multi-tab behavior, attachment recovery and visible uncertain outcomes. A lost
response uses the existing idempotent endpoint and receipt discovery; it never
authorizes a new command identity. Existing outbox records remain readable.
Do not weaken endpoint deadlines or introduce an unbounded retry loop.

Checks cover first apply, committed-response loss and reload, same ID with changed
business content, credential refresh, stale workspace/version, offline resume,
concurrent tabs, cancel races, mismatched receipts and persistence failures.
Assert authoritative server message counts, not only UI callbacks.

## Independent decision

After the combined Query implementation passes targeted and application checks,
a fresh Astra reviewer applies `$pivot1` against the baseline. Report retained
and deleted custom code, state owners, dependency/license/platform obligations,
bundle and request effects, migration/removal cost and observed performance.
Distinguish measured improvement from an estimate or fixture-only result.
The outcome may be keep, revise, or revert; adopting Query alone is not success.

Implementation owners record exact commands, commit identities, artifacts and
unverified boundaries with their results. Final review covers current integrated
heads after corrections, including the high-risk persistence boundaries.

Specification: ready with stated assumptions (local delivery; Query remains an
isolated experiment pending its review).
