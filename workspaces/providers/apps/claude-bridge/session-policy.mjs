import { MCP_AUTO_BACKGROUND_MS } from "./session-policy.generated.mjs";

// Claude 2.1.302: chunk-0hrc1z2k oe. Explicit duration avoids a rollout
// overriding Studio's visible setting. Native tool eligibility still applies.
export function mcpBackgroundEnvironment(session) {
  return {
    CLAUDE_AUTO_BACKGROUND_TASKS:
      session.claude?.mcpAutoBackground === false ? "false" : "true",
    CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS:
      session.claude?.mcpAutoBackground === false
        ? "0"
        : String(MCP_AUTO_BACKGROUND_MS),
  };
}

const ttl = (value) => (["5m", "1h"].includes(value) ? value : undefined);
const enabled = (value) =>
  ["1", "true", "yes", "on"].includes(String(value).toLowerCase().trim());

// Claude 2.1.302: chunk-hqtnbrjd _tn. A Studio worker is a separate SDK
// main query, so translate the native subagent override into its main env.
export function promptCacheTtl(session, effective, env = process.env) {
  if (
    enabled(env.FORCE_PROMPT_CACHING_5M) ||
    env.CLAUDE_CODE_MESSAGES_PROMPT_CACHE_TTL === "5m"
  )
    return "5m";
  if (session.managedWorker === true) {
    return (
      ttl(env.CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL) ||
      ttl(effective.subagentPromptCacheTtl) ||
      (enabled(env.ENABLE_PROMPT_CACHING_1H) ||
      (enabled(env.CLAUDE_CODE_USE_BEDROCK) &&
        enabled(env.ENABLE_PROMPT_CACHING_1H_BEDROCK))
        ? "1h"
        : "5m")
    );
  }
  return (
    ttl(env.CLAUDE_CODE_PROMPT_CACHE_TTL) ||
    ttl(effective.promptCacheTtl) ||
    "automatic"
  );
}

export async function sessionCachePolicy(session, options, resolveSettings) {
  const { effective: resolved } = await resolveSettings({
    cwd: session.cwd,
    settingSources: options.settingSources,
  });
  const effective = { ...resolved, ...session.moveProof?.options.settings };
  const value = promptCacheTtl(session, effective, options.env);
  return {
    effective:
      session.managedWorker === true
        ? { ...effective, promptCacheTtl: value }
        : effective,
    ttl: value,
    options:
      session.managedWorker === true
        ? {
            ...options,
            env: { ...options.env, CLAUDE_CODE_PROMPT_CACHE_TTL: value },
          }
        : options,
  };
}
