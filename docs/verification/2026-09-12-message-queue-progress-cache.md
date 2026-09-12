# Message queue and progress cache

Queue source: `7d4b334`. The following change adds the progress cache.

## Behavior

- Tab queues a nonempty composer draft. Modifiers, IME composition, empty input,
  and `/model` keep their existing behavior.
- One queue card supports edit, delete, drag reorder, and up/down controls.
- Queue revisions reject stale mutations. Durable receipts preserve exact retries.
- Queue edit journals preserve concurrent drafts and remove obsolete saved versions.
- PROGRESS.md restores a scoped local copy before its next HTTP response.
- The existing background scheduler prepares main chats before team workers.
- Cached progress retains its label and current viewport fit check. Empty live
  responses clear it. A failed read preserves the labelled copy.

## Checks

| Check | Result |
| --- | --- |
| `tests/queue-order-contract.py` | 12 passed |
| `tests/queue-update-contract.py` | 6 passed |
| `tests/message-queue-ui.mjs` | Chromium and WebKit passed |
| `tests/message-queue-drafts-ui.mjs` | Chromium and WebKit passed |
| `tests/chat-controls-ui.mjs` | Passed with durable attachment retry checks |
| `tests/progress-cache-browser.mjs` | Six cache boundaries passed in both engines |
| `tests/progress-cache-ui.mjs` | Both engines passed the original 40-worker fixture |
| `tests/progress-fit-browser.mjs` | Chromium passed |
| `tests/chat-prefetch-ui.mjs` | Chromium passed |
| `tests/model-command-ui.mjs` | Chromium passed |
| Production build and desktop package | Passed |

The progress integration test holds panel HTTP responses during chat switches.
All ten measured switches displayed cached content in the first animation frame.
Chromium measured 6.1 to 16.1 ms. WebKit measured 11 to 18 ms.
The checks include background file changes, reload, offline switches, scope
isolation, and empty and oversized revisions.
Queue controls remain reachable at a synthetic 390 by 480 pixel viewport.
The queue body scrolls separately from its heading.

The existing clock suite had one unrelated legacy exact-input failure. The
unchanged baseline queue method reproduced the same appended progress guidance.
The other five clock tests passed. This change does not claim that suite passes.

## Installed artifact

The installed app, local HTTP origin, and Tailscale HTTPS origin serve index SHA256
`79c367d09db6617b2b675f4ff4524fcc0348a65b6e4758f995e96ff5a03f4acd`.
The backend remains PID 76342. Its four native connections retained their process
and connection identities during the guarded queue update. The desktop is PID 9826.
Read-only smoke checks passed in Chromium locally and WebKit through HTTPS.
They sent no chat messages. The native window remained available.

Local evidence: `/Users/igor/.local/state/codex-agents/evidence/message-queue-20260912T232516`.
It contains source backups, update receipts, browser screenshots, test logs,
first-frame measurements, and installed artifact checks.

## Limits

A chat without a cached or prefetched copy still needs its first server response.
Caches are bounded and disposable. Source files and drafts remain authoritative.
Physical iPhone keyboard behavior and offline application-shell reload were not
measured. Fault tests block service workers to control HTTP responses.
