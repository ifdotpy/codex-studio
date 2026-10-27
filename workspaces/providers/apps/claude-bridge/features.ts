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
  BridgeRateBucket as LimitBucket,
  BridgeRateLimits,
} from "./bridge-protocol.ts";
type NativeLimits = NonNullable<SDKControlGetUsageResponse["rate_limits"]>;
type NativeWindow =
  | NonNullable<
      NativeLimits[Exclude<keyof NativeLimits, "model_scoped" | "extra_usage">]
    >
  | NonNullable<NativeLimits["model_scoped"]>[number];
type UsageResponse = Pick<
  SDKControlGetUsageResponse,
  "rate_limits_available" | "rate_limits" | "subscription_type"
>;
import {
  parseRateLimitInfo,
  rateWindowMinutes,
  usagePolicy,
  rateLimitFamily,
} from "./rate-limit-input.ts";
import type { RateLimitInfo } from "./rate-limit-input.ts";
export type { RateLimitInfo } from "./rate-limit-input.ts";
export {
  overageAllowed,
  parseRateLimitInfo,
  rateLimitRejected,
} from "./rate-limit-input.ts";
export type Limits = Partial<BridgeRateLimits>;
export type QueuedInput<T extends SDKUserMessage = SDKUserMessage> = T & {
  studioInputBatch?: QueuedInput<T>[];
};
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
      primary: window(raw.five_hour, rateWindowMinutes.primary),
      secondary: window(raw.seven_day, rateWindowMinutes.secondary),
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
      secondary: window(row, rateWindowMinutes.secondary),
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
  evidence: ReturnType<typeof parseRateLimitInfo>,
): BridgeRateLimits {
  const all = structuredClone(current?.rateLimitsByLimitId ?? {});
  const main = all.claude ?? { limitId: "claude", limitName: "Claude" };
  all.claude = main;
  const result: BridgeRateLimits = {
    ...current,
    rateLimits: main,
    rateLimitsByLimitId: all,
    claudeRateLimit: evidence.type === "known" ? evidence.raw : evidence,
  };
  if (evidence.type === "unsupported") return result;
  const { info, scope } = evidence;
  const bucket = (all[scope.id] ??= {
    limitId: scope.id,
    limitName: scope.label,
  });
  bucket[scope.window] = {
    ...bucket[scope.window],
    ...(info.utilization === undefined
      ? {}
      : { usedPercent: Math.max(0, Math.min(100, info.utilization * 100)) }),
    windowDurationMins: rateWindowMinutes[scope.window],
    ...(info.resetsAt === undefined ? {} : { resetsAt: info.resetsAt }),
  };
  bucket.claudeRateLimit = evidence.raw;
  return result;
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
export function limitApplies(info: RateLimitInfo, model: unknown) {
  const family = modelFamily(model);
  const scoped = rateLimitFamily(info);
  return !family || !scoped || family === scoped;
}
