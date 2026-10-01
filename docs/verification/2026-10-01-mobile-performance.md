# Mobile performance measurements

## Setup

Playwright ran headless Chromium and WebKit at 390 by 844 pixels. Chromium used four-times CPU slowdown and Fast 4G throttling. WebKit throttled asset responses; local fixture API requests and CPU were unthrottled. Both used the same fixture shape: 1,500 agents, 2,700 entity rows, 1,520 transcript rows, 700 work records, and about 6 MB of work results. Each run used a temporary state directory.

The Chromium comparison used clean main build `5ff47423c8dc8b1e` and branch build `e00773d60ed27cb6`. It used one successful full run per build, serially. The branch met the cold-list and cached-draft gates. The harness forced garbage collection before reading JavaScript heap. Timings can vary with host load.

## Chromium before and after

| Measure | Clean main | Branch | Change |
| --- | ---: | ---: | ---: |
| Cold composer ready | 23,283 ms | 5,159 ms | 78% faster |
| Cold usable chat list | 27,301 ms | 6,048 ms | 78% faster |
| Full entity sync | 35,225 ms | 12,628 ms | 64% faster |
| Cached chat and exact draft | 3,501 ms | 1,115 ms | 68% faster |
| Open chat | 1,277 ms | 631 ms | 51% faster |
| Switch chat | 843 ms | 451 ms | 46% faster |
| Fixture send visible | 655 ms | 422 ms | 36% faster |
| Return to long chat | 3,572 ms | 1,477 ms | 59% faster |
| Long-chat scroll | 1,199 messages, 9 pages, 15,445 ms | 1,199 messages, 9 pages, 11,890 ms | 23% faster |
| JavaScript heap after garbage collection | 24,725,204 B | 24,718,484 B | 0.03% lower |
| IndexedDB objects | 4,245 | 4,337 | +92 item rows |
| IndexedDB serialized estimate | 6,276,014 B | 6,290,584 B | +14,570 B |
| Browser origin use | 8,671,451 B | 8,741,878 B | +70,427 B |
| Transcript projection documents | 34 | 125 | Separate cached item rows |
| Initial JavaScript, gzip | 455,733 B | 429,958 B | 5.7% smaller |
| Total JavaScript, gzip | 1,567,487 B | 1,577,131 B | 0.6% larger |
| Initial sync transfer | 242,658 B in 47 requests | 223,764 B in 21 requests | 7.8% fewer bytes, 55% fewer requests |
| Main-thread long tasks | 74, 16,294 ms total, 1,413 ms max | 37, 5,993 ms total, 522 ms max | 50% fewer, 63% less total time |

The full transcript remains available offline. The implementation stores each transcript item in its own projection row, so row count rises while serialized storage stays close to baseline. Heap after garbage collection is effectively unchanged. The fixture measured 1,199 of 1,500 expected transcript messages across nine pages.

## WebKit branch run

The current branch completed the full fixture: cold chat list 7,439 ms, cached draft 6,071 ms, full entity sync 13,981 ms, open 411 ms, switch 383 ms, visible send 274 ms, and long-chat scroll 2,637 ms across nine pages. IndexedDB held 4,337 objects, including 125 transcript rows; its serialized estimate was 6,290,482 B. WebKit did not expose the JavaScript heap or long-task observer measurements. Its API and CPU were not throttled, so these timings are not directly comparable to Chromium.

The clean-main WebKit baseline reached the chat list in 24.8 to 25.7 seconds, then failed on fixture access-control checks. It did not produce a matched baseline for later interactions, heap, IndexedDB, or long-task measures.

## Changes measured

- Lazy-loaded analytics, terminal, settings, and background task panels. The initial JavaScript gzip size fell by 25,775 bytes.
- Limited mobile background transcript prefetch to two targets and stopped background-only subscriptions from opening invalidation streams. A foreground subscriber still opens its stream.
- Sent sparse transcript item deltas. The server hashes transcript pages before it opens the SQLite writer transaction. Per-agent revisions avoid invalidation from unrelated database commits.
- Stored transcript items separately in RxDB. Applied deltas to retained item objects and batched persistence.
- Prioritized the selected agent during initial entity sync and bounded mobile entity page work.

The transcript write-volume contract passed: five 400-item pulls fell from 2,313,020 to 468,676 bytes, a 79.7% reduction. Four contract tests passed.
