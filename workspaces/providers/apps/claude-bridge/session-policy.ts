import { MCP_AUTO_BACKGROUND_MS } from "./session-policy.generated.ts";

// Claude 2.1.302: chunk-0hrc1z2k oe. Explicit duration avoids a rollout
// overriding Studio's visible setting. Native tool eligibility still applies.
export function mcpBackgroundEnvironment(session: {
  claude?: { mcpAutoBackground?: boolean | undefined } | undefined;
}): Record<string, string> {
  return {
    CLAUDE_AUTO_BACKGROUND_TASKS:
      session.claude?.mcpAutoBackground === false ? "false" : "true",
    CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS:
      session.claude?.mcpAutoBackground === false
        ? "0"
        : String(MCP_AUTO_BACKGROUND_MS),
  };
}

export type PromptCacheTtl = "5m" | "1h" | "automatic";

const ttl = (value: unknown): "5m" | "1h" | undefined =>
  value === "5m" || value === "1h" ? value : undefined;
const enabled = (value: unknown) =>
  ["1", "true", "yes", "on"].includes(String(value).toLowerCase().trim());

// Claude 2.1.302: chunk-hqtnbrjd _tn. A Studio worker is a separate SDK
// main query, so it takes the native subagent cache policy.
export function promptCacheTtl(
  session: { managedWorker?: boolean | undefined },
  effective: { promptCacheTtl?: unknown; subagentPromptCacheTtl?: unknown },
  env: NodeJS.ProcessEnv = process.env,
): PromptCacheTtl {
  if (
    enabled(env.FORCE_PROMPT_CACHING_5M) ||
    env.CLAUDE_CODE_MESSAGES_PROMPT_CACHE_TTL === "5m"
  )
    return "5m";
  if (session.managedWorker === true)
    return (
      ttl(env.CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL) ||
      ttl(effective.subagentPromptCacheTtl) ||
      (enabled(env.ENABLE_PROMPT_CACHING_1H) ||
      (enabled(env.CLAUDE_CODE_USE_BEDROCK) &&
        enabled(env.ENABLE_PROMPT_CACHING_1H_BEDROCK))
        ? "1h"
        : "5m")
    );
  return (
    ttl(env.CLAUDE_CODE_PROMPT_CACHE_TTL) ||
    ttl(effective.promptCacheTtl) ||
    "automatic"
  );
}

// A lead keeps its main cache policy. A worker's main query receives the
// subagent policy as its main-query TTL environment.
export function sessionCachePolicy(
  session: { managedWorker?: boolean | undefined },
  effective: Record<string, unknown>,
  env: NodeJS.ProcessEnv = process.env,
): {
  effective: Record<string, unknown>;
  ttl: PromptCacheTtl;
  env: Record<string, string>;
} {
  const value = promptCacheTtl(session, effective, env);
  if (session.managedWorker !== true)
    return { effective, ttl: value, env: {} };
  return {
    effective: { ...effective, promptCacheTtl: value },
    ttl: value,
    env: { CLAUDE_CODE_PROMPT_CACHE_TTL: value },
  };
}
