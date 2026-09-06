# Time awareness

The application uses the native Codex 0.153.4 clock reminder. It enables
`features.current_time_reminder` with `reminder_interval_seconds = 0`,
`delivery_mode = "after_user_or_tool_output"`, and `clock_source = "system"`.
These settings apply to thread start, resume, and fork through `THREAD_CONFIG`.
No global Codex settings or binary change.

The native runtime appends the current UTC time before inference after a user
message or tool output. Successful and failed function calls, freeform tool
outputs, and tool search outputs set the same boundary. Parallel output batches
receive one reminder before the next inference. This gives the model the time
when it can act on those results. It does not add a separate clock to every
nested JavaScript tool result inside a single code-mode call.

The application also records the acceptance time for each queued or steered
message. Reorder operations do not change that time. An edit changes the time
only when the text changes. The timestamp stays outside the displayed message.
Legacy queue rows without a recorded acceptance time use the native dispatch
reminder. The application does not infer receipt time from queue priority.
A queued message has its original acceptance time plus a native current-time
reminder when the model receives it. These times can differ.
Managed tool results append a separate timestamp text item, so the original JSON
result stays valid. Both successful and failed results retain their timestamp
across retries and restart.

## Cache behavior

Clock values enter only newly appended input. Previous messages, timestamps,
instructions, and tool schemas stay unchanged. Native reminders become durable
history entries rather than a changing system instruction. Resume reads those
entries from disk. Normal Codex compaction can replace older history; this
application does not change that behavior.

The local provider test compares complete earlier input prefixes across six
requests, including a fresh process and thread resume. It also compares request
instructions and advertised tools. These checks establish prefix stability.
They do not measure or guarantee OpenAI cache hits or billing.

## Evidence

- `python3 -B tests/time-awareness.py`: receipt, queue edit,
  reorder, steer, successful and failed result replay, restart, and clean UI text.
- `python3 -B tests/time-awareness-native.py`: installed Codex,
  local Responses server, successful command, exit code 7, invalid freeform
  patch, unknown tool, immutable request prefixes, and process restart.
- The native test uses an isolated Codex home and rejects external requests.
  It makes no paid model calls.
- Tool search and compaction behavior are supported by source inspection;
  this test does not execute those paths.

Source references:

- [Codex clock boundary and delivery](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/session/time_reminder.rs)
- [Clock insertion before inference](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/session/turn.rs)
- [Durable history records](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/session/mod.rs)
- [OpenAI prefix cache requirements](https://developers.openai.com/api/docs/guides/prompt-caching)

A `PostToolUse` hook is insufficient here. Codex 0.153.4 runs that hook only for
successful tool results. The native clock boundary does not have this limit.
