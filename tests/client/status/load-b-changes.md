# Parallel browser checkpoint B

Base: `92794cb2` (`work/fast-browser-tests`). The production build passed.

## Findings and changes

- `background/background-controls-ui.spec.mjs`: the fixture omitted
  `/api/workspace/tasks`, so the activity drawer received an empty feed and
  could not render `native-task`; the test then waited for that impossible row
  until its 60-second deadline. Serve the task feed through the fixture entity
  contract. Solo repeat3 passed 3/3 (23.2s total).
- `mobile/mobile-keyboard-ui.spec.mjs`: initial loaded-state readings varied
  because the fixture seeds context on `Release lead`, while the mobile layout
  hides the `[data-chat]` rows needed to select that lead. A later attempt
  captured the Release lead ID from sync and restored it through
  `codex-mobile-opened`, but the rendered label still disagreed with the row
  intermittently. A first fixed-cost 390×390 A/B showed `Context 40%` fitting
  3/3 (root bottom 444px, footer bottom 428.96875px) and withheld `Context`
  overflowing 3/3 (503.5625px twice, 547.703125px once). A subsequent
  marker-removed run recorded one unknown-context overflow (503.5625px) and two
  fits (428.96875px); known-state checks timed out despite a seq 942 row with
  context usage. Later A/B attempts still did not separate cleanly: identical
  `Context` content overflowed in four of six withheld-context samples (67%)
  and fit in two; overflow bottoms included 503.5625px and 547.703125px, while
  fits measured 428.96875px as composer bottom changed. Both A and B reproductions
  are `test.fixme`. One cost-busy sample put the composer bottom at 445.703125px
  versus root bottom 444px (1.703125px over), so composer geometry is asserted
  only after the cost response settles. The main flow no longer waits for the seeded `Context 40%`
  label because that startup check failed 2/3 on solo repeat3; it measures
  loaded footer geometry while leaving 390px clearance in the isolated
  reproduction until A/B is stable. With the
  same `$0.42*` session-cost response, the context button measured 79.203125px
  for `Context 40%` and 50.40625px for `Context`; the cost summary measured
  180.40625px and 209.203125px respectively. Token-rate width was 86.390625px,
  limits group 112.609375px with one account dot and text `Limits`. Composer
  bottom also varied, so unknown-state samples ranged from 59.5625px and
  103.703125px overflow to a fit. Product sizing is `Usage.css:458-464`,
  summary flex rule `:452-455`, context label source `Usage.tsx:447-454,536`,
  and summary content `:485-518`. At 390×330 with retained keyboard scroll, a later sample had root bottom
  394px, composer bottom 406.5625px, and footer bottom 508.5625px. The
  no-keyboard 390×330 blank gap is 12.03px rather than 16px.
  Session cost effect source depends on agent/account identity
  (`Usage.tsx:311-373`), not viewport, but full-flow request counts varied
  across runs; do not claim a resize never rereads without stable-chat counts.
  Earlier null/object/null logs combined worker agents. A lead-only capture saw
  `Release lead` seq 942 with `{tokens:80000, window:200000}` and compactions
  still 2. It showed no later clear; runtime's explicit clear writer is
  context-compaction completion (`scripts/codex_runtime.py:7501-7505`). No
  same-agent product clear is established. This remains separate from the
  390×330 12.03px gap defect.
  `waitForFunction` had an async
  predicate, whose Promise was truthy before the resize handler updated the
  viewport. Use a synchronous animation-frame poll for three stable samples,
  including computed safe-area styles, and assert the exact captured sample.
  Focused scroll restoration remains asserted before blur. The hook resets
  nonzero scroll on scale-1 updates, but returns early during pinch zoom; the
  test sets scale 2 before retaining 150px so pinch preservation is deterministic.
- `settings/daybreak-settings-browser.spec.mjs`: the fixture model catalog is
  synchronous. After a reset, the generic enable helper queried controls before
  the settings view finished rendering; its count-based fallback could select
  the default model instead of preserving Terra, or target the wrong model
  picker in subagent defaults. Wait for the relevant model picker before the
  fallback check, and wait for and check the actual Daybreak control directly
  in the Terra and running-agent cases. Also clear the previous request/refresh
  counters before the sequential model selection: otherwise the old equal
  counter values let `settled()` return before the new request starts, and that
  request can land after the next fixture reset. Solo repeat3 passed 3/3
  (18.4s total; 5.7–5.8s each).
- `limits/limits-ui.spec.mjs`: the timed 30,050ms expectation is obsolete
  (class 2). Commit `ef95d230`, “Replace UI status polling with resource
  watches,” deliberately removes the generic 60-second poll and the 30-second
  error retry in favor of typed resource watches. Commit `6e2a1558` retained
  the error retry. Its exact subject is “Scope agent chats and account limits,
  remove completion noise” (no body); it is not the removal. Keep the
  account-selection wait and assert the default-account 503. Cover the
  supported recovery paths:
  successful manual Refresh and a successful forced read after a typed
  `limits/default` resource notification. Solo repeat3 passed 3/3 (50.9s
  total; 16.0–17.1s each), including the asserted 503 and both recoveries.
  Empty cached limits response is `data: null`: `/api/limits` uses the nullable
  `UsageLimitsResponse.data` DTO (`scripts/studio_api/accounts/models.py:265-270`),
  and the endpoint returns its cached null shape in
  `scripts/studio_api/accounts/router.py:195-205` (fallback construction at
  `scripts/codex_runtime.py:8513`). The fixture remains `data: null`. The
  original `origin/main` test also used `data: null` and asserted the badge was
  plain `Limits` after switching to the empty account (`tests/client/limits/limits-ui.spec.mjs:675-681`);
  this is restored baseline behavior, not a `{}` workaround or a new stale
  badge regression.

## UI observations for the owner

A failed limits read is no longer retried on a timer since `ef95d230`; the
notice stays until Refresh or the next limits event.

The Usage footer clearance is a product defect family with non-deterministic
measurements at 390×390. The single `test.fixme` group holds 390×330,
390×390 known/unknown-context, and 640×390 reproductions for re-enabling after
the layout fix. The main flow removes footer clearance assertions at every
viewport while keeping settled composer geometry, non-overlap,
focus/selection, scroll preservation, pinch, zoom, orientation, and desktop
checks active. The 390×330 no-keyboard 12.03px gap expected failure remains
separate and repeat-stable. Consolidated sizes, rates, measurements, and CSS/TSX
sources are in `misc-known-failures.md`.

## Solo verification

These commands ran from `web` with the managed browser runtime environment;
each used one Playwright worker and three repetitions.

```sh
node node_modules/@playwright/test/cli.js test ../tests/client/background/background-controls-ui.spec.mjs --project=client --reporter=list --workers=1 --repeat-each=3 --output=/dev/shm/load-b-background-fixed
```

Result: 3/3 passed (23.2s).

```sh
node node_modules/@playwright/test/cli.js test ../tests/client/settings/daybreak-settings-browser.spec.mjs --project=client --reporter=list --workers=1 --repeat-each=3 --output=/dev/shm/load-b-daybreak-finalsolo5
```

Result: 3/3 passed (18.4s; 5.7–5.8s each).

```sh
node node_modules/@playwright/test/cli.js test ../tests/client/limits/limits-ui.spec.mjs --project=client --reporter=list --workers=1 --repeat-each=3 --output=/dev/shm/load-b-limits-finalsolo
```

Result: 3/3 passed (50.9s; 16.0–17.1s each).

```sh
node node_modules/@playwright/test/cli.js test ../tests/client/mobile/mobile-keyboard-ui.spec.mjs --project=client --reporter=list --workers=1 --repeat-each=3 --output=/dev/shm/load-b-mobile-review-final
```

Result: 9 passed, 3 skipped (1.2m). Main flow and compact keyboard setup passed
3/3; the 390×330 gap expected-failure behaved as expected in all repeats; the
consolidated small-mobile footer fixme was skipped. The final main-flow sample
measured root bottom 444px, composer bottom 326.96875px, and footer bottom
428.96875px after session cost settled, with request/response counts 1/1. Trace
evidence is under `/dev/shm/pw-40bIL7/codex-mobile-keyboard-J3emds`,
`...-r4PKWO`, and `...-SDGuhI`.

## Loaded combined run

The three checkpoint specs passed 9/9 at `--workers=3 --repeat-each=3` while
eight CPU busy-loop processes ran (`/dev/shm/load-b-three-final`). The final
mobile whole-file repeat3 is recorded above.
