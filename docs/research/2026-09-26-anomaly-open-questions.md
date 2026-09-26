# Anomaly audit follow-up, 2026-09-26

## Scope and method

I read the audit and the anomaly sections in the Codex, Claude/Rust, OpenCode, and T3 Code reports. I inspected the current source and used SQLite URI `mode=ro` for the state measurements below. The database snapshot was read at 2026-09-26 01:32:41 UTC. Its file size was 32,912,437,248 bytes. I did not query or scan `runtime_items`. Local native rollout checks used exact thread file names and exact event IDs. No native history request or state change was made.

## Open questions

1. **The 74 uncertain events.** The current event table has 49,237 rows. It has 74 uncertain rows: 30 agent messages, 23 followups, 12 child results, 5 user messages, 2 complaints, and 2 work reviews. I checked the matching default Codex rollout files for exact event IDs. Thirty-two threads had no matching local file; the other 42 files had no matching ID. This does not check other account homes or the live native `thread/turns/list` response. `orchestration_agent_manage recover` reads that history, then may mark an input delivered or put it back in the queue. That action is not read-only, so I did not call it. **Cause:** the saved event row does not retain enough native evidence. **Classification:** intentionally unknown until exact native evidence is read. **Action:** retain all 74 as unknown in this audit. Do not infer delivery or retry permission from `Codex app-server is offline` errors.

2. **The browser recovery count.** The incident report's “Browser recovery: 15” is the count of browser recovery contract tests, not 15 live recoveries. It also reports two controlled live recovery runs. The database has two `browser_recovery` delivery events, and two agent records have recovery state, one `verified` and one `failed`. **Cause:** the report and database count different things. **Classification:** intended measurement difference. **Action:** report test and live counts separately. The failed state stops automatic retries by design.

3. **The 66 native notices.** All 66 rows are in `runtime_native_notices`, the configuration notice store. Queue notices are saved as `queueNotice` on agent records and sent through the normal `send(..., delivery="steer")` path. Fifteen agent records retain a latest `queueNotice`; this is not a count of all notices ever sent. **Cause:** queue notices do not use the native configuration notice table. **Classification:** intended separate records. **Action:** keep these counters distinct. Add a durable queue-notice history only if historical notice rates become a product need.

4. **Usage resumes.** `runtime_usage_resumes` has zero rows, and no agent record has a `usageResume` field. This proves there is no retained resume record in this snapshot. It cannot prove the scheduler never fired because completed records may be removed. **Cause:** no retained usage-resume history. **Classification:** inconclusive usage, not a defect. **Action:** add durable completion metrics before drawing a firing-rate conclusion.

5. **JSON decoding cost.** The database contains many JSON records, but there is no decode-time, lock-hold, or table-specific attribution in the inspected measurements. Existing incident notes measure callback and lock delay, not the share caused by JSON decoding. **Cause:** missing timing instrumentation. **Classification:** unmeasured design risk. **Action:** instrument representative hot reads and lock spans before planning a schema migration.

6. **The original browser failure.** The incident records `Browser is not available: chrome` on the affected native thread, including an explicit probe. A new session could connect. Unsubscribe and resume with current settings restored discovery in the same thread. The retained native logs had no low-level pipe failure for the incident window. **Cause:** still unknown. A loaded thread retaining stale resume settings explains the verified recovery path, not the original failure. **Classification:** unresolved incident cause; recovery is intended. **Action:** capture native transport details with thread, item, connection, and request IDs if it recurs.

7. **A native replacement for context repair.** The inspected Studio path reads native turns and receipts, then can sanitize a verified Codex rollout and fork a thread. I found no Codex and Claude contract that imports a checkpoint while preserving exact token budgets and tool receipts. **Cause:** Studio owns input and receipt state outside provider history. **Classification:** intended guarded recovery path, with design debt. **Action:** retain it until a provider API contract and compatibility tests prove a replacement.

8. **Radio versus regular shared chat.** Radio adds a saved two-person speaker floor, ordered turns, pass and stop actions, question routing, and revision checks. Regular shared chat also orders replies but does not use the radio floor state. The database has 567 rooms, 555 private and 12 broadcast, with no radio room and no `radio_turn` event. **Cause:** the feature has no recorded use in this snapshot. Its code is called from the runtime scheduler and message paths. **Classification:** inactive capability, not dead code. **Action:** use radio-specific usage metrics before considering removal.

9. **Complaint response rules.** The database has 23 open and 24 in-progress complaints. Open complaints have ages from 0.1 to 13.1 days; in-progress complaints have ages from 2.6 to 19.4 days, with an 18.3-day median. A lead completion is blocked only by an unanswered complaint addressed to that lead. An open user task also needs a user response. The code records three missed presented lead complaints before it blocks that lead. **Cause:** complaints remain open until the responsible party replies or resolves them. **Classification:** intended, with old items needing review. Open or in-progress status alone does not block every lead. **Action:** keep explicit response ownership; do not expire complaints automatically.

10. **Live patch receipts.** The current `live-update.json` says `applied` for manifest `turn-delivery-65c6f7a`, attempt 1. It records one current process ID and application time. It is overwritten by later receipts, so it cannot count all successful patches or process generations. The 198 `runtime_native_catalog_updates` rows are a separate mechanism. **Cause:** no append-only live patch receipt history. **Classification:** current activation verified; historical count inconclusive. **Action:** retain the receipt and add durable history only if generation-level reporting is required.

## New findings

1. **Orphaned account transfers, confirmed defect and fixed.** The snapshot has five pending transfer records. None is referenced by an agent's `accountTransferId` or `accountTransfer` summary. All five contain only `waiting` or `completed` members. Each lead now has a different terminal transfer summary. The oldest record is 8.3 days old. `AccountTransfers.tick` only advances transfers that an agent still references, so these records have no route to finish. A stale `save` could also replace a newer terminal summary when the stale operation had no exact pointer. I changed [codex_account_transfer.py](../../scripts/codex_account_transfer.py) to preserve a different current summary and cancel unreferenced pending records only when every member has no submitted or unknown native mutation. The normal durable `save` path records cancellation. The scheduler does not touch the live database until a future source activation.

2. **Pending messages on interrupted agents, held by design.** There are seven pending `agent_message` events. Six are 2.2 days old on interrupted agents with `autoWake=false`; their event epoch still matches the agent. One is 0.2 days old on a queued agent with `autoWake=true`. Interrupted agents do not wake automatically. The lead can resume them with `orchestration_send`. **Classification:** deliberate hold for stopped work, not proof of a delivery defect. **Action:** keep the explicit resume path; show age and stopped state when reviewing long queues.

3. **Failed browser recovery, safe stop.** One agent record retains a failed recovery with “No browser is available.” The other is verified. The recovery code stops after failed verification and does not replay browser actions. **Classification:** intended safety stop. **Action:** restore browser availability before a later explicit resume; preserve the failure detail.

## Harness anomaly classifications

The four harness reports describe comparison points and design risks. Their source comparisons alone do not prove a Studio defect. These are the distinct Studio mechanisms they cover:

| Studio mechanism | Cause and evidence | Classification | Action |
|---|---|---|---|
| Mirrored turn state | Durable scheduling and UI state mirror native turn callbacks in `codex_runtime.py`. | Intended boundary; risk of stale projection. | Keep native reconciliation and measure drift before redesign. |
| Global callback queue and runtime lock | Shared state and ordered receipts cross agent boundaries. Incidents show queue and lock delay. | Intended ordering with measured performance risk. | Measure lock and callback spans; do not remove ordering without ownership tests. |
| Queue, steer, and after-tool delivery | Each mode has different native timing and retry rules. | Intentional distinct semantics; complex API. | Keep exact receipts; consolidate only where native contracts match. |
| Queue-age notices | Queued input cannot reach an active turn. The notice uses the normal steer path. | Intended workaround for queued delivery. | Keep until input delivery changes; distinguish notice state from native configuration notices. |
| Context repair and rollout rewriting | Studio input receipts can diverge from native history. | Intended guarded recovery; high maintenance cost. | Keep identity and history checks until a safe provider replacement exists. |
| Detached command monitors | Commands can outlive a model turn and have separate receipts. | Intended capability; duplicate command lifecycle. | Keep detached monitor behavior; consider one receipt ledger later. |
| Capacity and usage retries | Native terminal capacity errors and account quota windows have different evidence and timing. | Intended separate recovery policies. | Do not merge retry rules without preserving each authorization boundary. |
| Browser reconnect and probe | Loaded browser runtime settings can be stale. Recovery reconnects and verifies without replay. | Intended recovery; original incident cause unresolved. | Keep one guarded attempt and collect transport evidence on recurrence. |
| Claude protocol bridge | Claude Agent SDK maps into Studio's Codex-shaped runtime contract. | Intentional provider adapter; semantic drift risk. | Keep provider-specific behavior behind tested contracts. |
| Managed worker fleet and budgets | Studio supports cross-provider workers, durable tasks, worktrees, and tree budgets. | Intended Studio behavior; some duplication versus native delegation. | Preserve mixed-provider and worktree guarantees before using native delegation. |
| Split operation receipts | Studio records events, operation receipts, and native receipts at different mutation boundaries. | Intentional safety records; possible consolidation debt. | Keep uncertainty explicit. Consolidate only with exact identity and no-replay tests. |

No dead code was proved by the four source comparisons. The radio snapshot shows no recorded use, but runtime callers remain. No new skill or README contradiction was confirmed in the inspected paths.

## Verification

- Read-only SQLite URI query at 2026-09-26 01:32:41 UTC. No `runtime_items` scan.
- Exact-ID local rollout check for 74 uncertain events. It did not query other account homes or the live native history API.
- `python3 -B tests/account-transfer-contract.py`: 38 tests passed.
- No live backend restart, live database edit, browser action, or application change.
