import { createHash } from "node:crypto";

// Only prefix inputs, never usage counters, transcript length, or lifecycle state.
export function promptPrefix(s, effective = {}) {
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
        s.moveProof?.options,
        s.moveTurnOptions,
        effective,
      ]),
    )
    .digest("hex");
}

export function cacheReport(
  s,
  usage,
  requestId,
  now = Date.now(),
  effective = {},
  env = process.env,
) {
  const n = (value) => (Number.isFinite(value) && value >= 0 ? value : 0);
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
    env.CLAUDE_CODE_PROMPT_CACHE_TTL || effective.promptCacheTtl;
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

export function idleRefusal(s, active, expected, effective = {}) {
  if (expected.prefix !== promptPrefix(s, effective)) return "prefix_changed";
  if (expected.requestId !== s.idleCache?.requestId) return "newer_request";
  if (
    active?.turn ||
    active?.tasks?.size ||
    active?.input?.values?.length ||
    active?.input?.pending?.length ||
    active?.reservingInput ||
    active?.pendingSteers?.size
  )
    return "active";
  return null;
}

export function compactionEnabled(effective, env = process.env) {
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
