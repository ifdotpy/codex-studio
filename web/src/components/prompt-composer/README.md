# Prompt composer

## What this owns

`PromptComposer.tsx` owns the form subtree and the live text for one chat.
`PromptInput.tsx` owns the textarea, autocomplete menu, keyboard handling, and
caret-sensitive skill insertion. The composer subscribes to one chat ID; the
sidebar and conversation do not subscribe to that chat's keystrokes.

Think of the screen as a room with three people: the sidebar, the conversation,
and the person typing. Before this change, every letter called everyone into
the room to redraw. Now only the typing person redraws for a letter. They still
put the letter in the same saved draft record right away.

## Draft and send contract

`useSyncedDrafts` in [`../../sync/drafts.ts`](../../sync/drafts.ts) remains the
only owner of draft text, the recovery journal, sync, and conflict handling. On
each edit, it updates its current value and notifies the chat's subscriber,
then writes the recovery journal and that chat's workspace-scoped local record
in the same synchronous call. Both writes finish before the input handler
returns. Unchanged chats are not serialized on each keystroke. Remote
reconciliation and workspace adoption update local records and notify changed
chat subscribers.

The first read copies entries from the old `codex-drafts:<workspace>` map into
individual chat keys. Each per-chat key is an idempotent commit point; a failed
partial migration leaves the old map untouched, and reload retries missing
entries. Tombstones prevent cleared chats from returning from the old map.
An older open tab can still update its legacy map; the current client journals
changed values as an independent writer version and adopts chats first created
in that old tab into the running hook. A bounded rescan retries a failed legacy
journal write without requiring another storage event or reload. The legacy map
remains a compatibility source and ordinary current-client edits do not rewrite
it. Local records retain workspace identity, and cached identity still cannot
authorize sync to a different server.

App and Conversation read the current value by chat ID when a send begins. This
keeps an immediate Enter send and a send that waits on uploads or delivery from
using an old render's text. The existing request ID is retained through retry;
the draft clears only when the durable outbox owns the message and only if the
user has not typed newer text. Changing chats changes the subscription, so each
chat keeps its own draft.

The textarea remains controlled. Attachments, dictation, prompt recall, skill
completion, selection and caret placement, Enter, Tab, Shift+Enter, and IME
composition keep their existing event paths.

## Owned checks

From `web/`, run:

```sh
npm run test:prompt-composer
npm run benchmark:prompt-composer
npm run benchmark:prompt-composer:paired
```

The first command builds and runs the geometry, autocomplete/recall/IME/caret,
and 500-draft persistence/render-isolation browser checks. The benchmark command
runs the draft fixture and production UI responsiveness fixture. The paired
command makes a temporary worktree at baseline revision
`e681bcc618d9ea4f4a0799de3fa81d01eee92687`, alternates two baseline/current
500-draft measurements, then runs the real production UI fixture on each
revision. The baseline is a sparse worktree containing only the web app and
fixture support files, which avoids copying unrelated repository content. It
removes its temporary worktree and linked dependency directory when complete.
The command needs enough free disk quota for that baseline checkout and browser
build. Set `CHROME_BIN` if Chromium is not at the default path.

`PromptInput.test.mjs` directly mounts the production input without App or
Conversation. It checks controlled and programmatic value changes, disabled
and overlength states, Enter, Shift+Enter, Tab, IME composition, and skill
insertion. The 500-draft fixture has 20 edits and checks the real
`PromptComposer` with the production draft hook. Its surrounding Harness,
SidebarFixture, and TranscriptFixture are explicitly test stand-ins, not the
production App/sidebar/transcript. It deterministically requires zero renders
in those three functions for the isolated revision (and 20 for the historical
baseline), 20 composer commits, one synchronous per-chat record write and one
journal write per edit in the current revision, and no writes for unchanged
reconciliation. The paired baseline still writes one aggregate map per edit.
This fixture checks the
store/composer boundary. The separate 301-message UI
fixture runs the production App, Sidebar, and Conversation at four-times CPU
slowdown on desktop and mobile, checks chat switching and streamed updates, and
records function-level render probes and timings. It asserts App and Sidebar do
not render while typing; it allows at most one incidental Conversation update
to coincide with typing, while the focused 500-draft fixture remains strictly
zero for parent/transcript renders. Neither browser fixture has a latency
threshold.

## Recorded measurements

The paired Chromium script builds the pinned baseline and current worktree.
Measurements below distinguish archived composer history from the per-chat
draft migration pair. Timing numbers are local observations, not guarantees.

### Historical composer browser measurements

This archived measurement used `e681bcc`, which changed the hook's render
subscription behavior relative to the task's pinned `2470fb5` baseline. It is
not evidence for the per-chat draft migration and is retained only as earlier
composer history.

| 500 drafts, 20 edits                          |     Baseline |    Current |
| --------------------------------------------- | -----------: | ---------: |
| Parent / sidebar / transcript-fixture renders | 20 / 20 / 20 |  0 / 0 / 0 |
| Composer commits                              |           20 |         20 |
| Synchronous scope-map writes                  |           20 |         20 |
| Aggregate scope-map bytes written             |   40,195,940 | 40,195,940 |
| Synchronous journal writes                    |           20 |         20 |
| Setter median, paired round 1                 |      24.2 ms |    21.3 ms |
| Setter median, paired round 2                 |      22.9 ms |    27.3 ms |
| Setter max, paired round 1                    |      28.1 ms |    31.2 ms |
| Setter max, paired round 2                    |      30.9 ms |    42.2 ms |

### Paired storage-only comparison for per-chat drafts

This Node fixture uses an in-memory `localStorage` adapter, 500 drafts of 2,000
characters, and 20 edits. Each edit includes the pending journal bytes. The
final edit batch used 10,580 bytes versus 40,197,820 baseline bytes. Copying
the fixture once used 2,212,780 additional bytes while retaining the legacy
map. Median write-call time was 0.003687 ms baseline and 0.010508 ms current in
this in-memory fixture. These sub-millisecond figures do not measure browser
storage, React, or perceived input latency. The focused browser pair exercises
the real composer hook; the separate production UI fixture measures App-level
input behavior.

The paired setter timings varied and do not establish a speedup. Both current
rounds use a session-bound SHA-256 legacy checkpoint to detect old-tab edits
without duplicating draft text.

### Pinned `2470fb5` draft-hook browser pair

This pair uses the task's exact pre-change revision. It runs in headless
Chromium with 500 drafts and 20 edits. Both revisions retain the same
per-chat composer subscription boundary; the pinned baseline already keeps the
parent, sidebar, and transcript fixture at zero renders during typing.

| Measurement                                   |   Baseline |   Current |
| --------------------------------------------- | ---------: | --------: |
| Parent / sidebar / transcript-fixture renders |  0 / 0 / 0 | 0 / 0 / 0 |
| Composer commits                              |         20 |        20 |
| Aggregate map writes                          |         20 |         0 |
| Aggregate map bytes                           | 40,195,940 |         0 |
| Per-chat record writes                        |          0 |        20 |
| Per-chat record bytes                         |          0 |     9,660 |
| Pending journal writes                        |         20 |        20 |
| One-time per-chat migration bytes             |          0 | 2,208,842 |
| Setter median, round 1                        |    23.2 ms |    3.8 ms |
| Setter median, round 2                        |    22.4 ms |    4.9 ms |
| Setter maximum, round 1                       |    59.4 ms |    9.1 ms |
| Setter maximum, round 2                       |   280.4 ms |   46.4 ms |

Browser byte counts above separate synchronous chat-record writes from
pending-journal writes; the Node adapter pair below combines their bytes. These
two browser rounds are local observations, not a controlled latency guarantee.

The archived production fixture uses 301 transcript records and CPU slowdown 4. One baseline/current observation was:

| Viewport and phase       | Baseline App / Sidebar / Conversation renders | Current renders | Input p50/p95, baseline → current |
| ------------------------ | --------------------------------------------: | --------------: | --------------------------------: |
| Desktop, idle typing     |                                  27 / 27 / 17 |       0 / 0 / 0 |     91.9 / 301.6 → 39.5 / 45.6 ms |
| Desktop, scrolled typing |                                  31 / 31 / 27 |       0 / 0 / 0 |     90.6 / 250.6 → 44.5 / 57.9 ms |
| Mobile, idle typing      |                                  27 / 27 / 27 |       0 / 0 / 0 |     63.1 / 168.4 → 37.9 / 45.8 ms |
| Mobile, scrolled typing  |                                  31 / 31 / 31 |       0 / 0 / 0 |     68.7 / 264.9 → 36.7 / 45.9 ms |

These `e681bcc` measurements are local historical observations and do not isolate the draft migration. On an earlier exact `2470fb5` pair the baseline production fixture passed; the candidate fixture recorded two App renders in the mobile/scrolled-input phase (limit 0). The standalone report previously described as a candidate pass is labeled with source `2470fb5`, so it is baseline evidence and does not establish a candidate pass. The final-source focused storage pair passed both rounds, then the baseline production fixture recorded three App renders in the desktop/scrolled-input phase (limit 0), so the candidate production fixture was not reached. The candidate production UI result remains unresolved. The production probe uses a 20 ms input window. One archived run captured a single
Conversation call in a typing phase, while the later run captured zero; its
root cause is not isolated. The production fixture allows at most one such call
per phase and strictly forbids App or Sidebar renders.
An independent replay of the saved candidate artifact
(`/dev/shm/studio-ui-responsiveness-PitPa7`, `dist/index.html` SHA-256
`a1dc56d5cd16c7b040c6842c69615ab26a252db0616e757f79b2947c713639e9`)
reproduced one App render in mobile/scrolled typing against a zero limit. No
full candidate production-responsiveness run passed; the focused draft-hook
pair's zero parent renders and 20 composer commits do not establish that claim.
The isolated 500-draft fixture strictly requires zero parent/sidebar/transcript
renders in both baseline and current. Neither fixture has a latency pass/fail limit.
They use local test data and do not measure live model requests.

The message-delivery fixture counts mutation requests to ensure removing a
receipt does not send, cancel, or edit a command. Draft recovery can legitimately
push `POST /api/sync/drafts` during the same reload window. The fixture still
records that request and permits only that exact endpoint after validating
each row's RxDB identity, expected device, chat, matching master/new document
ID and session, and before/after text states;
message, queue, cancel, edit, and unknown mutations remain failures. The
previously observed extra POST contained four expected draft rows and no
delivery or command identity.
