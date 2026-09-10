# Sentence stream

Assistant prose appears at sentence boundaries. New text fades from 35% to full
opacity over 180 ms. The animation does not change position or dimensions.
Existing sentence nodes remain mounted as the paragraph grows. Loaded history
uses the existing static renderer and has no entrance animation. Reduced motion
disables the fade.

Markdown inline constructs stay intact. Fenced code waits for its closing fence.
Lists and headings use complete lines. A terminal event exposes any unfinished
text. A period at a chunk boundary waits for the next chunk so a decimal or URL
does not briefly appear as a finished sentence.

## Verification

- Production builds passed in the delivery worktree and active source checkout.
- `node --experimental-strip-types tests/sentence-stream-contract.mjs` passed.
  Character-by-character input covered decimals, links, code and emphasis without
  retracting already visible text.
- `node tests/sentence-stream-ui.mjs` passed in both checkouts. It uses an isolated
  runtime and browser, with native protocol events and no model requests.
  It covers retained DOM nodes, final tails, interruption, history, reduced motion,
  Markdown and a 390-pixel viewport. Desktop and mobile captures were inspected.
- `node tests/selection-quote-ui.mjs` passed in the active checkout.
- `node tests/conversation-motion-ui.mjs` passed at 1440 and 390 pixels.
- The active HTTP server serves the new sentence renderer. No server or agent
  restart was needed.

An exploratory run of the older full live-chat suite stopped at its tool-group
selector and, after a local selector adjustment, its JSON-wrap assertion. That
suite is not recorded as a pass. Its source was restored; the focused sentence
suite supplies the verification for this change.

The reported persistent old prompt remains unverified. The text was found in an
already delivered initial user message. A fresh client showed subsequent answers
and a completed session. The location of the reported extra display is still
needed to reproduce it. No message was removed or sent again.

The delivery worktree also corrects a nullable account email passed to the
optional transfer-label property, which otherwise prevented its TypeScript build.
