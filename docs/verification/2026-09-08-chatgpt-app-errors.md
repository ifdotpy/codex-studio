# Installed ChatGPT app: error handling

## Evidence

- App: `/Applications/ChatGPT.app`, version `26.825.51511`, build `7377`.
- Bundle identifier: `com.openai.codex`.
- Package name: `openai-codex-electron`.
- Archive: `Contents/Resources/app.asar`.
- Archive SHA-256: `f56ac8d5254a10fc4a04e7417fa787d135c3bbca49bad7d668d4ae65833d40c7`.
- Main renderer bundle: `webview/assets/app-initial-B6Gk5KCN.js` inside that archive.
- Studio comparison: commit `437dd25`.

The inspection used an extracted copy under `/tmp/chatgpt-error-audit`.
It did not start the app, read account storage, send requests, or change app files.
The archive contains compiled JavaScript. It does not contain original source maps.
Symbols and byte offsets below identify this exact bundle, not future releases.

## Native Codex conversations

| Case | Installed app code | Studio difference |
| --- | --- | --- |
| `serverOverloaded` | The latest failed turn gets a Retry button. An optional countdown starts a new empty turn. | Studio shows a warning. It has no capacity countdown or dedicated Retry button. |
| Capacity delays | Consecutive empty capacity failures use 10, 30, 120, then 300 seconds. After those attempts, the countdown is absent. | Studio retains its explicit failure hold. |
| Retry conditions | The automatic timer requires flag `2899820207`, an available delay, an idle retry control, and conversation role `owner`. | The installed account's flag value was not inspected. |
| Retry identity | A per-conversation set prevents concurrent button actions. The helper starts a turn with `input: []`. It checks native activity first. | This is a new native turn, not a replay of the previous user message. |
| Native `willRetry` | Retry events become separate stream-error items. Adjacent numbered reconnect events replace one display item and preserve its ID. | Studio displays the current native retry status and details. |
| Usage limit | Guidance depends on account plan, workspace role, credit availability, and reset time. | Studio distinguishes owner and member cases from the native limits snapshot. Personal-plan guidance is less specific. |
| Error payload | The app extracts nested JSON from plain JSON, code fences, or surrounding text. It displays the nested error message. | Studio's parser accepts a complete JSON string. Wrapped JSON can remain raw. |
| Bio policy | The same JSON extractor feeds the bio-policy classifier. Dedicated policy content replaces the generic error row. | Studio can miss the policy notice when JSON has a surrounding prefix or code fence. |
| Misalignment policy | A dedicated precaution banner offers a new chat. The generic error row excludes this policy type. | Studio also blocks the thread and offers a new chat or another chat. |

Automatic retry is code-supported, not proven active for the installed account.
The timer belongs to the mounted retry component. Its effect removes the interval
when the component unmounts. The component has no separate Cancel control.
A rejected retry request clears its countdown and releases its local pending state.
The start-empty-turn helper does not repeat the previous user text.

## Source anchors

All offsets refer to UTF-8 bytes in the main renderer bundle.

| Symbol | Byte offset | Purpose |
| --- | ---: | --- |
| `W1o` | 8776329 | Retry button and countdown text |
| `Z1o` | 8779967 | Latest failed turn check |
| `Q1o` | 8780849 | Feature flag, owner role, timer, and click action |
| `a0o` | 8786196 | Consecutive capacity failure delay |
| `hWt` | 2141819 | Empty turn and native activity checks |
| `q1o` | 8777417 | Plan and workspace usage guidance |
| `Pzi` | 6128227 | Generic error text and policy exclusion |
| `FFi` | 6083080 | JSON extraction |
| `RFi` | 6083632 | Bio and cyber policy classification |
| `jVi` | 6182275 | Adjacent reconnect display deduplication |
| `u0o` | 8788595 | Precaution banner |

## Checks and limits

An isolated Node probe executed the actual extracted delay selector and JSON extractor.
It confirmed all four delays, exhaustion, reset after new input or another error,
three JSON wrappers, and the plain-text fallback. Dependencies of the delay selector
used fixture values. No app runtime or model service was involved.

This review does not establish full equivalence between Studio, native Codex,
and ChatGPT cloud conversations. The capacity action adds orchestration beyond CLI
error presentation. It requires its own request-identity and restart contract in Studio.

## ChatGPT cloud conversation streams

These paths share the app bundle but use a different protocol from native Codex.

- `MCn` (byte 2951011) resumes the same conversation through `/f/conversation/resume`.
  It sends the conversation ID, acknowledged event offset, and optional conduit token.
- `LSn` (byte 2940691) permits 12 consecutive failed attempts. An acknowledged
  non-response event resets that counter and advances the offset.
- The delay in milliseconds is `min(300 * 1.5^attempt, 5000) * random(0.5, 1)`.
  A conversation ID is required. Without a token, only offset zero can resume.
- `BSn` (byte 2942099) classifies network errors and HTTP 408, 409, 425, 429,
  502, and 504 as retryable. `MCn` also requires a network failure, an initially
  resumed stream, or failure of its current resume request. These HTTP statuses
  do not authorize automatic repetition of an initial completion request.
- `VSn` (byte 2942256) has a separate recoverable-error classification.
  It includes network failures and most HTTP 4xx responses. It excludes 403,
  `history_disabled_conversation_expired`, and `conversation_deleted`.
  A recoverable callback does not prove that another request occurs.
- `XCn` (byte 2960923) ignores duplicate WebSocket item IDs. Missing parents,
  invalid data, and unavailable catch-up history produce recoverable errors.
  An embedded error string produces a terminal error.
- WebSocket silence times out after five seconds before the first valid payload,
  or 30 seconds afterward. Heartbeats count as valid payloads.
- `createCompletionStreamHandlers` (byte 3017891) sends recoverable WebSocket
  failures to the resume handler. After handoff, it suppresses the original
  fetch stream's completion and error callbacks.
- `startCompletionStream` (byte 3015882) disables automatic resume when
  `history_and_training_disabled` is true. `resumeCompletionStream` explicitly
  enables resume. The sidebar stream uses a separate path.

Transport reconnect has separate counters: 20 retries in the foreground and five
in the background. These counters do not replace the completion timeout or the
12-attempt resume counter. An open socket resets the transport retry counter.

No explicit unknown-mutation-result state was found in these cloud completion
paths. Native Codex has a separate `outcome-unknown` path for `turn/start`.
The code inspection does not prove the installed account's route selection,
feature flags, or observed server behavior.
