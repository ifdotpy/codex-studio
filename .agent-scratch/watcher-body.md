## Request

The owner asked for a watcher of agents that Studio errors failed: one list of agent work that ended failed, lost, uncertain or stuck because of a Studio error, not because of the agent's own work, with the reason and a safe action, and without repeating any input or command.

## Measurement (live state, last 7 days, read only)

Studio-caused, by kind:

- Monitors: 99 lost after a backend restart, 29 lost after Studio lost the provider result (#443), 16 barrier failures (#442), 25 "changed during preparation", 4 configuration reads with an unknown outcome, 1 startup deadline, 1 unreadable Claude sign-in.
- Input: 11 "Provider did not send turn/start", 11 not confirmed before the turn ended, 5 resume outcomes unknown, 1 resume ENOENT, 2 turn/start timeouts still uncertain.
- Agents: 2 workspace recovery or token-stale failures, 1 missing VM helper.

Excluded as the agent's own: exit codes, stops, provider policy flags.

## Change

- `runtime-services::studio_errors` classifies the last 7 days of monitors and input events, and the current agents, into eight kinds. Each kind has one plain reason and one action:
  - `resume` (connection recovery, which reads native history and never sends uncertain input again) for uncertain or stuck input and a disconnected turn;
  - `check` (open the agent) for the other kinds, where only the owner can decide.
- `GET /api/studio-errors` returns the rows, newest first, at most 200. `POST /api/studio-errors/dismiss {id}` hides a row; dismissing it again changes nothing. Both are owner-only through the existing API boundary.
- The sidebar shows "Studio errors" with a count when rows exist, and a list with Resume or Open and Dismiss per row. Resume calls the existing `/api/connection-recovery`. Nothing in the list runs a command or sends input.
- The error texts that the classifier reads have one owner in `shared_values` (`PROVIDER_START_UNKNOWN_ERROR`, `PROVIDER_START_NOT_SENT_ERROR`, `PROVIDER_RESUME_UNKNOWN_ERROR`, `MONITOR_OWNER_STOPPED_ERROR`), and their producers use it. `recovery_policy.rs` still has its copy of the start-unknown text, so this PR does not conflict with #450; it can use the owner after #450 merges.

Not in this first slice: "Retry once" for a monitor proven not started. No such case occurred after 21 October, and a retry would need a new monitor identity. It can follow if new cases appear.

## Tests

- `studio_errors::tests` (runtime-services): each Studio cause gets its kind; an exit code, an owner's stop, a young pending input, a user stop and a provider policy flag get none; only kinds that history decides offer `resume`. Added to the short tier.
- `studio_errors::studio_errors_list_studio_caused_endings_and_hide_a_dismissed_row` (runtime-http integration): a lost monitor is listed with its kind, action and agent name. A monitor that ended with an exit code and an owner-stopped one are not listed. A dismissed row stays hidden after a repeated dismissal.
- `StudioErrors.test.ts` (web): rows show the reason, subject and one action; an empty list says so.
- Focused: runtime-services, runtime-http and runtime-store nextest selection for `studio_errors`, route registration and stored-shape tests, scheduler, and the moved-text callers: 134 passed. runtime-server `monitor_port::`: 14 passed. `pnpm run contracts:check`: clean. Clippy on the five crates: no diagnostics in the changed files. Web `tsc --noEmit`: clean except the missing generated WASM module, which this checkout did not build. `Sidebar.test.ts` and `StudioErrors.test.ts`: passed.

Full tiers: in the coordinator's batch gate.
