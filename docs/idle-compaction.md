# Claude idle compaction

Studio can compact an idle Claude conversation before its five-minute or one-hour prompt cache expires.
Codex agents do not use this feature.

Use **Settings > Agent orchestration > Claude idle compaction**:

- **Off** is the default.
- **Log only** records `would_compact` and sends no compaction request.
- **Compact** starts the existing native `/compact` turn.
- **Compact on return** prepares a separate summary. The next input selects that summary after the source checks pass.

The default context threshold is 150,000 tokens. The minimum is 60,000 tokens.
`CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS` overrides this threshold on the Rust server.
Invalid or non-finite values keep the configured threshold. Values below the minimum use 60,000.
The Claude settings `autoCompactEnabled: false` and `idleCompaction: false` also prevent compaction.

The bridge reports the last main request's usage, request identity, prompt prefix,
activity time, cache status, and TTL. A write with
`cache_creation.ephemeral_1h_input_tokens > 0` proves the one-hour TTL.
A five-minute write proves the shorter TTL. A mixed write uses the shorter TTL.
A request without cache writes keeps the preceding observed TTL, as Claude Code does.
An unknown TTL cannot arm the timer. Aggregate SDK result usage cannot prove the last request's TTL.
The SDK does not expose Claude Code's private cache clock or prefix snapshot.
Studio uses the SDK request status and time-to-first-token signals for the request time.
It checks the Studio prompt inputs and effective Claude settings for prefix changes.

Rust saves one deadline per session at:

```text
min(lastActivityAt + (expiresAt - lastActivityAt) * 0.9, unloadAt - 30 seconds)
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

## Studio sleep compaction and deferred replacement design

Studio extends the same policy to confirmed five-minute and one-hour caches.
The default threshold is 150,000 tokens, with a 60,000-token floor.
Existing explicit thresholds remain valid.
The account worker publishes its actual idle unload deadline. The timer selects
90 percent of the cache interval or 30 seconds before that deadline, whichever
comes first. An immediate worker unload has no advance interval. It first lets
the same durable compaction admission settle. Queued input and active work retain
priority. Studio does not use the remote-host environment gate.

### Compact on return

The additional **Compact on return** mode uses a separate native session:

1. Reserve the ordinary idle action with its existing request identity.
2. Read the main native history and fork it with the public SDK `forkSession` API.
3. Verify the fork preserves each source message and its delivery flags.
   The SDK remaps UUIDs and changes the last assistant timestamp. These known
   fork changes are excluded from the copy proof. The source fingerprint includes them.
4. Send `/compact` to a separate SDK query that resumes the fork. Keep the main
   session and Studio transcript unchanged. Do not send user input to this query.
5. Save a ready receipt only after a successful result and a compact boundary.
   The receipt includes the source native ID, source history fingerprint, prompt
   prefix, request identity, target native ID, and compact metadata.
6. On the next ordinary input, check those source values again. Discard a stale
   receipt. For a valid receipt, persist the native ID change before sending input.
   Retain the former native ID as a history branch. Continue through the ordinary
   input receipt so retries cannot send the input twice.
7. Before the return input is compiled, use the durable ready notification to
   capture the current B1-6 team state. Include its roster block in that same
   input under the existing 24,000-byte Studio budget and 8,000-byte roster share.
   The writer guard checks this projection again. The bridge refuses replacement
   unless this preparation receipt is present. A lost ready notification keeps
   the original history.
8. Publish the compaction boundary and its notice when the replacement commits,
   never when the separate query prepares the summary.

The bridge journal owns the ready receipt and native ID change. Rust owns the
idle deadline, mode, admission, and fire receipt. A process failure before the
ready receipt leaves the main history intact. An uncertain preparation is not
repeated. A process failure after the native ID change resumes the selected
native session. The stored summary remains private to its Studio thread.

A new turn, changed settings or prompt, rollback, different native history, or
unsettled input invalidates the prepared receipt. A failed fork, summary, write,
or proof keeps the original native ID. The setting and transcript notice state
that preparation keeps history until the next input. Tests cover actual bridge calls, restart, changed source, failed preparation,
and duplicate input. An additional test uses the installed SDK fork and history APIs
with an isolated memory store. No test sends a live model request.

This design uses public SDK fork and query APIs. It does not edit native JSONL
history or use the unavailable private precompute/swap methods. Cache reuse is
subject to the provider accepting the same prompt prefix for the forked query.

The deferred replacement notice is:

> Compacted on return with a summary prepared while idle.

The separate summary query has a ten-minute deadline. Its account check precedes
input admission. It cannot use tools. The previous native history remains in
`historyBranches`; Studio never rewrites native transcript files.

Behavior sources: Claude Code 2.1.302 `chunk-m1vqcgc7.js:148-256` for sleep
compaction and `chunk-hqtnbrjd.js:71095-71212` for deferred compaction.
