# Prompt composer

`PromptComposer.tsx` owns the form subtree and subscribes to the active chat's
draft. `PromptInput.tsx` owns the controlled textarea, autocomplete, keyboard
handling, and caret-sensitive skill insertion. The draft hook in
[`../../sync/drafts.ts`](../../sync/drafts.ts) owns draft persistence and
conflict handling. Send reads the current draft by chat ID so an immediate
Enter or a send delayed by an upload uses current text.

Textarea height is measured at most once per animation frame, including after
programmatic draft changes and width changes. CSS owns its minimum and maximum
height; long drafts scroll inside the textarea. Live text is subscribed to by
the transcript subtree so token deltas do not render the composer. Attachments,
dictation, recall, skill completion, caret placement, Enter, Tab, Shift+Enter,
and IME composition retain their input paths.

From `workspaces/client/apps/web/`, run:

```sh
pnpm run test:prompt-composer
pnpm run benchmark:prompt-composer
```

The input browser check exercises geometry, coalesced layout reads, input
handling, and autocomplete. The benchmark uses an isolated server and scripted
provider to create at least 300 completed turns containing Markdown and code.
It opens the actual SSE transcript, checks switching between two real chats,
and records input-to-second-animation-frame latency and render probes while
idle and while tokens stream at 4× CPU slowdown. It also checks that a token
without typing does not render the composer. It does not use a live model.

One local Chromium observation (301 completed turns) gave these timings:

| Phase                 | Before p50/p95  | After p50/p95   |
| --------------------- | --------------- | --------------- |
| Idle input            | 18.5 / 28.1 ms  | 21.7 / 29.9 ms  |
| Input while streaming | 29.9 / 158.9 ms | 28.6 / 140.3 ms |

These individual runs do not establish a latency improvement. In the same
observations, Conversation renders per streaming key fell from 0.68 to 0.36;
the token-only composer assertion changed from one render to zero. Phase-wide
render counts include asynchronous background work, so they do not by
themselves establish what caused each render.
