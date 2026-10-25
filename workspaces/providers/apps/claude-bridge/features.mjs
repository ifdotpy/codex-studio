import usagePolicyJson from "./usage-policy.mjs";
const usagePolicy = JSON.parse(usagePolicyJson);

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
  const all = (result.rateLimitsByLimitId ||= {});
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
    const id = names.length === 1 ? names[0] : "claude-unknown-limit";
    bucket = all[id] ||= { limitId: id, limitName: "Claude · Unknown limit" };
  } else if (["seven_day_opus", "seven_day_sonnet"].includes(type)) {
    const name = type.slice(10),
      id = usagePolicy.modelBuckets[name];
    bucket = all[id] ||= { limitId: id, limitName: "Claude · " + name };
  } else if (!["five_hour", "seven_day"].includes(type)) {
    if (!info.status && !info.overageStatus) return null;
    const id = type === "overage" ? "claude-overage" : "claude-unknown-limit";
    bucket = all[id] ||= {
      limitId: id,
      limitName: "Claude · " + (type || "Unknown limit"),
    };
  }
  // Native events can grant overage without reporting utilization.
  const hasStatus =
    typeof info.status === "string" ||
    typeof info.overageStatus === "string" ||
    typeof info.isUsingOverage === "boolean" ||
    typeof info.overageInUse === "boolean";
  if (!Number.isFinite(info.utilization) && !hasStatus) return null;
  bucket[key] = {
    ...bucket[key],
    ...(Number.isFinite(info.utilization)
      ? { usedPercent: Math.max(0, Math.min(100, info.utilization * 100)) }
      : {}),
    windowDurationMins: type === "five_hour" ? 300 : 10080,
    ...(Number.isFinite(info.resetsAt) ? { resetsAt: info.resetsAt } : {}),
  };
  // Replace permission on each event; a later rejection revokes it.
  bucket.claudeRateLimit = structuredClone(info);
  result.claudeRateLimit = structuredClone(info);
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
export class InputQueue {
  values = [];
  nonQueryIds = new Set();
  pending = [];
  // Stop removes every SDK message that has not crossed the iterator boundary.
  withdraw() {
    const messages = this.pending.splice(0);
    for (const value of this.values.splice(0))
      messages.push(...(value.studioInputBatch || [value]));
    return messages;
  }
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
      if (this.values.length) {
        const value = this.values.shift();
        this.pending = [...(value.studioInputBatch || [value])];
        while (this.pending.length && !this.closed) {
          const message = this.pending.shift();
          if (message.shouldQuery === false) this.nonQueryIds.add(message.uuid);
          yield message;
        }
      }
    }
  }
}

// This is permission from a native rate-limit event, not the billing preference.
export function overageAllowed(info) {
  if (info.overageStatus === "rejected") return false;
  return (
    usagePolicy.allowedOverageStatuses.includes(info.overageStatus) ||
    info.isUsingOverage === true ||
    info.overageInUse === true
  );
}

// Keep API failure classification at both assistant and terminal boundaries.
export function claudeErrorInfo(error, text) {
  if (error === "rate_limit") return "rateLimitExceeded";
  if (error === "overloaded") return "serverOverloaded";
  if (error === "server_error") return "internalServerError";
  if (error === "authentication_failed") return "unauthorized";
  if (
    (!error || error === "unknown") &&
    /(?:^|\b(?:http(?: error)?|api(?: error)?|status(?: code)?)[ :]+)429\b/i.test(
      text || "",
    )
  )
    return "rateLimitExceeded";
  if (
    error === "unknown" &&
    /can't reach the api server|ENOTFOUND|ECONNRESET|ECONNREFUSED|ETIMEDOUT|EAI_AGAIN|connection error|socket hang up|fetch failed/i.test(
      text || "",
    )
  )
    return "httpConnectionFailed";
  return null;
}

export function providerNotice(message) {
  if (message.subtype === "api_retry") {
    const status =
      message.error_status == null
        ? "no HTTP response"
        : "HTTP " + message.error_status;
    return `Claude API retry ${message.attempt} of ${message.max_retries}. Delay: ${message.retry_delay_ms} ms. Status: ${status}.`;
  }
  if (["model_fallback", "model_consent_fallback"].includes(message.subtype)) {
    const original =
      message.original_model || message.originalModel || "unknown";
    const fallback =
      message.fallback_model || message.fallbackModel || "unknown";
    return `Claude model fallback: ${original} to ${fallback}. ${message.content || message.text || ""}`.trim();
  }
  return null;
}

export function modelFamily(model) {
  if (typeof model !== "string") return null;
  return (
    Object.keys(usagePolicy.modelBuckets).find(
      (family) =>
        model === family ||
        model === "claude-" + family ||
        model.startsWith("claude-" + family + "-"),
    ) || null
  );
}
export function limitApplies(info, model) {
  const family = modelFamily(model);
  const scoped = Object.keys(usagePolicy.modelBuckets).find(
    (name) => info.rateLimitType === "seven_day_" + name,
  );
  return !family || !scoped || family === scoped;
}
