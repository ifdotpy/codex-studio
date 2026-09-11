# Background chat updates, 2026-09-11

Baseline: `a844a5eabea9c4839bc56a1bed47a67a53b0ef1b`.

Studio prepares unarchived lead chats and agents in the selected team while the page is visible.
The selected chat loads first. Background work uses two transfers at most and the shared sync stream.
Changed chat metadata schedules a refresh. Busy histories also refresh periodically.
Hidden or offline pages do not start background transfers.

Background and foreground reads use the same persisted RxDB projections and checkpoints.
A synchronous cache retains 32 histories within a 24 MiB payload estimate.
Other persisted histories remain in IndexedDB.
A chat switch uses the prepared history before a network request completes.
Older and focused pages retain their rows, cursors, and scroll position in the first frame.
Server sequences reject late replies, including replies from another browser tab.
Tombstones reach active views and survive reload.
The sequence guard covers RxDB 17.5's downstream equality check.
The browser regression fails if that guard permits a temporary stale write.

## Checks

- TypeScript and production builds passed.
- Transport regression passed in Chromium and WebKit.
  It covers transfer limits, hidden/offline state, stale replies across tabs,
  deletion, persistence, workspace identity, and resource release after failures.
- Full App regression passed in Chromium and WebKit.
  It holds the initial foreground request and rejects premature background reads.
  It updates an unopened chat, blocks the network, and checks each rendered frame on selection.
  Earlier-history and focused-search returns retain their exact scroll position.
- WebKit also passed with the old server identity, without `chatState`.
- Existing chat-switch, message-delivery, large-history, mobile-send, and cached-identity checks passed.
  The large-history test measured 1,062 ms from response to content in its final isolated run.
  An earlier concurrent run measured 1,832 ms and exceeded its 1,800 ms threshold.
- The chat-switch fixture now expects shared transport and a valid message receipt.
  Exact send counts, message IDs, scroll, and late-response checks remain.

[Measurements](2026-09-11-chat-prefetch.json) record fixture and live results.
Prepared chat selection took 102 ms in Chromium and 80 ms in WebKit.
The fixture isolates the service worker because its request routes model server responses.
The service worker has separate lifecycle coverage from the mobile reliability change.
No physical iPhone measurement is claimed.

## Installed UI

The installed static interface matches the verified renderer index:
`79d92f4a6c212c063f96579da0cd985d9ee153d27ac62d6398507a668932f000` (SHA-256).
Assets were copied before the index. Existing hashed assets remain for open pages.
Backup: `app-backups/chat-prefetch-ui-20260911-172124` in the Studio state directory.
The backend PID remained `48902`. Its source files did not change.

A headless mobile viewport opened the installed UI through the configured Tailscale HTTPS address.
It loaded another real chat in the background, then blocked further network requests.
Selection displayed that chat's saved history in 456 ms.
The check permitted GET requests only and submitted no messages or other writes.
Open clients receive this UI on reload.

The full package also passed its isolated hidden startup check.
Package tree SHA-256: `a4e022d1d6fffa4c5b8a1b02061ea20a30a4b1de4befba70491aee86065f33cc`.
Package evidence: `codex-desktop-package-test-yF4zeo` in the system temporary directory.
The earlier compact-state and offline-shell server changes still require a backend update after active work finishes.
