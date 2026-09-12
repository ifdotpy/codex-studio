# UI responsiveness, 2026-09-12

The change separates composer updates from transcript rendering. Each draft edit
still writes its local recovery journal before the asynchronous sync.

## Changes

- Reuse unchanged message objects after a full server projection.
- Preserve transcript groups and event callbacks during composer edits.
- Group draft versions by chat once. Skip writes and renders for unchanged values.
- Keep the visible scroll anchor instead of measuring every preceding paragraph.
- Reuse completed Markdown output across chat visits. Streaming prefixes stay local.
- Create message dialogs on first use. Preserve keyboard focus after close.

The completed Markdown cache holds at most 512 entries. Its accounting limit is
4 MiB, including source text, output text, and bookkeeping. This limit does not
measure the JavaScript heap. The cache stores no agent identities or React nodes.

## Reproduce

Run `npm --prefix web run test:responsiveness`.
Set `BROWSER=webkit` for the component and storage checks.
The final performance fixture always uses Chromium.

The production fixture contains 301 messages and 7,290 DOM elements. It uses a
CPU slowdown of four, desktop and mobile viewports, and full transcript updates.
Input latency measures the input event to the second animation frame. The report
also records native Event Timing entries and long tasks. These are different metrics.

## Measurements before the completed Markdown cache

Baseline commit: `dabd21c85ab9c72f86f2cd5a65b42bd8930144aa`.
The intermediate build includes transcript isolation and draft changes.
It precedes the completed Markdown cache and lazy dialogs.

| Input p95, milliseconds | Baseline | Intermediate |
| --- | ---: | ---: |
| Desktop, idle | 189.4 | 47.4 |
| Mobile viewport, idle | 187.9 | 35.4 |
| Desktop, scrolled history | 165.7 | 43.2 |
| Mobile viewport, scrolled history | 137.1 | 36.0 |
| Desktop, concurrent stream | 250.0 | 111.9 |
| Mobile viewport, concurrent stream | 263.4 | 112.5 |

The intermediate cached-chat switch did not improve. Desktop changed from
886.9 to 1,060.0 milliseconds. Mobile changed from 812.3 to 960.2 milliseconds.
These measurements motivated the completed Markdown cache and lazy dialogs.

Baseline index SHA-256:
`76d5910a4a96143d19d21a9823a6b59f2e40a8c73c89fcd29acd654f880a1a11`.
Intermediate index SHA-256:
`c228d0c3e52ac5bd91ec66f3dee2be51df830a203ac6ddc43e6319757d5a437e`.

Private evidence resides in the Studio state directory under
`evidence/ui-responsiveness-20260912`. It includes the reports and exact index hashes.

## Final build checks

Final index SHA-256:
`1bfedf591c47d13eebf2a0477fce4f244832540492ea706bfc8447c432d96f8d`.

The final fixture preserved all 301 messages, 12 stream revisions, and typed text
at both viewports. Scrolled rectangle reads fell from 8,457 to 371 desktop and
330 mobile. The final run started with host load 453 and ended at 289.
Its input p95 was 352.5 milliseconds desktop and 266.5 milliseconds mobile.
The corresponding cached switches took 1,676.4 and 1,435.2 milliseconds.
The changing host load prevents a useful final latency comparison.

The isolated Chromium and WebKit checks passed these cases:

- Return between two chats with 150 completed messages each: zero repeat Markdown parses or sanitizer calls.
- New text invalidates the cache. File links use the current agent.
- Streaming prefixes cannot enter the shared cache. Entry and byte limits evict old content.
- Both message dialogs return keyboard focus after the first and second close.
- Draft recovery preserves local edits and independent changes from another tab.
- Scroll restoration preserves the history position through reload and chat changes.

The production checks passed quotation, queue controls, mobile sends, shell navigation,
and native error display. The package check preserved its backend after desktop quit.
An initial Vite page-load timeout affected queue and mobile-send checks. Both retries passed.
The unrelated Python turn-history fixture failed before it created a turn; it is not a pass.

## Paired comparison after host recovery

Both fixtures ran sequentially after host load fell. The one-minute load changed
from 21.80 to 19.18 to 16.19. The old assets came from the saved application.
Their index hash matches the baseline above. The runner checkout is not the old
asset source identity. The final assets match the final index hash above.

| p95, milliseconds | Baseline | Final |
| --- | ---: | ---: |
| Desktop input | 213.5 | 45.8 |
| Mobile input | 255.5 | 33.5 |
| Mobile input, scrolled history | 205.6 | 45.3 |
| Desktop input during stream | 392.1 | 148.4 |
| Mobile input during stream | 406.7 | 101.3 |
| Mobile stream update | 422.8 | 118.2 |

Cached switches changed from 1,167.6 to 566.9 milliseconds desktop and from
1,157.1 to 620.8 milliseconds mobile. All message and draft checks passed.
Large histories still require substantial time to open. They are not instant.
The separate chat-prefetch check failed during host contention, then passed after
host recovery. It measured 66 milliseconds for a never-opened chat and 45 and
51 milliseconds for revisits. Its first-frame, scroll, and workspace guards passed.

The installed desktop uses the final index hash. Its current chat and team rendered
without a page error in the read-only live check. A WindowServer image confirms
the installed application renders the chat. Backend PID 76342 remained unchanged.

## Limits

These checks do not measure a physical iPhone, its keyboard, or suspended iOS execution.
They do not certify native application latency. Concurrent host work affects timing.
