# Workspace task feed verification

## Behavior

`GET /api/workspace?view=work`, `view=inbox`, and `view=annotations` return independently cacheable representations. `GET /api/workspace/tasks` returns an initial window of at most 100 tasks, then records newer than the client's `(updated,id)` cursor. `updated` is derived from each task's existing `created` and `finished` timestamps. The route also accepts `before` for older pages. Task summaries omit output tails, arguments, and errors.

The task drawer polls the scoped feed every five seconds while open. It merges task updates, keeps a 100-item window, and removes completed tasks from the active list. The Changes section requests only annotations.

## Fixture measurement

Measured through the local `Canvas` HTTP handler with gzip negotiation. No live backend or DB was used. The fixture seeded 320 work records and 350 active tasks, created 20 more commands per minute at three-second intervals, and polled every five seconds for one minute. The full workspace response averaged 729,941 decoded bytes per poll in this fixture.

| Request path | Poll window | Statuses | Body bytes | Total request + response bytes |
| --- | ---: | --- | ---: | ---: |
| Before: full `/api/workspace` | 12 polls | 12 × 200 | 1,097,615 gzip; 8,759,292 decoded | 1,107,191/min |
| After: visible task drawer feed | 12 warm polls over 60 s | 12 × 200 | 9,376 | 20,576/min |
| After: unchanged work view | conditional polls | 304 | 0 per response | 0 body bytes |
| After: unchanged inbox view | conditional polls | 304 | 0 per response | 0 body bytes |

The task-feed measurement includes HTTP request and response headers. Its warm body total was 9,376 bytes; the remainder was HTTP overhead. The initial bounded-window load occurred before the measured minute. The full-workspace fixture is larger than the observed live response, so the baseline is conservative for this fixture.

## Checks

- `tests/workspace-contract.py`: 44 passed, including scoped incremental changes and older-page retrieval.
- `tests/mobile-state-contract.py`: 9 passed, including independent validators and HTTP 304 behavior.
- `tests/runtime-contract.py`: 59 passed.
- `npm --prefix web run build`: passed.
- `tests/workspace-task-feed-ui.mjs`: passed; the open drawer displayed a fixture command and removed it after a completion update, within the five-second feed interval.
- `tests/workspace-ui.mjs`: stops at `plan agent has a native thread` in its fixture, before reaching the new feed assertions. The isolated task-feed UI check above covers the changed caller.

## Live-patch surface

The changed handler method in the `make_server` closure is `Handler.do_GET`. Runtime methods called by that route are `Runtime.workspace_part`, `Runtime.workspace_task_feed`, and `Runtime._workspace_task_summary`. No DB migration is required.
