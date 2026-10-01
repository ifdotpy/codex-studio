# Mobile performance measurements

## Setup

Playwright ran headless Chromium and WebKit at 390 by 844 pixels. Chromium used four-times CPU slowdown and Fast 4G throttling. WebKit throttled asset responses; local fixture API requests and CPU were unthrottled. Both used 1,500 agents, 2,700 entity rows, about 1,520 transcript rows, 700 work records, and about 6 MB of work results. Each run used a temporary state directory.

The matched Chromium comparison used clean main `f2d1545` build `6c95ccb6381dfd6c` and branch `d115ec0` build `694025e7a2d56032`. One full run per build ran serially. The branch met the cold-list and cached-draft gates. The harness forced garbage collection before reading JavaScript heap. Timings can vary with host load.

## Chromium before and after

| Measure                                  |                         Clean main |                            Branch |                               Change |
| ---------------------------------------- | ---------------------------------: | --------------------------------: | -----------------------------------: |
| Cold composer ready                      |                           9,665 ms |                          5,059 ms |                           48% faster |
| Cold usable chat list                    |                          10,478 ms |                          5,372 ms |                           49% faster |
| Full entity sync                         |                          17,317 ms |                         12,092 ms |                           30% faster |
| Cached chat and exact draft              |                           1,404 ms |                          1,390 ms |                            1% faster |
| Open chat                                |                           1,078 ms |                            728 ms |                           33% faster |
| Switch chat                              |                             363 ms |                            394 ms |                         31 ms slower |
| Fixture send visible                     |                             341 ms |                            322 ms |                            6% faster |
| Return to long chat                      |                             913 ms |                          1,092 ms |                        179 ms slower |
| Long-chat scroll                         | 1,199 messages, 9 pages, 11,165 ms | 1,199 messages, 9 pages, 9,446 ms |                           15% faster |
| JavaScript heap after garbage collection |                       24,456,152 B |                      24,456,004 B |                          148 B lower |
| IndexedDB objects                        |                              4,243 |                             4,336 |                        +93 item rows |
| IndexedDB serialized estimate            |                        6,270,626 B |                       6,287,832 B |                            +17,206 B |
| Browser origin use                       |                        8,629,478 B |                       8,749,059 B |                           +119,581 B |
| Transcript projection documents          |                                 32 |                               124 |            Separate cached item rows |
| Initial JavaScript, gzip                 |                          456,486 B |                         430,693 B |                         5.7% smaller |
| Total JavaScript, gzip                   |                        1,568,280 B |                       1,577,936 B |                          0.6% larger |
| Initial sync transfer                    |           245,968 B in 50 requests |          223,127 B in 19 requests | 9.3% fewer bytes, 62% fewer requests |
| Main-thread long tasks                   |     28, 4,329 ms total, 326 ms max |    29, 4,351 ms total, 358 ms max |                  Similar in this run |

The full transcript remains available offline. The implementation stores each transcript item in its own projection row, so row count rises while serialized storage stays close to baseline. Heap after garbage collection is effectively unchanged. The fixture measured 1,199 of 1,500 expected transcript messages across nine pages. Chat switch, return-to-chat, and long-task results did not improve in this single run.

## WebKit branch run

The current branch completed the full fixture before the rebase: cold chat list 7,439 ms, cached draft 6,071 ms, full entity sync 13,981 ms, open 411 ms, switch 383 ms, visible send 274 ms, and long-chat scroll 2,637 ms across nine pages. IndexedDB held 4,337 objects, including 125 transcript rows; its serialized estimate was 6,290,482 B. WebKit did not expose the JavaScript heap or long-task observer measurements. Its API and CPU were not throttled, so these timings are not directly comparable to Chromium. The clean-main WebKit baseline failed on fixture access-control checks and did not produce matched interactions.

## Changes measured

- Lazy-loaded analytics, terminal, settings, and background task panels. Initial JavaScript gzip fell by 25,793 bytes.
- Limited mobile background transcript prefetch to two targets and stopped background-only subscriptions from opening invalidation streams. A foreground subscriber still opens its stream.
- Sent sparse transcript item deltas. The server hashes transcript pages before it opens the SQLite writer transaction. Per-agent revisions avoid invalidation from unrelated database commits.
- Stored transcript items separately in RxDB. Applied deltas to retained item objects and batched persistence.
- Prioritized the selected agent during initial entity sync and bounded mobile entity page work.

The transcript write-volume contract passed: five 400-item pulls fell from 2,313,020 to 468,676 bytes, a 79.7% reduction. The requested sync and SQLite transaction contracts passed.
