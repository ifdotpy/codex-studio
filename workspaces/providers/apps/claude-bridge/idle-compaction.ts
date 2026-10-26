import type {
  SDKAssistantMessage,
  Settings,
} from "@anthropic-ai/claude-agent-sdk";
import type { CacheReport } from "./runtime-contract.generated.ts";
import type { Session } from "./session-state.ts";
import { object } from "./history-records.ts";
type PrefixSession = Partial<
  Pick<
    Session,
    | "cwd"
    | "model"
    | "approvalPolicy"
    | "sandbox"
    | "claude"
    | "developerInstructions"
    | "dynamicTools"
    | "capabilityProfile"
    | "moveProof"
    | "moveTurnOptions"
    | "idleCache"
  >
>;
type RequestUsage = Partial<
  Omit<SDKAssistantMessage["message"]["usage"], "cache_creation">
> & {
  cache_creation?: Partial<
    NonNullable<SDKAssistantMessage["message"]["usage"]["cache_creation"]>
  > | null;
};
import { createHash } from "node:crypto";

// Only prefix inputs, never usage counters, transcript length, or lifecycle state.
export function promptPrefix(s: PrefixSession, effective: Settings = {}) {
  return createHash("sha256")
    .update(
      JSON.stringify([
        s.cwd,
        s.model,
        s.approvalPolicy,
        s.sandbox,
        s.claude,
        s.developerInstructions,
        s.dynamicTools,
        s.capabilityProfile,
        object(s.moveProof) ? s.moveProof.options : undefined,
        s.moveTurnOptions,
        effective,
      ]),
    )
    .digest("hex");
}

export function cacheReport(
  s: PrefixSession,
  usage: RequestUsage,
  requestId: string,
  now = Date.now(),
  effective: Settings = {},
): CacheReport {
  const n = (value: number | null | undefined) =>
    typeof value === "number" && Number.isFinite(value) && value >= 0
      ? value
      : 0;
  const writes = n(usage.cache_creation_input_tokens);
  const reads = n(usage.cache_read_input_tokens);
  const creation = usage.cache_creation;
  // Claude's cache tracker inherits the previous TTL when no tokens are written.
  let ttl =
    writes === 0 && s.idleCache
      ? s.idleCache.ttl
      : n(creation?.ephemeral_5m_input_tokens) > 0
        ? "5m"
        : n(creation?.ephemeral_1h_input_tokens) > 0
          ? "1h"
          : "unknown";
  const configuredTtl =
    process.env.CLAUDE_CODE_PROMPT_CACHE_TTL || effective.promptCacheTtl;
  if (configuredTtl === "5m") ttl = "5m";
  return {
    requestId,
    ttl,
    warm: reads + writes > 0,
    lastActivityAt:
      s.idleCache?.requestId === requestId ? s.idleCache.lastActivityAt : now,
    contextTokens:
      n(usage.input_tokens) + reads + writes + n(usage.output_tokens),
    prefix: promptPrefix(s, effective),
    compactionEnabled: compactionEnabled(effective),
    idleCompactionEnabled: effective.idleCompaction !== false,
  };
}

export function idleRefusal(
  s: PrefixSession,
  active: unknown,
  expected: Pick<CacheReport, "prefix" | "requestId">,
  effective: Settings = {},
) {
  if (expected.prefix !== promptPrefix(s, effective)) return "prefix_changed";
  if (expected.requestId !== s.idleCache?.requestId) return "newer_request";
  if (
    object(active) &&
    (active.turn ||
      (object(active.tasks) && active.tasks.size) ||
      (object(active.input) &&
        Array.isArray(active.input.values) &&
        active.input.values.length))
  )
    return "active";
  return null;
}

export function compactionEnabled(
  effective: Settings,
  env: NodeJS.ProcessEnv = process.env,
) {
  const disabled = ["1", "true", "yes", "on"].includes(
    String(env.DISABLE_COMPACT || "")
      .toLowerCase()
      .trim(),
  );
  return (
    !disabled &&
    !env.DISABLE_AUTO_COMPACT &&
    effective.autoCompactEnabled !== false
  );
}
