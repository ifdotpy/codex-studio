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
then writes the recovery journal and saved scope map in the same synchronous
call. Both writes finish before the input handler returns. The replication
client coalesces upstream draft pushes with a trailing quiet wait and a bounded
maximum wait ([`client.ts`](../../sync/client.ts)); it does not delay local
recovery or change the storage format. Remote reconciliation and workspace
adoption still update the legacy hook state and notify changed chat subscribers.

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

`PromptInput.spec.mjs` directly mounts the production input without App or
Conversation. It checks controlled and programmatic value changes, disabled
and overlength states, Enter, Shift+Enter, Tab, IME composition, and skill
insertion. The 500-draft fixture has 20 edits and checks the real
`PromptComposer` with the production draft hook. Its surrounding Harness,
SidebarFixture, and TranscriptFixture are explicitly test stand-ins, not the
production App/sidebar/transcript. It deterministically requires zero renders
in those three functions for the isolated revision (and 20 for the historical
baseline), 20 composer commits, one synchronous map write and one journal write
per edit, and no writes for unchanged reconciliation. This fixture checks the
store/composer boundary. The separate 301-message UI
fixture runs the production App, Sidebar, and Conversation at four-times CPU
slowdown on desktop and mobile, checks chat switching and streamed updates, and
records function-level render probes and timings. It asserts App and Sidebar do
not render while typing; it allows at most one incidental Conversation update
to coincide with typing, while the focused 500-draft fixture remains strictly
zero for parent/transcript renders. Neither browser fixture has a latency
threshold.

## Recorded measurements

The paired script uses headless Chromium and a production build. The original
baseline was measured before extraction. Timing numbers are local observations,
not service-level guarantees. Draft persistence counts and render counts are the
more repeatable contract.

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

The paired setter timings varied and do not establish a speedup. A later focused
run on the final draft code measured a 22.4 ms median and 29.7 ms maximum. The
paired rounds were captured just before the independent scroll-follow control
was moved out of Conversation; that later change does not modify draft writing.

The separate production fixture uses 301 transcript records and CPU slowdown 4. One baseline/current observation was:

| Viewport and phase       | Baseline App / Sidebar / Conversation renders | Current renders | Input p50/p95, baseline → current |
| ------------------------ | --------------------------------------------: | --------------: | --------------------------------: |
| Desktop, idle typing     |                                  27 / 27 / 17 |       0 / 0 / 0 |     91.9 / 301.6 → 39.5 / 45.6 ms |
| Desktop, scrolled typing |                                  31 / 31 / 27 |       0 / 0 / 0 |     90.6 / 250.6 → 44.5 / 57.9 ms |
| Mobile, idle typing      |                                  27 / 27 / 27 |       0 / 0 / 0 |     63.1 / 168.4 → 37.9 / 45.8 ms |
| Mobile, scrolled typing  |                                  31 / 31 / 31 |       0 / 0 / 0 |     68.7 / 264.9 → 36.7 / 45.9 ms |

These are local single-run timing observations, not a controlled statistical
claim. The render counts are the stable result. The production probe uses a 20 ms input window. One run captured a single
Conversation call in a typing phase, while the later run captured zero; its
root cause is not isolated. The production fixture allows at most one such call
per phase and strictly forbids App or Sidebar renders.
The isolated 500-draft fixture strictly requires zero parent/sidebar/transcript
renders. Neither fixture has a latency pass/fail limit. They use local test
data and do not measure live model requests.
