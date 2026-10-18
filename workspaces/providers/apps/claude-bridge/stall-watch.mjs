// Studio's watchdog for a Claude tool call that never returns.
//
// Claude Code bounds the work inside a tool (Bash moves a command to the
// background at its timeout), but nothing bounds the time between a tool_use
// and its tool_result when the CLI process itself stops progressing. Codex runs
// its tools in-process; this bridge sees the CLI only through its stream, so it
// owns this limit. Waits that Studio owns (approvals, questions, Studio tool
// calls) are never stalls: the bridge tracks them and pauses the clock.

const positive = (value) => {
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? number : null;
};

// Claude Code's own Bash limits, in milliseconds: default 120000, ceiling
// 600000, both overridable with the variables the CLI reads from the
// environment this bridge passes to it (code.claude.com/docs/en/tools-reference,
// "Timeout and output limits").
const CLAUDE_BASH_DEFAULT_TIMEOUT_MS = 120_000;
const CLAUDE_BASH_MAX_TIMEOUT_MS = 600_000;

export function bashTimeoutMs(input, env = process.env) {
  const fallback =
    positive(env.BASH_DEFAULT_TIMEOUT_MS) ?? CLAUDE_BASH_DEFAULT_TIMEOUT_MS;
  const ceiling = Math.max(
    positive(env.BASH_MAX_TIMEOUT_MS) ?? CLAUDE_BASH_MAX_TIMEOUT_MS,
    fallback,
  );
  return Math.min(positive(input?.timeout) ?? fallback, ceiling);
}

// The time limit a tool declares for itself, in milliseconds, or null.
export function declaredTimeoutMs(block, env = process.env) {
  return block?.name === "Bash" ? bashTimeoutMs(block.input, env) : null;
}

// Watchdog limits, in milliseconds. The STUDIO_CLAUDE_* variables exist for
// tests and for operators; production uses the defaults.
export function stallLimits(env = process.env) {
  return {
    // Added to a declared tool timeout. The CLI must also have sent no event
    // at all for this long, so hooks and progress keep a live call alive.
    graceMs: positive(env.STUDIO_CLAUDE_TOOL_STALL_GRACE_MS) ?? 120_000,
    // A tool without a declared timeout stalls after this much complete
    // silence. It equals Claude Code's idle abort for a stdio MCP call
    // (code.claude.com/docs/en/env-vars, CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT).
    silenceMs: positive(env.STUDIO_CLAUDE_TOOL_SILENCE_MS) ?? 1_800_000,
    // How long the CLI has to end the turn after an interrupt request before
    // the bridge closes the process.
    interruptWaitMs:
      positive(env.STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS) ?? 15_000,
    // How long a closed CLI can live: the SDK closes stdin, sends SIGTERM
    // after 2 s and SIGKILL 5 s later (ProcessTransport.close in the SDK).
    // Its native history is final only after this.
    exitWaitMs: positive(env.STUDIO_CLAUDE_STALL_EXIT_WAIT_MS) ?? 10_000,
    // How long an approval that Claude Code asks for in bypassPermissions
    // mode waits before Studio declines it. Claude Code resolves a permission
    // dialog parked at a remote client to its safe no-action default after
    // 5 minutes by default, 10 at most when finite (SDK option dialogExpiry).
    unexpectedApprovalMs:
      positive(env.STUDIO_CLAUDE_UNEXPECTED_APPROVAL_WAIT_MS) ?? 600_000,
  };
}

// The first stalled tool clock, or null. `clocks` maps a tool_use id to
// { startedAt, timeoutMs }; `lastEventAt` is the time of the newest message
// from the CLI. All times are epoch milliseconds.
export function findStall(clocks, now, lastEventAt, limits) {
  const silentMs = Math.max(0, now - lastEventAt);
  for (const [id, clock] of clocks) {
    const elapsedMs = Math.max(0, now - clock.startedAt);
    const stalled =
      clock.timeoutMs == null
        ? silentMs >= limits.silenceMs
        : elapsedMs >= clock.timeoutMs + limits.graceMs &&
          silentMs >= limits.graceMs;
    if (stalled) return { id, ...clock, elapsedMs, silentMs };
  }
  return null;
}

const count = (value, unit) => value + " " + unit + (value === 1 ? "" : "s");
// A duration in milliseconds as plain text, for notices.
export const durationText = (ms) =>
  ms >= 60_000
    ? count(Math.round(ms / 60_000), "minute")
    : count(Math.max(1, Math.round(ms / 1000)), "second");

export function stallMessage(stall) {
  const limit =
    stall.timeoutMs == null
      ? "Claude Code sent no event for " + durationText(stall.silentMs)
      : "its timeout is " +
        durationText(stall.timeoutMs) +
        ", and Claude Code sent no event for " +
        durationText(stall.silentMs);
  return (
    "Studio stopped this turn: the " +
    (stall.tool || "tool") +
    " call did not return after " +
    durationText(stall.elapsedMs) +
    " (" +
    limit +
    "). Messages sent during the turn go to the next turn."
  );
}

// Native input ids that Claude Code wrote into its conversation history: user
// messages, and queued messages it absorbed into a turn. `lines` is the
// session's JSONL transcript.
export function recordedInputIds(lines) {
  const ids = new Set();
  for (const line of lines) {
    let entry;
    try {
      entry = JSON.parse(line);
    } catch {
      continue;
    }
    if (entry?.type === "user" && typeof entry.uuid === "string")
      ids.add(entry.uuid);
    const attachment = entry?.attachment;
    if (
      entry?.type === "attachment" &&
      attachment?.type === "queued_command" &&
      typeof attachment.source_uuid === "string"
    )
      ids.add(attachment.source_uuid);
  }
  return ids;
}
