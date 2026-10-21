# Monitor output and missed one-shot rules

`monitor_flood_and_missed_rules.rs` tests these Rust policies through decisions,
the output receipt adapter, SQLite transactions, restart, and the managed watch action.

## Monitor output

`src/monitor_matches.rs` owns the output wake limits and their units:

- `OUTPUT_WAKE_CAPACITY`: initial and maximum batch tokens per monitor.
- `OUTPUT_WAKE_REFILL_SECONDS`: time to replenish one token.
- `OUTPUT_WAKE_RECOVERY_SECONDS`: time without suppression that ends a flood episode.
- `OUTPUT_WAKE_FLOOD_SECONDS`: suppression duration before output wakes pause.
- `OUTPUT_EXCERPT_BYTES`: maximum excerpt bytes.
- `OUTPUT_CONTEXT_BYTES`: context bytes retained per stream for the next excerpt.

The bucket admits ten initial batches and replenishes one token every two seconds.
The existing one-second cooldown still combines matches into batches.
The first suppressed batch emits one notice outside the bucket.
Later admitted batches include the number of matches suppressed since the last notice.
Counts include overlapping matches and matches against different patterns on the same line.

Continued suppression for thirty seconds pauses output-match wakes.
The final flood notice includes the total suppression count and the required owner action.
The command continues. The log remains complete. The exit event remains active.
To resume output-match wakes, cancel the monitor and start a new monitor with more selective `output_patterns`.
This policy avoids terminating a command solely because its output is noisy.
The bucket and pause state survive restart. A quiet period cannot clear a flood pause.
The exit event includes the total suppression count, including matches after the pause.

Each new match payload includes the first matched line excerpt for that stream and pattern.
`matches[].logOffset` identifies the excerpt's first byte in the combined monitor log.
Per-byte context offsets preserve this position across chunk boundaries and interleaved streams.
An excerpt can be incomplete if the line exceeds the bound or the next chunk has not arrived.
Invalid UTF-8 bytes use replacement characters. The excerpt stays within its byte bound.

## One-shot rules

`src/rules_engine.rs::ONCE_RULE_GRACE_SECONDS` owns the automatic catch-up grace in seconds.
A one-shot rule up to sixty seconds late runs as before.
A later rule pauses and emits one `rule_missed` event with `rule`, `dueAt`, and `delaySeconds`.
The event and pause commit in the same transaction. The command does not start.
Ticks and restarts leave the rule paused without another missed event.

The owner can use `orchestration_watch` with `action=resume` to schedule a fresh run.
The existing resume action schedules that run after the rule's interval.
The owner can use `action=delete` to cancel the rule.
No new tool action or client control is required.
