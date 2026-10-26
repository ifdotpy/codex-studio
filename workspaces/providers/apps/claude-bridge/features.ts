import type {
  SDKControlGetUsageResponse,
  SDKAssistantMessage,
  SDKUserMessage,
} from "@anthropic-ai/claude-agent-sdk";
import type { KnownCodexErrorKind } from "./provider-errors.generated.ts";
import type { FreshToolItem } from "./session-state.ts";
type NativeToolBlock = Extract<
  SDKAssistantMessage["message"]["content"][number],
  { type: "tool_use" }
>;
import type {
  BridgeRateLimitInfo,
  BridgeRateBucket as LimitBucket,
  BridgeRateLimits,
} from "./bridge-protocol.ts";
type NativeLimits = NonNullable<SDKControlGetUsageResponse["rate_limits"]>;
type NativeWindow = Partial<NonNullable<NativeLimits["five_hour"]>>;
type UsageResponse = Partial<
  Omit<SDKControlGetUsageResponse, "rate_limits">
> & {
  rate_limits?:
    | {
        [K in keyof NativeLimits]?: K extends "model_scoped"
          ? (NativeWindow &
              Pick<
                NonNullable<NativeLimits["model_scoped"]>[number],
                "display_name"
              >)[]
          : K extends "extra_usage"
            ? Partial<NonNullable<NativeLimits[K]>> | null
            : NativeWindow | null;
      }
    | null;
};
export type RateLimitInfo = BridgeRateLimitInfo;
export type Limits = Partial<BridgeRateLimits>;
export type QueuedInput<T extends SDKUserMessage = SDKUserMessage> = T & {
  studioInputBatch?: QueuedInput<T>[];
};
import usagePolicyJson from "./usage-policy.ts";
const usagePolicy: {
  modelBuckets: Record<string, string>;
  allowedOverageStatuses: string[];
} = JSON.parse(usagePolicyJson);

// Native Claude values normalized to Studio's existing public protocol.
export function usageLimits(response: UsageResponse): BridgeRateLimits {
  if (!response.rate_limits_available || !response.rate_limits)
    throw new Error(
      "Claude did not return subscription limits for this account",
    );
  const raw = response.rate_limits;
  const window = (value: NativeWindow | null | undefined, minutes: number) =>
    value && {
      usedPercent: Number.isFinite(value.utilization)
        ? Math.max(0, Math.min(100, value.utilization!))
        : null,
      resetsAt:
        value.resets_at && Number.isFinite(Date.parse(value.resets_at))
          ? Date.parse(value.resets_at) / 1000
          : null,
      windowDurationMins: minutes,
    };
  const buckets: Record<string, LimitBucket> & { claude: LimitBucket } = {
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
  ] as const)
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
export function limitEvent(
  current: Limits | null | undefined,
  info: RateLimitInfo,
) {
  const result: Limits = structuredClone(
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
    key: "primary" | "secondary" =
      type === "five_hour" ? "primary" : "secondary";
  if (type === "seven_day_overage_included") {
    const names = Object.keys(all).filter(
      (id) =>
        id !== "claude" &&
        !["claude-opus", "claude-sonnet", "claude-oauth-apps"].includes(id),
    );
    const id = names.length === 1 ? names[0]! : "claude-unknown-limit";
    bucket = all[id] ||= { limitId: id, limitName: "Claude · Unknown limit" };
  } else if (type === "seven_day_opus" || type === "seven_day_sonnet") {
    const name = type === "seven_day_opus" ? "opus" : "sonnet",
      id = usagePolicy.modelBuckets[name] ?? "";
    bucket = all[id] ||= { limitId: id, limitName: "Claude · " + name };
  } else if (!["five_hour", "seven_day"].includes(type as string)) {
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
      ? {
          usedPercent: Math.max(0, Math.min(100, info.utilization! * 100)),
        }
      : {}),
    windowDurationMins: type === "five_hour" ? 300 : 10080,
    ...(Number.isFinite(info.resetsAt) ? { resetsAt: info.resetsAt } : {}),
  };
  // Replace permission on each event; a later rejection revokes it.
  bucket.claudeRateLimit = structuredClone(info);
  result.claudeRateLimit = structuredClone(info);
  return Object.assign(result, { rateLimits: main, rateLimitsByLimitId: all });
}
export function nativeItem(block: NativeToolBlock, cwd: string): FreshToolItem {
  const input = block.input;
  const isInputRecord = (value: unknown): value is Record<string, unknown> =>
    value !== null && typeof value === "object" && !Array.isArray(value);
  if (!isInputRecord(input)) throw new Error("Invalid Claude tool input");
  const base = {
    id: block.id,
    status: "inProgress" as const,
    tool: block.name,
    arguments: input,
  };
  if (block.name === "Bash")
    return {
      ...base,
      type: "commandExecution" as const,
      command: input.command,
      cwd,
      aggregatedOutput: "",
    };
  if (["Edit", "Write", "NotebookEdit"].includes(block.name))
    return {
      ...base,
      type: "fileChange" as const,
      changes: [
        {
          path: input.file_path || input.notebook_path,
          kind: { type: block.name === "Write" ? "add" : "update" },
          diff:
            input.content ||
            "-" + (input.old_string || "") + "\n+" + (input.new_string || ""),
        },
      ],
    };
  return { ...base, type: "mcpToolCall" as const, server: "claude" };
}
export class InputQueue<
  T extends { uuid?: string; shouldQuery?: boolean } | string = QueuedInput,
> {
  values: (T & { studioInputBatch?: T[] })[] = [];
  nonQueryIds = new Set<string | undefined>();
  pending: (T & { uuid?: string; shouldQuery?: boolean })[] = [];
  // Stop removes every SDK message that has not crossed the iterator boundary.
  withdraw() {
    const messages = this.pending.splice(0);
    for (const value of this.values.splice(0))
      messages.push(...(value.studioInputBatch || [value]));
    return messages;
  }
  wake: (() => void) | null = null;
  closed = false;
  push(value: T & { studioInputBatch?: T[] }) {
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
        await new Promise<void>((resolve) => (this.wake = resolve));
      if (this.closed) break;
      if (this.values.length) {
        const value = this.values.shift()!;
        this.pending = [...(value.studioInputBatch || [value])];
        while (this.pending.length && !this.closed) {
          const message = this.pending.shift()!;
          if (message.shouldQuery === false) this.nonQueryIds.add(message.uuid);
          yield message;
        }
      }
    }
  }
}

// This is permission from a native rate-limit event, not the billing preference.
export function overageAllowed(info: RateLimitInfo) {
  if (info.overageStatus === "rejected") return false;
  return (
    usagePolicy.allowedOverageStatuses.includes(info.overageStatus as string) ||
    info.isUsingOverage === true ||
    info.overageInUse === true
  );
}

// The SDK owns this closed input domain; new SDK variants require a decision.
const assistantErrorKinds = {
  authentication_failed: "unauthorized",
  oauth_org_not_allowed: null,
  account_on_hold: null,
  verification_required: null,
  billing_error: null,
  rate_limit: "rateLimitExceeded",
  overloaded: "serverOverloaded",
  invalid_request: null,
  model_not_found: null,
  server_error: "internalServerError",
  unknown: null,
  max_output_tokens: null,
  cloud_credential_error: null,
} satisfies Record<
  NonNullable<SDKAssistantMessage["error"]>,
  KnownCodexErrorKind | null
>;

// Keep API failure classification at the SDK assistant boundary.
export function claudeErrorInfo(
  error: SDKAssistantMessage["error"],
  text: string | null | undefined,
): KnownCodexErrorKind | null {
  // The SDK yields parsed JSON without validating this declared union. Check
  // primitive own tags before indexing so arrays and prototype keys stay raw.
  if (error !== undefined && !isAssistantError(error)) return null;
  if (error && error !== "unknown") return assistantErrorKinds[error];
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

function isAssistantError(
  value: unknown,
): value is NonNullable<SDKAssistantMessage["error"]> {
  return typeof value === "string" && Object.hasOwn(assistantErrorKinds, value);
}

// Terminal exceptions and legacy callers are untyped external ingress. A future
// vendor tag stays unclassified; its original exception belongs to the caller.
export function legacyClaudeErrorInfo(
  error: unknown,
  text: string | null | undefined,
): KnownCodexErrorKind | null {
  if (error == null || isAssistantError(error))
    return claudeErrorInfo(error ?? undefined, text);
  return null;
}

export function providerNotice(message: Record<string, unknown>) {
  if (message.subtype === "api_retry") {
    const status =
      message.error_status == null
        ? "no HTTP response"
        : "HTTP " + message.error_status;
    return `Claude API retry ${message.attempt} of ${message.max_retries}. Delay: ${message.retry_delay_ms} ms. Status: ${status}.`;
  }
  if (
    ["model_fallback", "model_consent_fallback"].includes(
      message.subtype as string,
    )
  ) {
    const original =
      message.original_model || message.originalModel || "unknown";
    const fallback =
      message.fallback_model || message.fallbackModel || "unknown";
    return `Claude model fallback: ${original} to ${fallback}. ${message.content || message.text || ""}`.trim();
  }
  return null;
}

export function modelFamily(model: unknown) {
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
export function limitApplies(
  info: Pick<RateLimitInfo, "rateLimitType">,
  model: unknown,
) {
  const family = modelFamily(model);
  const scoped = Object.keys(usagePolicy.modelBuckets).find(
    (name) => info.rateLimitType === "seven_day_" + name,
  );
  return !family || !scoped || family === scoped;
}
