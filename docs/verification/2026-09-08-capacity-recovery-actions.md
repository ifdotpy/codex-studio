# Capacity retry and error recovery actions

## Contract

A known terminal `serverOverloaded` failure schedules a new native turn with empty input.
Studio shows Retry now, a countdown, and Cancel automatic retry.
The server stores the deadline and action identity. Browser timers only display time.
Two tabs and a lost HTTP response must not submit two native turns.

The schedule is 10, 30, 120, then 300 seconds. After four continuations,
only manual retry remains. A new instruction resets the schedule.
Other native failures keep their existing failure hold.

The failed user message, attachments, and commands are not replayed.
Pending events stay held until the capacity continuation succeeds or a new
instruction replaces the hold. A retry preserves the selected native permissions,
service tier, reasoning effort, account, and thread.

## Durable boundaries

- SQLite stores one retry record per agent, account, thread, epoch, and failed turn.
- `POST /api/capacity-retry` takes `id`, `retry_id`, and `action` (`retry` or `cancel`).
- The server commits the claim before it queues native work. Responses return the record.
- A repeated action on a claimed identity returns its stored state.
  It cannot start another turn, even after a known start rejection.
- Cancellation affects an unclaimed automatic retry. It does not interrupt commands or agents.
  If submission already owns the retry, cancellation returns that state.
- Restart cancels unsent automatic retries. Submitted work with an unknown outcome stays unknown.
- New input releases an unsubmitted capacity reservation before it enters the queue.
  Submitted reservations retain their identity until native acknowledgement.
- Terminal events that precede acknowledgement cannot authorize a retry until the exact
  native turn identity is confirmed.
- Submission checks the account, thread, epoch, workspace, model settings, policy block,
  pending approvals, token budget, and concurrency limits.
- Existing runtime updaters reject instances without the capacity retry mixin.
  They cannot install only part of this feature.

## Error text and limits

The error view extracts a readable message from complete JSON, fenced JSON, and JSON
inside an HTTP error. Details retain the original payload. Parsing has a size and depth
bound. Native error kinds remain authoritative. Bio-policy notices use exact codes and prefixes.

The limit banner and limit popover use one classifier. It checks account scope and freshness.
It shows the latest reset among exhausted windows, so an earlier reset cannot imply full recovery.

- Workspace owners get a billing or usage-limit link.
- Workspace members can copy a request for their owner. Studio does not send that request.
- Personal plans get a ChatGPT usage or credit link.
- Unknown or stale data keeps neutral refresh guidance.

External links come from the inspected ChatGPT app bundle documented in
`2026-09-08-chatgpt-app-errors.md`. The interface asks the user to use the same account
in ChatGPT. Opening a link does not purchase credits or change a limit.

## Verification

- 22 deterministic capacity contract tests cover schedule, identities, acknowledgement loss,
  cancellation, restart, stale events, guards, pending input, and terminal-before-acknowledgement.
- Existing runtime (48), critical runtime (10), native error (32), native error updater (5),
  and turn recovery (17) checks pass with CPython 3.14.
- The installed Codex schema check covers all 18 error kinds and wrapped JSON cases.
- Limit classifier checks cover role, plan, stale data, account scope, links, and reset time.
- The capacity browser test uses two pages, SQLite, and a native fixture.
  It checks cancellation beyond the deadline and after reload, a lost HTTP success response,
  duplicate Retry, empty native input, draft retention, and a real ten-second scheduler delay.
- Native-error and limits browser checks cover readable JSON, policy classification,
  owner actions, copy-request text, and mobile bounds.
- TypeScript and the production build pass. Desktop checks use hidden browsers.

The checks use isolated state directories. They do not send paid model requests,
redeem real reset credits, or restart a live backend. Activation requires a later
server start after active tasks and monitors finish.
