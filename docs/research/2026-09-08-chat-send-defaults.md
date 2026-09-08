# Chat delivery layout and default models

## Source comparison

The installed ChatGPT Work assets use stable client message identities and explicit
scroll compensation. Source inspection covered `thread-scroll-layout-Bu5PFUH3.js`
and `turn.readable.js` from version `26.818.41509`, extracted under
`/tmp/studio-work-ux-audit`. This was source inspection, not a live ChatGPT UI test.

Studio now retains the rendered user message when its server ID changes. It clears
the submitted draft with the optimistic message and preserves any subsequent draft.
Routine activity uses a fixed composer slot. Rapid phase changes settle for 250 ms.
Errors remain immediate. Scroll compensation retains the distance from the bottom.
Prompt navigation uses the arrow keys when the composer is empty.

## Model defaults

Each new team starts with Astra Medium and Luna Max. It does not copy execution
settings from the previous chat. Existing chats and explicit worker overrides remain
unchanged. Reusing an empty chat retains that chat's explicit settings.

The live server still had the older `create` and `worker_defaults` methods. Their
bytecode matched the source before commit `4bcf0b0`. A guarded update replaced only
`create`, `new_lead`, and `worker_defaults` in PID 35212. Account connections and
executor pools retained their identities. A validation-only call inside that process
returned Astra Medium, Luna Max, and Fast mode off. It created no chat or native turn.
The activation receipt is `/tmp/studio-defaults-activation.json`.

## Verification

- 49 runtime contract tests and 13 worker-default contract tests passed.
- Delivery passed through the HTTP fallback and RxDB paths. The same DOM node survived a changed server ID.
- Composer and header geometry passed at 1440, 900, and 390 px.
- Conversation motion passed at 1440 and 390 px, including an 18 px bottom offset.
- Prompt navigation passed, including protection of a nonempty draft.
- The production build passed. The HTTP server returned the exact built index.

These checks used isolated fixtures. They did not send model requests or stop live work.
