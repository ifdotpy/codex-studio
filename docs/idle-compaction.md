# Claude idle compaction

Studio can compact an idle Claude conversation before its one-hour prompt cache expires.
Codex agents do not use this feature.

Use **Settings > Agent orchestration > Claude idle compaction**:

- **Off** is the default.
- **Log only** records `would_compact` and sends no compaction request.
- **Compact** starts the existing native `/compact` turn.

The default context threshold is 200,000 tokens. The minimum is 100,000 tokens.
`CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS` overrides this threshold on the Rust server.
Invalid or non-finite values keep the configured threshold. Values below the minimum use 100,000.
The Claude settings `autoCompactEnabled: false` and `idleCompaction: false` also prevent compaction.

The bridge reports the last main request's usage, request identity, prompt prefix,
activity time, cache status, and TTL. A write with
`cache_creation.ephemeral_1h_input_tokens > 0` proves the one-hour TTL.
A request without cache writes keeps the preceding observed TTL, as Claude Code does.
An unknown TTL cannot arm the timer. Aggregate SDK result usage cannot prove the last request's TTL.
The SDK does not expose Claude Code's private cache clock or prefix snapshot.
Studio uses the SDK request status and time-to-first-token signals for the request time.
It checks the Studio prompt inputs and effective Claude settings for prefix changes.

Rust saves one deadline per session at:

```text
lastActivityAt + (expiresAt - lastActivityAt) * 0.9
```

The policy clamps a finite fire fraction to 0.5 through 0.95. A non-finite fraction uses 0.9.
The fire check refuses a disabled setting, a changed prefix, a different TTL,
a cold cache, a timer over 60 seconds late, a small context, a newer request,
or account limits that do not permit use. Studio also refuses active, queued, and stopped agents.
The existing live-limit policy checks the account's model windows and observation age.
A native Claude limit status other than `allowed` prevents compaction.

Each episode has a stable request identity. The fire receipt and native admission commit together.
The native action owner commits an uncertain receipt before the provider write.
A server or bridge restart does not repeat a hand-off.
A final writer check catches input that arrives during preparation.
The bridge checks the effective Claude settings, prompt prefix, and request identity before `/compact`.
Studio disables the CLI's private idle timer so there is only one timer owner.

The transcript notice appears only after Claude reports the completed compact boundary:

> Compacted while idle, before the prompt cache expired.

The notice includes the setting path to turn it off. The existing compaction
notification also advances the compaction epoch. The next ordinary turn receives
the current Studio team roster through the existing B1-6 context path.

## Why `compact_on_return` is unavailable

The installed `@anthropic-ai/claude-agent-sdk` version is 0.3.308.
Its `Query` interface has no method to precompute a compaction summary and later
swap that summary into the current session. `SDKCompactBoundaryMessage` reports
a completed compaction. It cannot defer the replacement of the conversation.
`forkSession` creates another session. It cannot replace the current session's
history with a precomputed summary. Studio therefore does not expose this mode.

The behavior reference is Claude Code 2.1.302, `X2t`, `k1o`, and `Z2t` in
`readable/chunk-hqtnbrjd.js`, and the `idleCompaction` setting in `chunk-jfv2evcj.js`.
