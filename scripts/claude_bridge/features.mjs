// Native Claude values normalized to Studio's existing public protocol.
export function usageLimits(response) {
  if (!response.rate_limits_available || !response.rate_limits)
    throw new Error(
      "Claude did not return subscription limits for this account",
    );
  const raw = response.rate_limits;
  const window = (value, minutes) =>
    value && {
      usedPercent: Number.isFinite(value.utilization)
        ? Math.max(0, Math.min(100, value.utilization))
        : null,
      resetsAt:
        value.resets_at && Number.isFinite(Date.parse(value.resets_at))
          ? Date.parse(value.resets_at) / 1000
          : null,
      windowDurationMins: minutes,
    };
  const buckets = {
    claude: {
      limitId: "claude",
      limitName: "Claude",
      primary: window(raw.five_hour, 300),
      secondary: window(raw.seven_day, 10080),
    },
  };
  const scoped = [...(raw.model_scoped || [])];
  for (const [field, label] of [
    ["seven_day_opus", "Opus"],
    ["seven_day_sonnet", "Sonnet"],
    ["seven_day_oauth_apps", "OAuth apps"],
  ])
    if (raw[field] && !scoped.some((row) => row.display_name === label))
      scoped.push({ ...raw[field], display_name: label });
  for (const row of scoped) {
    const id =
      "claude-" +
      String(row.display_name)
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, "-");
    buckets[id] = {
      limitId: id,
      limitName: "Claude · " + row.display_name,
      secondary: window(row, 10080),
    };
  }
  return {
    rateLimits: buckets.claude,
    rateLimitsByLimitId: buckets,
    subscriptionType: response.subscription_type,
    extraUsage: raw.extra_usage || null,
  };
}
export function limitEvent(current, info) {
  const result = structuredClone(
    current || {
      rateLimitsByLimitId: {
        claude: { limitId: "claude", limitName: "Claude" },
      },
    },
  );
  const all = result.rateLimitsByLimitId;
  const main = (all.claude ||= { limitId: "claude", limitName: "Claude" });
  const type = info.rateLimitType;
  let bucket = main,
    key = type === "five_hour" ? "primary" : "secondary";
  if (type === "seven_day_overage_included") {
    const names = Object.keys(all).filter(
      (id) =>
        id !== "claude" &&
        !["claude-opus", "claude-sonnet", "claude-oauth-apps"].includes(id),
    );
    if (names.length !== 1) return null;
    bucket = all[names[0]];
  } else if (["seven_day_opus", "seven_day_sonnet"].includes(type)) {
    const name = type.slice(10),
      id = "claude-" + name;
    bucket = all[id] ||= { limitId: id, limitName: "Claude · " + name };
  } else if (!["five_hour", "seven_day"].includes(type)) return null;
  if (!Number.isFinite(info.utilization)) return null;
  bucket[key] = {
    ...bucket[key],
    usedPercent: Math.max(0, Math.min(100, info.utilization * 100)),
    windowDurationMins: type === "five_hour" ? 300 : 10080,
    ...(Number.isFinite(info.resetsAt) ? { resetsAt: info.resetsAt } : {}),
  };
  result.rateLimits = main;
  return result;
}
export function nativeItem(block, cwd) {
  const base = {
    id: block.id,
    status: "inProgress",
    tool: block.name,
    arguments: block.input,
  };
  if (block.name === "Bash")
    return {
      ...base,
      type: "commandExecution",
      command: block.input.command,
      cwd,
      aggregatedOutput: "",
    };
  if (["Edit", "Write", "NotebookEdit"].includes(block.name))
    return {
      ...base,
      type: "fileChange",
      changes: [
        {
          path: block.input.file_path || block.input.notebook_path,
          kind: { type: block.name === "Write" ? "add" : "update" },
          diff:
            block.input.content ||
            "-" +
              (block.input.old_string || "") +
              "\n+" +
              (block.input.new_string || ""),
        },
      ],
    };
  return { ...base, type: "mcpToolCall", server: "claude" };
}
export function usageTokens(usage) {
  if (!usage) return null;
  const n = (value) => (Number.isFinite(value) && value >= 0 ? value : 0);
  const input =
    n(usage.input_tokens) +
    n(usage.cache_read_input_tokens) +
    n(usage.cache_creation_input_tokens);
  return {
    inputTokens: input,
    outputTokens: n(usage.output_tokens),
    cachedInputTokens: n(usage.cache_read_input_tokens),
    totalTokens: input + n(usage.output_tokens),
  };
}
export class InputQueue {
  values = [];
  wake = null;
  closed = false;
  push(value) {
    if (this.closed) throw new Error("Claude input closed");
    this.values.push(value);
    this.wake?.();
    this.wake = null;
  }
  close() {
    this.closed = true;
    this.wake?.();
    this.wake = null;
  }
  async *[Symbol.asyncIterator]() {
    while (!this.closed) {
      if (!this.values.length)
        await new Promise((resolve) => (this.wake = resolve));
      if (this.closed) break;
      if (this.values.length) yield this.values.shift();
    }
  }
}
