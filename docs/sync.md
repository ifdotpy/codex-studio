# Entity sync tombstone reset contract

Entity sync uses the `state:entities:v1` scope and one sequence space. The
server retains the newest 10,000 non-transcript tombstones. At the observed
35,000 task tombstones per day, this is about 6.9 hours of replay. An entity
pull never waits for pruning. A background worker deletes at most 500 rows in
each committed transaction, then pauses for 150 ms before the next batch. A
transactionally maintained tombstone count and tombstone-only sequence index
keep each batch selection bounded. Entity writes do not run pruning work. Live
entities are never pruned. The greatest
pruned sequence is stored as `sync_entity_meta.entity_tombstone_floor` and is
included in `maxSeq` even when no retained entity has a sequence that high.

## Request and response

An entity client that implements this contract adds `reset=1` to each pull:

```text
GET /api/sync/pull?scope=state%3Aentities%3Av1&after=4850997&limit=500&reset=1
```

If `0 < after < floor` on an ordinary delta pull, the server returns this
explicit response, with no documents or ordinary checkpoint:

```json
{
  "workspaceId": "<workspace-id>",
  "reset": true,
  "floor": 5000000,
  "maxSeq": 5100000
}
```

The actual `floor` and `maxSeq` are integers. The reset response is sent only
when the request declares `reset=1`. A caller without that parameter keeps the
ordinary response shape and receives whatever retained changes follow its
cursor. This lets older renderers and Rust clients continue parsing responses
during rollout.

Every `after=0` pull returns live rows and only tombstones with `seq > floor`.
It includes all live rows regardless of their sequence. Pages remain ordered by
sequence, and the final page checkpoint reaches `maxSeq`, which is at least the
floor. This also applies to callers without `reset=1`.

Use this same `fresh=1` baseline for an initial full load. After receiving
reset, the client starts it again at `after=0`. Keep `fresh=1` and the returned
`initialHigh` on every page of the baseline. This marks continuation of the
full replacement and prevents a second reset response while the checkpoint is
moving through live rows whose sequences are below the floor. The server
includes all live rows and tombstones created after `initialHigh`; then the
client continues with ordinary delta pulls.

## Required client steps

1. Send `reset=1` on entity pulls. Do not add it to drafts or transcript pulls.
2. On `{ "reset": true }`, hide the current entity projection and persist a
   resetting marker before clearing entity documents. Reset the entity
   checkpoint to sequence `0` and clear the initial-load marker. Keep drafts
   and outbox documents intact.
3. Clear only local entity documents. Pull again with `after=0` and `reset=1`,
   and persist pages and checkpoints in sequence order.
4. Keep the projection hidden while any page is incomplete. Publish it only
   after the checkpoint reaches that response's `maxSeq`, then mark the
   projection ready. If the client stops during reset, resume the clear/pull
   operation before publishing.

The server floor may advance during pagination. A page set is complete when
its checkpoint reaches the `maxSeq` returned by that page sequence; subsequent
entity invalidations start another pull from that checkpoint and may request a
new reset if the client has fallen behind the newer floor.
